package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"sort"
	"strings"
	"time"
)

// Ref change notification, caches following upstream refs, and read-only
// standbys.
//
// GET /v1/refs?wait=<etag> answers at once if refs differ from that etag,
// otherwise when they change (or after ~25 s with the same etag). Caches use
// it to keep a fresh copy of upstream refs; a standby (`serve --follow`) uses
// it to mirror another server.

const longPoll = 25 * time.Second

func refsETag(refs map[string]string) string {
	names := make([]string, 0, len(refs))
	for n := range refs {
		names = append(names, n)
	}
	sort.Strings(names)
	var b strings.Builder
	for _, n := range names {
		b.WriteString(n + " " + refs[n] + "\n")
	}
	return HashObject("refs", []byte(b.String()))[:16]
}

// refsChanged returns a channel closed at the next ref change by this process.
func (s *Server) refsChanged() <-chan struct{} {
	s.refsMu.Lock()
	defer s.refsMu.Unlock()
	if s.refsCh == nil {
		s.refsCh = make(chan struct{})
	}
	return s.refsCh
}

func (s *Server) notifyRefs() {
	s.refsMu.Lock()
	if s.refsCh != nil {
		close(s.refsCh)
		s.refsCh = nil
	}
	s.refsMu.Unlock()
}

func (s *Server) handleRefs(w http.ResponseWriter, req *http.Request) {
	view := s.viewOf(req)
	list := func() map[string]string {
		refs := s.repo.ListRefs()
		if view != nil {
			if p, err := view.projectRefs(s.repo, refs); err == nil {
				return p
			}
			return map[string]string{}
		}
		return refs
	}
	refs := list()
	etag := refsETag(refs)
	if wait := req.URL.Query().Get("wait"); wait != "" && wait == etag {
		deadline := time.NewTimer(longPoll)
		defer deadline.Stop()
		poll := time.NewTicker(250 * time.Millisecond) // other processes sharing the repo don't notify us
		defer poll.Stop()
		for etag == wait {
			changed := s.refsChanged()
			select {
			case <-changed:
			case <-poll.C:
			case <-deadline.C:
				writeJSON(w, 200, map[string]any{"refs": refs, "etag": etag})
				return
			case <-req.Context().Done():
				return
			case <-s.stop:
				writeJSON(w, 200, map[string]any{"refs": refs, "etag": etag})
				return
			}
			refs = list()
			etag = refsETag(refs)
		}
	}
	writeJSON(w, 200, map[string]any{"refs": refs, "etag": etag})
}

// WaitRefs returns the remote's refs once they differ from etag ("" = now).
func (rm *Remote) WaitRefs(etag string) (map[string]string, string, error) {
	data, _, err := rm.do("GET", "/v1/refs?wait="+etag, nil, nil)
	if err != nil {
		return nil, "", err
	}
	var v struct {
		Refs map[string]string `json:"refs"`
		ETag string            `json:"etag"`
	}
	if err := json.Unmarshal(data, &v); err != nil {
		return nil, "", err
	}
	return v.Refs, v.ETag, nil
}

func (s *Server) upRef(name string) (string, bool) {
	s.refsMu.Lock()
	defer s.refsMu.Unlock()
	if !s.upFresh {
		return "", false
	}
	return s.upRefs[name], true
}

func (s *Server) setUpRef(name, id string, fresh bool) {
	s.refsMu.Lock()
	defer s.refsMu.Unlock()
	if !fresh {
		s.upFresh = false
		return
	}
	if s.upFresh {
		s.upRefs[name] = id
	}
}

// followUpstream keeps a cache's copy of upstream refs current, so landings
// start from the right tip without asking first.
func (s *Server) followUpstream() {
	etag := ""
	for {
		select {
		case <-s.stop:
			return
		default:
		}
		refs, next, err := s.upstream.WaitRefs(etag)
		s.refsMu.Lock()
		if err == nil {
			s.upRefs, s.upFresh = refs, true
		} else {
			s.upFresh = false
		}
		s.refsMu.Unlock()
		if err != nil {
			sleepOrStop(s.stop, time.Second)
			etag = ""
			continue
		}
		etag = next
	}
}

func sleepOrStop(stop <-chan struct{}, d time.Duration) {
	select {
	case <-stop:
	case <-time.After(d):
	}
}

// runFollow mirrors another server into this repo: for every ref change,
// fetch what is missing (dependencies first), then move the ref.
func (s *Server) runFollow() {
	etag := ""
	for {
		select {
		case <-s.stop:
			return
		default:
		}
		refs, next, err := s.follow.WaitRefs(etag)
		if err == nil {
			err = s.mirror(refs)
		}
		if err != nil {
			select {
			case <-s.stop:
				return
			default:
			}
			fmt.Fprintf(os.Stderr, "sprig: following %s: %v\n", s.follow.base(), err)
			sleepOrStop(s.stop, time.Second)
			etag = ""
			continue
		}
		etag = next
	}
}

func (s *Server) mirror(refs map[string]string) error {
	local := s.repo.ListRefs()
	for name, id := range refs {
		if local[name] == id || !validRefName(name) {
			continue
		}
		if _, err := s.repo.fetchTip(s.follow, id); err != nil {
			return err
		}
		if err := s.repo.WriteRef(name, id); err != nil {
			return err
		}
	}
	for name := range local {
		if _, ok := refs[name]; !ok {
			if err := s.repo.DeleteRef(name); err != nil {
				return err
			}
		}
	}
	s.notifyRefs()
	return nil
}

// readOnly refuses writes on a standby; reads (including the POSTs that only
// read) pass.
func (s *Server) readOnly(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		write := req.Method == "PUT" || req.Method == "POST" &&
			(strings.HasPrefix(req.URL.Path, "/v1/refs/") || req.URL.Path == "/v1/land" || req.URL.Path == "/v1/objects/put")
		if write {
			httpErr(w, 503, "read-only standby of %s; send writes there", s.follow.base())
			return
		}
		next.ServeHTTP(w, req)
	})
}
