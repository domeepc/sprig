package main

import (
	"bytes"
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"maps"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// ---------------------------------------------------------------------------
// Server: the agent-facing side. Everything a CI job or tool needs to ask
// about the repo (history, files, refs) is an API call; no local clone.

type Server struct {
	repo     *Repo
	token    string           // full access
	scopes   map[string]Scope // limited tokens
	upstream *Remote          // set when this server is a cache in front of another
	follow   *Remote          // set when this server is a read-only standby of another

	stopOnce sync.Once
	stop     chan struct{} // closed by Stop: ends long-polls and followers

	refsMu  sync.Mutex
	refsCh  chan struct{}     // closed when refs change (long-poll wake-up)
	upRefs  map[string]string // a cache's copy of upstream refs, kept by followUpstream
	upFresh bool
}

const maxObjectSize = 256 << 20

func NewServer(r *Repo, token string) *Server {
	return &Server{repo: r, token: token, stop: make(chan struct{})}
}

// Stop ends long-polls and background followers, so the HTTP server can
// shut down without waiting for them.
func (s *Server) Stop() { s.stopOnce.Do(func() { close(s.stop) }) }

type scopeKey struct{}

// Scope limits a token. In tokens.json a plain list means Write only.
type Scope struct {
	Write []string `json:"write"`          // path prefixes it may land in
	Read  []string `json:"read,omitempty"` // if set, the only paths it may read
}

func (sc *Scope) UnmarshalJSON(b []byte) error {
	var list []string
	if json.Unmarshal(b, &list) == nil {
		sc.Write = list
		return nil
	}
	type plain Scope
	return json.Unmarshal(b, (*plain)(sc))
}

func scopeOf(req *http.Request) (Scope, bool) {
	sc, ok := req.Context().Value(scopeKey{}).(Scope)
	return sc, ok
}

// readLimited refuses uploads from read-scoped tokens: they work from a
// projected history and may only land.
func readLimited(w http.ResponseWriter, req *http.Request) bool {
	if sc, ok := scopeOf(req); ok && sc.Read != nil {
		httpErr(w, 403, "a read-scoped token can't upload objects; use land")
		return true
	}
	return false
}

// mayRead reports whether path p, or something below it, is readable.
func mayRead(req *http.Request, p string) (whole, part bool) {
	sc, ok := scopeOf(req)
	if !ok || sc.Read == nil || inScope(sc.Read, p) {
		return true, true
	}
	for _, pre := range sc.Read {
		if p == "" || strings.HasPrefix(pre, p+"/") {
			return false, true
		}
	}
	return false, false
}

func inScope(scope []string, p string) bool {
	for _, pre := range scope {
		if p == pre || strings.HasPrefix(p, pre+"/") {
			return true
		}
	}
	return false
}

func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	if s.upstream != nil {
		// Cache tier: objects never change, so they are served locally once
		// fetched; everything else (refs, landing, log, ...) goes upstream.
		s.repo.fallback = s.upstream
		up := s.upstream
		proxy := &httputil.ReverseProxy{
			Rewrite: func(pr *httputil.ProxyRequest) {
				u, _ := url.Parse(up.base())
				pr.SetURL(u)
			},
			ErrorHandler: func(w http.ResponseWriter, req *http.Request, err error) {
				up.failedOver(req.URL.Scheme + "://" + req.URL.Host)
				httpErr(w, 502, "upstream: %v", err)
			},
		}
		mux.Handle("/v1/", proxy)
		// Read-scoped tokens see a projection only the main server has.
		local := func(h http.HandlerFunc) http.HandlerFunc {
			return func(w http.ResponseWriter, req *http.Request) {
				if sc, ok := scopeOf(req); ok && sc.Read != nil {
					proxy.ServeHTTP(w, req)
					return
				}
				h(w, req)
			}
		}
		go s.followUpstream()
		mux.HandleFunc("GET /v1/object/{id}", local(s.handleGetObject))
		mux.HandleFunc("GET /v1/changes/{id}", local(s.handleChange))
		mux.HandleFunc("POST /v1/objects/get", local(s.handleGetObjects))
		mux.HandleFunc("POST /v1/land", local(s.handleLand)) // merge here; only the ref swap goes upstream
		return s.auth(mux)
	}
	defer func() {
		if s.follow != nil {
			go s.runFollow()
		}
	}()
	mux.HandleFunc("GET /v1/refs", s.handleRefs)
	mux.HandleFunc("GET /v1/object/{id}", s.handleGetObject)
	mux.HandleFunc("PUT /v1/object/{id}", s.handlePutObject)
	mux.HandleFunc("POST /v1/objects/missing", s.handleMissing)
	mux.HandleFunc("POST /v1/objects/get", s.handleGetObjects)
	mux.HandleFunc("POST /v1/objects/put", s.handlePutObjects)
	mux.HandleFunc("POST /v1/refs/{name...}", s.handleUpdateRef)
	mux.HandleFunc("POST /v1/land", s.handleLand)
	mux.HandleFunc("GET /v1/log", s.handleLog)
	mux.HandleFunc("GET /v1/changes/{id}", s.handleChange)
	mux.HandleFunc("GET /v1/file", s.handleFile)
	mux.HandleFunc("GET /v1/ls", s.handleLs)
	if s.follow != nil {
		return s.auth(s.readOnly(mux))
	}
	return s.auth(mux)
}

func (s *Server) auth(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if s.token == "" && len(s.scopes) == 0 {
			next.ServeHTTP(w, req)
			return
		}
		got := []byte(strings.TrimPrefix(req.Header.Get("Authorization"), "Bearer "))
		if s.token != "" && subtle.ConstantTimeCompare(got, []byte(s.token)) == 1 {
			next.ServeHTTP(w, req)
			return
		}
		for tok, scope := range s.scopes {
			if subtle.ConstantTimeCompare(got, []byte(tok)) == 1 {
				next.ServeHTTP(w, req.WithContext(context.WithValue(req.Context(), scopeKey{}, scope)))
				return
			}
		}
		httpErr(w, http.StatusUnauthorized, "unauthorized")
	})
}

func httpErr(w http.ResponseWriter, code int, format string, a ...any) {
	writeJSON(w, code, map[string]string{"error": fmt.Sprintf(format, a...)})
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(v)
}

// fill copies an object from upstream into a cache server's store.
func (s *Server) fill(id string) {
	if s.upstream == nil || s.repo.HasObject(id) {
		return
	}
	if typ, data, err := s.upstream.GetObject(id); err == nil { // hash-verified
		s.repo.PutObject(typ, data)
	}
}

func (s *Server) handleGetObject(w http.ResponseWriter, req *http.Request) {
	id := req.PathValue("id")
	if !isID(id) {
		httpErr(w, 400, "bad id")
		return
	}
	if view := s.viewOf(req); view != nil && !view.may(id) {
		httpErr(w, 403, "object %s is %v", short(id), errNotInScope)
		return
	}
	s.fill(id)
	typ, data, err := s.repo.GetObject(id)
	if err != nil {
		httpErr(w, 404, "%v", err)
		return
	}
	w.Header().Set("X-Sprig-Type", typ)
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Write(data)
}

func (s *Server) handlePutObject(w http.ResponseWriter, req *http.Request) {
	if readLimited(w, req) {
		return
	}
	id, typ := req.PathValue("id"), req.Header.Get("X-Sprig-Type")
	if !isID(id) {
		httpErr(w, 400, "bad id")
		return
	}
	data, err := io.ReadAll(http.MaxBytesReader(w, req.Body, maxObjectSize))
	if err != nil {
		httpErr(w, 413, "body too large or unreadable")
		return
	}
	if HashObject(typ, data) != id {
		httpErr(w, 400, "hash mismatch")
		return
	}
	if msg := s.checkObject(typ, data); msg != "" {
		httpErr(w, 409, "%s", msg)
		return
	}
	if _, err := s.repo.PutObject(typ, data); err != nil {
		httpErr(w, 500, "%v", err)
		return
	}
	writeJSON(w, 200, map[string]string{"id": id})
}

func (s *Server) handleMissing(w http.ResponseWriter, req *http.Request) {
	if readLimited(w, req) {
		return
	}
	var body struct {
		IDs []string `json:"ids"`
	}
	if err := json.NewDecoder(http.MaxBytesReader(w, req.Body, 8<<20)).Decode(&body); err != nil {
		httpErr(w, 400, "bad json")
		return
	}
	if len(body.IDs) > 20000 {
		httpErr(w, 400, "too many ids")
		return
	}
	missing := []string{}
	for _, id := range body.IDs {
		if !isID(id) {
			httpErr(w, 400, "bad id %q", id)
			return
		}
		if !s.repo.HasObject(id) {
			missing = append(missing, id)
		}
	}
	writeJSON(w, 200, map[string]any{"missing": missing})
}

type refUpdate struct {
	Old   string `json:"old"`
	New   string `json:"new"`
	Force bool   `json:"force"`
}

func (s *Server) handleUpdateRef(w http.ResponseWriter, req *http.Request) {
	name := req.PathValue("name")
	if !validRefName(name) {
		httpErr(w, 400, "bad ref name")
		return
	}
	var u refUpdate
	if err := json.NewDecoder(http.MaxBytesReader(w, req.Body, 1<<20)).Decode(&u); err != nil {
		httpErr(w, 400, "bad json")
		return
	}
	if _, scoped := scopeOf(req); scoped {
		httpErr(w, 403, "this token may only land changes, not move refs")
		return
	}
	unlock, err := s.repo.lockRefs()
	if err != nil {
		httpErr(w, 500, "%v", err)
		return
	}
	defer unlock()
	cur, _ := s.repo.ReadRef(name)
	if cur != u.Old {
		httpErr(w, 409, "ref moved: expected %q, found %q", short(u.Old), short(cur))
		return
	}
	if u.New == "" {
		if err := s.repo.DeleteRef(name); err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		s.logRef(reflogEntry{Ref: name, Old: cur, Via: "delete", Who: tokenKind(req)})
		writeJSON(w, 200, map[string]string{"ok": "deleted"})
		return
	}
	c, err := s.repo.GetChange(u.New)
	if err != nil {
		httpErr(w, 400, "new value is not a known change: %v", err)
		return
	}
	if !u.Force {
		if cur != "" {
			ok, err := s.repo.IsAncestor(cur, u.New)
			if err != nil {
				httpErr(w, 500, "%v", err)
				return
			}
			if !ok {
				httpErr(w, 409, "non-fast-forward update rejected")
				return
			}
		}
		// Policy gate: unresolved merge conflicts may not land on a ref.
		if len(c.Conflicts) > 0 {
			httpErr(w, 422, "tip change has %d unresolved conflict(s): %s", len(c.Conflicts), c.Conflicts[0].Path)
			return
		}
	}
	if err := s.repo.WriteRef(name, u.New); err != nil {
		httpErr(w, 500, "%v", err)
		return
	}
	s.logRef(reflogEntry{Ref: name, Old: cur, New: u.New, Via: "push", Who: tokenKind(req)})
	writeJSON(w, 200, map[string]string{"ok": "updated"})
}

// Landing: the client sends only the files it changed and the change it
// edited against, and the server applies them onto the branch's current tip.
// The merge runs outside the lock; only the ref swap is serialized, and it
// retries on a fresh tip if another landing got in first, so landings don't
// queue behind each other and nothing has to be cloned or pushed. The lock is
// a file lock, so several server processes can share one repository.

type LandFile struct {
	Mode string `json:"mode"` // file | exec | link
	Data []byte `json:"data"`
}

type LandRequest struct {
	Branch  string               `json:"branch"`
	Base    string               `json:"base"`  // change the edits were made against ("" = empty tree)
	Files   map[string]*LandFile `json:"files"` // null deletes the path
	Message string               `json:"message"`
	Author  string               `json:"author"`
	Meta    Meta                 `json:"meta"`
	Sig     *Signature           `json:"sig,omitempty"` // over landPayload; see sign.go
}

type LandResult struct {
	ID      string `json:"id"`
	Parent  string `json:"parent"`
	Retries int    `json:"retries"`
}

const maxLandRetries = 20

func (s *Server) handleLand(w http.ResponseWriter, req *http.Request) {
	var u LandRequest
	if err := json.NewDecoder(http.MaxBytesReader(w, req.Body, maxObjectSize)).Decode(&u); err != nil {
		httpErr(w, 400, "bad json")
		return
	}
	name := "heads/" + u.Branch
	switch {
	case u.Branch == "" || !validRefName(name):
		httpErr(w, 400, "bad branch name")
		return
	case len(u.Files) == 0:
		httpErr(w, 400, "nothing to land")
		return
	case strings.TrimSpace(u.Message) == "":
		httpErr(w, 400, "a message is required")
		return
	}
	if u.Base != "" && !isID(u.Base) {
		httpErr(w, 400, "bad base id")
		return
	}
	view := s.viewOf(req)
	if view != nil && u.Base != "" { // a projected clone names a projected base
		if real := view.Real(u.Base); real != "" {
			u.Base = real
		} else if !view.may(u.Base) {
			httpErr(w, 400, "unknown base change %s", short(u.Base))
			return
		}
	}
	baseTree, err := s.treeOf(u.Base)
	if err != nil {
		httpErr(w, 400, "unknown base change %s", short(u.Base))
		return
	}
	sc, scoped := scopeOf(req)
	scope := sc.Write
	// Only the edited paths are looked at, so a landing costs the same in a
	// small repo and a huge one.
	base, ours := Flat{}, Flat{}
	for p, f := range u.Files {
		if !validPath(p) {
			httpErr(w, 400, "bad path %q", p)
			return
		}
		if scoped && !inScope(scope, p) {
			httpErr(w, 403, "this token may not write %q", p)
			return
		}
		if be, ok, err := s.repo.lookup(baseTree, p); err != nil {
			httpErr(w, 500, "%v", err)
			return
		} else if ok {
			base[p] = be
		}
		if f == nil {
			continue
		}
		if f.Mode != "file" && f.Mode != "exec" && f.Mode != "link" {
			httpErr(w, 400, "bad mode %q for %q", f.Mode, p)
			return
		}
		id, err := s.repo.PutBlob(f.Data)
		if err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		ours[p] = FileEnt{f.Mode, id}
	}
	var sig *Signature
	if u.Sig != nil {
		proof := &LandProof{Branch: u.Branch, Base: u.Base, Files: map[string]string{}}
		for p, f := range u.Files {
			proof.Files[p] = ""
			if f != nil {
				proof.Files[p] = HashObject(TypeBlob, f.Data)
			}
		}
		if err := checkSig(u.Sig, landPayload(proof, u.Message, u.Author, u.Meta)); err != nil {
			httpErr(w, 400, "bad signature: %v", err)
			return
		}
		sig = &Signature{Key: u.Sig.Key, Sig: u.Sig.Sig, Land: proof}
	}
	if u.Author == "" {
		if sig != nil {
			httpErr(w, 400, "a signed landing needs an author")
			return
		}
		u.Author = "unknown <unknown@localhost>"
	}
	for retries := 0; retries < maxLandRetries; retries++ {
		tip, err := s.readRef(name)
		if err != nil {
			httpErr(w, 502, "%v", err)
			return
		}
		tipTree, err := s.treeOf(tip)
		if err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		theirs := Flat{}
		for p := range u.Files {
			te, ok, err := s.repo.lookup(tipTree, p)
			if err != nil {
				httpErr(w, 500, "%v", err)
				return
			}
			if ok && te.Mode == "dir" || base[p].Mode == "dir" {
				httpErr(w, 422, "path %q is a directory", p)
				return
			}
			if ok {
				theirs[p] = te
			}
		}
		// An edited file someone else renamed since the base: edit it at its
		// new path (the only case that needs whole trees).
		paths := slices.Collect(maps.Keys(u.Files))
		b2, o2, t2 := base, ours, theirs
		moved := map[string]string{}
		for _, p := range paths {
			if _, inB := base[p]; inB && ours[p].ID != "" && theirs[p].ID == "" {
				moved[p] = ""
			}
		}
		if len(moved) > 0 {
			bf, err1 := s.repo.Flatten(baseTree)
			tf, err2 := s.repo.Flatten(tipTree)
			if err1 != nil || err2 != nil {
				httpErr(w, 500, "%v", errors.Join(err1, err2))
				return
			}
			ren := s.repo.renames(bf, tf)
			b2, o2, t2 = maps.Clone(base), maps.Clone(ours), maps.Clone(theirs)
			for p := range moved {
				q, ok := ren[p]
				if !ok {
					delete(moved, p)
					continue
				}
				if scoped && !inScope(scope, q) {
					httpErr(w, 403, "%s was moved to %s, which this token may not write", p, q)
					return
				}
				moved[p] = q
				b2[q], o2[q] = b2[p], o2[p]
				delete(b2, p)
				delete(o2, p)
				if te, ok := tf[q]; ok {
					t2[q] = te
				}
				paths[slices.Index(paths, p)] = q
			}
		}
		merged, conflicts, err := s.repo.MergeFlat(b2, o2, t2, "landing", u.Branch)
		if err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		if len(conflicts) > 0 {
			paths := make([]string, len(conflicts))
			for i, cf := range conflicts {
				paths[i] = cf.Path
			}
			writeJSON(w, 409, map[string]any{
				"error":     fmt.Sprintf("conflicts with changes landed since your base: %s", strings.Join(paths, ", ")),
				"conflicts": paths, "tip": tip})
			return
		}
		edits := map[string]*FileEnt{}
		for _, p := range paths {
			if fe, ok := merged[p]; ok {
				edits[p] = &fe
			} else {
				edits[p] = nil
			}
		}
		tree, err := s.repo.updateTree(tipTree, edits)
		if err != nil {
			httpErr(w, 422, "%v", err)
			return
		}
		var parents []string
		if tip != "" {
			parents = []string{tip}
		}
		now := time.Now()
		id, err := s.repo.PutChange(&Change{Parents: parents, Tree: tree, Author: u.Author,
			Time: now.Unix(), TZ: now.Format("-0700"), Message: u.Message, Meta: u.Meta, Sig: withMoved(sig, moved)})
		if err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		if s.upstream != nil {
			if err := s.publish(id, tree, edits); err != nil {
				httpErr(w, 502, "%v", err)
				return
			}
		}
		raced, err := s.swapRef(name, tip, id, u.Author)
		if err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		if raced {
			continue // someone landed meanwhile: re-merge onto the new tip
		}
		res := LandResult{id, tip, retries}
		if view != nil { // answer in the token's own view of history
			if res.ID, err = view.Project(s.repo, id); err == nil && tip != "" {
				res.Parent, err = view.Project(s.repo, tip)
			}
			if err != nil {
				httpErr(w, 500, "%v", err)
				return
			}
		}
		writeJSON(w, 200, res)
		s.repo.maybeAutoGC()
		return
	}
	httpErr(w, 503, "%s moved %d times during landing; try again", u.Branch, maxLandRetries)
}

// withMoved records on a landing's signature which signed paths were edited
// at the new path someone else had renamed them to.
func withMoved(sig *Signature, moved map[string]string) *Signature {
	if sig == nil || len(moved) == 0 {
		return sig
	}
	c := *sig
	c.Moved = moved
	return &c
}

// readRef reads a branch tip, from upstream when this server is a cache.
func (s *Server) readRef(name string) (string, error) {
	if s.upstream == nil {
		id, _ := s.repo.ReadRef(name)
		return id, nil
	}
	if id, ok := s.upRef(name); ok {
		return id, nil
	}
	refs, err := s.upstream.Refs()
	return refs[name], err
}

// swapRef moves name from old to new, reporting moved=true if it no longer
// pointed at old. A cache sends this one compare-and-swap upstream; it is the
// only step of a landing that the authoritative server has to do itself.
func (s *Server) swapRef(name, old, new, who string) (moved bool, err error) {
	if s.upstream != nil {
		err := s.upstream.UpdateRef(name, old, new, false)
		var re *remoteError
		if errors.As(err, &re) && re.Code == 409 && strings.HasPrefix(re.Msg, "ref moved") {
			s.setUpRef(name, "", false) // our copy is stale: ask upstream next time
			return true, nil
		}
		if err == nil {
			s.setUpRef(name, new, true)
		}
		return false, err
	}
	unlock, err := s.repo.lockRefs()
	if err != nil {
		return false, err
	}
	defer unlock()
	if cur, _ := s.repo.ReadRef(name); cur != old {
		return true, nil
	}
	if err := s.repo.WriteRef(name, new); err != nil {
		return false, err
	}
	s.logRef(reflogEntry{Ref: name, Old: old, New: new, Via: "land", Who: who})
	return false, nil
}

// logRef records a ref change; called with the ref lock held. A failure to
// log does not undo the change, but is reported on stderr.
func (s *Server) logRef(e reflogEntry) {
	if err := s.repo.appendReflog(e); err != nil {
		fmt.Fprintf(os.Stderr, "sprig: reflog: %v\n", err)
	}
	s.notifyRefs()
}

func tokenKind(req *http.Request) string {
	if _, scoped := scopeOf(req); scoped {
		return "scoped token"
	}
	if req.Header.Get("Authorization") != "" {
		return "full token"
	}
	return "no token"
}

// publish uploads what a landing created (the edited files, the directories
// along their paths, the change) to upstream, dependencies first.
func (s *Server) publish(id, tree string, edits map[string]*FileEnt) error {
	var blobs, trees []string
	depth := map[string]int{}
	for p, fe := range edits {
		if fe != nil {
			blobs = append(blobs, fe.ID)
		}
		parts := strings.Split(p, "/")
		cur := tree
		for d := 0; cur != ""; d++ {
			if _, ok := depth[cur]; !ok {
				trees = append(trees, cur)
			}
			depth[cur] = max(depth[cur], d)
			if d == len(parts)-1 {
				break
			}
			fe, ok, err := s.repo.lookup(tree, strings.Join(parts[:d+1], "/"))
			if err != nil {
				return err
			}
			if !ok || fe.Mode != "dir" {
				break
			}
			cur = fe.ID
		}
	}
	sort.Slice(trees, func(i, j int) bool { return depth[trees[i]] > depth[trees[j]] })
	order := append(append(blobs, trees...), id)
	missing, err := s.upstream.Missing(order)
	if err != nil {
		return err
	}
	need := map[string]bool{}
	for _, m := range missing {
		need[m] = true
	}
	var send []string
	for _, o := range order {
		if need[o] {
			send = append(send, o)
			need[o] = false
		}
	}
	return s.upstream.PutObjects(send, func(id string) (object, error) {
		typ, data, err := s.repo.GetObject(id)
		return object{typ, data}, err
	})
}

func (s *Server) treeOf(changeID string) (string, error) {
	if changeID == "" {
		return "", nil
	}
	c, err := s.repo.GetChange(changeID)
	if err != nil {
		return "", err
	}
	return c.Tree, nil
}

func (s *Server) resolve(req *http.Request) (string, error) {
	ref := req.URL.Query().Get("ref")
	if ref == "" {
		ref = "main"
	}
	return s.repo.ResolveRev(ref)
}

func (s *Server) handleLog(w http.ResponseWriter, req *http.Request) {
	id, err := s.resolve(req)
	if err != nil {
		httpErr(w, 404, "%v", err)
		return
	}
	if view := s.viewOf(req); view != nil {
		if id, err = view.Project(s.repo, id); err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		if id == "" {
			writeJSON(w, 200, []any{})
			return
		}
	}
	limit := 50
	if v, err := strconv.Atoi(req.URL.Query().Get("limit")); err == nil && v > 0 && v <= 1000 {
		limit = v
	}
	type item struct {
		ID     string  `json:"id"`
		Change *Change `json:"change"`
	}
	out := []item{}
	err = s.repo.Walk(id, func(id string, c *Change) bool {
		out = append(out, item{id, c})
		return len(out) < limit
	})
	if err != nil {
		httpErr(w, 500, "%v", err)
		return
	}
	writeJSON(w, 200, out)
}

func (s *Server) handleChange(w http.ResponseWriter, req *http.Request) {
	id := req.PathValue("id")
	if view := s.viewOf(req); view != nil && !view.may(id) {
		httpErr(w, 404, "no change %s", short(id))
		return
	}
	if isID(id) {
		s.fill(id)
	}
	c, err := s.repo.GetChange(id)
	if err != nil {
		httpErr(w, 404, "%v", err)
		return
	}
	writeJSON(w, 200, c)
}

func (s *Server) handleFile(w http.ResponseWriter, req *http.Request) {
	id, err := s.resolve(req)
	if err != nil {
		httpErr(w, 404, "%v", err)
		return
	}
	p := strings.Trim(req.URL.Query().Get("path"), "/")
	if whole, _ := mayRead(req, p); !whole {
		httpErr(w, 403, "this token may not read %q", p)
		return
	}
	flat, err := s.repo.ChangeFlat(id)
	if err != nil {
		httpErr(w, 500, "%v", err)
		return
	}
	fe, ok := flat[p]
	if !ok {
		httpErr(w, 404, "no such file %q", p)
		return
	}
	data, err := s.repo.GetBlob(fe.ID)
	if err != nil {
		httpErr(w, 500, "%v", err)
		return
	}
	w.Header().Set("X-Sprig-Mode", fe.Mode)
	w.Header().Set("Content-Type", "application/octet-stream")
	w.Write(data)
}

func (s *Server) handleLs(w http.ResponseWriter, req *http.Request) {
	id, err := s.resolve(req)
	if err != nil {
		httpErr(w, 404, "%v", err)
		return
	}
	prefix := strings.Trim(req.URL.Query().Get("path"), "/")
	if _, part := mayRead(req, prefix); !part {
		httpErr(w, 403, "this token may not read %q", prefix)
		return
	}
	flat, err := s.repo.ChangeFlat(id)
	if err != nil {
		httpErr(w, 500, "%v", err)
		return
	}
	seen := map[string]string{}
	for p, fe := range flat {
		rest := p
		if prefix != "" {
			if !strings.HasPrefix(p, prefix+"/") {
				continue
			}
			rest = strings.TrimPrefix(p, prefix+"/")
		}
		name, mode := rest, fe.Mode
		if i := strings.Index(rest, "/"); i >= 0 {
			name, mode = rest[:i], "dir"
		}
		full := name
		if prefix != "" {
			full = prefix + "/" + name
		}
		if _, part := mayRead(req, full); part {
			seen[name] = mode
		}
	}
	writeJSON(w, 200, map[string]any{"entries": seen})
}

// ---------------------------------------------------------------------------
// Client

type Remote struct {
	Name  string
	URL   string // the first server; base() is the one in use
	Token string
	hc    *http.Client
	urls  []string // all servers to try, in order (a comma-separated URL)
	cur   int32    // index into urls, changed atomically
}

func (r *Repo) Remote(name string) (*Remote, error) {
	cfg := r.LoadConfig()
	u, ok := cfg.Remotes[name]
	if !ok {
		return nil, fmt.Errorf("no such remote %q (add one with `sprig remote add`)", name)
	}
	return newRemote(name, u), nil
}

// newRemote makes a client for one server, or for several given as a
// comma-separated list: requests go to the first that answers.
func newRemote(name, u string) *Remote {
	var urls []string
	for _, one := range strings.Split(u, ",") {
		if one = strings.TrimRight(strings.TrimSpace(one), "/"); one != "" {
			urls = append(urls, one)
		}
	}
	if len(urls) == 0 {
		urls = []string{""}
	}
	return &Remote{Name: name, URL: urls[0], Token: os.Getenv("SPRIG_TOKEN"),
		hc: &http.Client{Timeout: 5 * time.Minute}, urls: urls}
}

// base is the server to use now.
func (rm *Remote) base() string {
	if len(rm.urls) == 0 {
		return rm.URL
	}
	return rm.urls[int(atomic.LoadInt32(&rm.cur))%len(rm.urls)]
}

// failedOver moves on to the next server after one did not answer.
func (rm *Remote) failedOver(from string) {
	if len(rm.urls) > 1 && rm.base() == from {
		atomic.AddInt32(&rm.cur, 1)
	}
}

func (rm *Remote) do(method, path string, body []byte, hdr map[string]string) ([]byte, http.Header, error) {
	var resp *http.Response
	var base string
	tries := max(len(rm.urls), 1)
	for try := 0; ; try++ {
		base = rm.base()
		req, err := http.NewRequest(method, base+path, bytes.NewReader(body))
		if err != nil {
			return nil, nil, err
		}
		if rm.Token != "" {
			req.Header.Set("Authorization", "Bearer "+rm.Token)
		}
		for k, v := range hdr {
			req.Header.Set(k, v)
		}
		if resp, err = rm.hc.Do(req); err == nil {
			break
		}
		if try == tries-1 {
			return nil, nil, err
		}
		rm.failedOver(base) // no answer at all: try the next server
	}
	defer resp.Body.Close()
	if strings.HasPrefix(resp.Header.Get("Content-Type"), "text/html") { // sprig servers only send JSON or raw bytes
		return nil, nil, &remoteError{resp.StatusCode, base + " is not a sprig server; to publish to GitHub use `sprig export-git` and `git push`"}
	}
	data, err := io.ReadAll(io.LimitReader(resp.Body, maxObjectSize+1024))
	if err != nil {
		return nil, nil, err
	}
	if resp.StatusCode >= 300 {
		var e struct {
			Error string `json:"error"`
		}
		json.Unmarshal(data, &e)
		if e.Error == "" {
			e.Error = strings.TrimSpace(string(data))
		}
		return nil, nil, &remoteError{resp.StatusCode, e.Error}
	}
	return data, resp.Header, nil
}

type remoteError struct {
	Code int
	Msg  string
}

func (e *remoteError) Error() string { return fmt.Sprintf("remote: %s (HTTP %d)", e.Msg, e.Code) }

func (rm *Remote) Refs() (map[string]string, error) {
	data, _, err := rm.do("GET", "/v1/refs", nil, nil)
	if err != nil {
		return nil, err
	}
	var v struct {
		Refs map[string]string `json:"refs"`
	}
	if err := json.Unmarshal(data, &v); err != nil {
		return nil, err
	}
	return v.Refs, nil
}

func (rm *Remote) GetObject(id string) (string, []byte, error) {
	data, h, err := rm.do("GET", "/v1/object/"+id, nil, nil)
	if err != nil {
		return "", nil, err
	}
	typ := h.Get("X-Sprig-Type")
	if HashObject(typ, data) != id {
		return "", nil, fmt.Errorf("remote sent corrupt object %s", short(id))
	}
	return typ, data, nil
}

func (rm *Remote) Missing(ids []string) ([]string, error) {
	var out []string
	for len(ids) > 0 {
		n := min(len(ids), 5000)
		body, _ := json.Marshal(map[string]any{"ids": ids[:n]})
		data, _, err := rm.do("POST", "/v1/objects/missing", body, nil)
		if err != nil {
			return nil, err
		}
		var v struct {
			Missing []string `json:"missing"`
		}
		if err := json.Unmarshal(data, &v); err != nil {
			return nil, err
		}
		out = append(out, v.Missing...)
		ids = ids[n:]
	}
	return out, nil
}

func (rm *Remote) PutObject(id, typ string, data []byte) error {
	_, _, err := rm.do("PUT", "/v1/object/"+id, data, map[string]string{"X-Sprig-Type": typ})
	return err
}

func (rm *Remote) UpdateRef(name, old, new string, force bool) error {
	body, _ := json.Marshal(refUpdate{old, new, force})
	_, _, err := rm.do("POST", "/v1/refs/"+name, body, nil)
	return err
}

func (rm *Remote) Land(lr *LandRequest) (*LandResult, error) {
	body, err := json.Marshal(lr)
	if err != nil {
		return nil, err
	}
	data, _, err := rm.do("POST", "/v1/land", body, nil)
	if err != nil {
		return nil, err
	}
	var res LandResult
	return &res, json.Unmarshal(data, &res)
}

// closure returns every object reachable from id, dependencies first.
func (r *Repo) closure(id string) ([]string, error) {
	lazy := r.LoadConfig().Lazy != ""
	var order []string
	seen := map[string]bool{}
	var visit func(string) error
	visit = func(id string) error {
		if seen[id] {
			return nil
		}
		seen[id] = true
		if lazy && !r.HasObject(id) {
			return nil // never downloaded, so it came from the remote, which has it
		}
		typ, data, err := r.GetObject(id)
		if err != nil {
			return err
		}
		kids, err := Children(typ, data)
		if err != nil {
			return err
		}
		for _, k := range kids {
			if err := visit(k); err != nil {
				return err
			}
		}
		order = append(order, id)
		return nil
	}
	return order, visit(id)
}

func (r *Repo) Push(rm *Remote, branch string, force bool) (string, error) {
	local, ok := r.ReadRef("heads/" + branch)
	if !ok {
		return "", fmt.Errorf("no local branch %q", branch)
	}
	refs, err := rm.Refs()
	if err != nil {
		return "", err
	}
	old := refs["heads/"+branch]
	if old == local {
		return "up to date", nil
	}
	order, err := r.closure(local)
	if err != nil {
		return "", err
	}
	missing, err := rm.Missing(order)
	if err != nil {
		return "", err
	}
	need := map[string]bool{}
	for _, id := range missing {
		need[id] = true
	}
	var send []string
	for _, id := range order { // dependency order: the server enforces closure
		if need[id] {
			send = append(send, id)
		}
	}
	sent := len(send)
	if err := rm.PutObjects(send, func(id string) (object, error) {
		typ, data, err := r.GetObject(id)
		return object{typ, data}, err
	}); err != nil {
		return "", err
	}
	if err := rm.UpdateRef("heads/"+branch, old, local, force); err != nil {
		var re *remoteError
		if errors.As(err, &re) && re.Code == 409 {
			return "", fmt.Errorf("%w\nhint: pull and merge first, or push with --force", err)
		}
		return "", err
	}
	if err := r.WriteRef("remotes/"+rm.Name+"/"+branch, local); err != nil {
		return "", err
	}
	return fmt.Sprintf("pushed %s (%d objects sent)", branch, sent), nil
}

// Land sends the working tree's edits against HEAD to the remote, which
// applies them onto the branch tip. The local branch and working tree then
// move to the landed change. HEAD must be a change the remote already has.
func (r *Repo) Land(rm *Remote, branch, msg string, meta Meta) (*LandResult, error) {
	ref, head := r.Head()
	if ref != "heads/"+branch {
		return nil, fmt.Errorf("land works on the checked-out branch; run `sprig checkout %s` first", branch)
	}
	base, err := r.ChangeFlat(head)
	if err != nil {
		return nil, err
	}
	cur, err := r.Snapshot(true)
	if err != nil {
		return nil, err
	}
	files := map[string]*LandFile{}
	proof := &LandProof{Branch: branch, Base: head, Files: map[string]string{}}
	for _, d := range DiffFlat(base, cur) {
		proof.Files[d.Path] = d.New.ID // "" for a deletion
		if d.Kind == "D" {
			files[d.Path] = nil
			continue
		}
		data, err := r.GetBlob(d.New.ID)
		if err != nil {
			return nil, err
		}
		files[d.Path] = &LandFile{d.New.Mode, data}
	}
	if len(files) == 0 {
		return nil, errNothingToCommit
	}
	lr := &LandRequest{Branch: branch, Base: head, Files: files,
		Message: msg, Author: r.authorString(), Meta: meta}
	if key, err := signingKey(); err != nil {
		return nil, err
	} else if key != nil {
		lr.Sig = sign(key, landPayload(proof, lr.Message, lr.Author, lr.Meta))
	}
	res, err := rm.Land(lr)
	if err != nil {
		var re *remoteError
		if errors.As(err, &re) && strings.Contains(re.Msg, "unknown base") {
			return nil, fmt.Errorf("%w\nhint: HEAD has local changes the remote lacks; `sprig push` them or land from a fetched change", err)
		}
		return nil, err
	}
	if _, _, err := r.Fetch(rm, branch); err != nil {
		return nil, fmt.Errorf("landed %s but fetching it failed: %w", short(res.ID), err)
	}
	to, err := r.ChangeFlat(res.ID)
	if err != nil {
		return nil, err
	}
	if err := r.Materialize(cur, to); err != nil {
		return nil, err
	}
	return res, r.WriteRef(ref, res.ID)
}

// Fetch downloads the closure of the remote branch tip, dependencies first,
// so that "object present" always implies "its whole history is present".
func (r *Repo) Fetch(rm *Remote, branch string) (string, int, error) {
	refs, err := rm.Refs()
	if err != nil {
		return "", 0, err
	}
	tip, ok := refs["heads/"+branch]
	if !ok {
		return "", 0, fmt.Errorf("remote has no branch %q", branch)
	}
	n, err := r.fetchTip(rm, tip)
	if err != nil {
		return "", n, err
	}
	return tip, n, r.WriteRef("remotes/"+rm.Name+"/"+branch, tip)
}

// fetchTip downloads everything reachable from tip that is missing here.
func (r *Repo) fetchTip(rm *Remote, tip string) (int, error) {
	lazy := r.LoadConfig().Lazy != ""
	// Download level by level, one batched request per level, into a staging
	// store; then move objects in, dependencies first, so a partial fetch
	// never looks complete. Only ids are kept in memory.
	stageDir, err := os.MkdirTemp(r.Dir, ".fetch-")
	if err != nil {
		return 0, err
	}
	defer os.RemoveAll(stageDir)
	stage := &Repo{Dir: stageDir, Bare: true}
	got := map[string]bool{}
	kids := map[string][]string{}
	for level := []string{tip}; len(level) > 0; {
		var want []string
		for _, id := range level {
			if !got[id] && !r.HasObject(id) {
				got[id] = true
				want = append(want, id)
			}
		}
		level = nil
		err := rm.GetObjects(want, func(id string, o object) error {
			if !got[id] {
				return fmt.Errorf("remote sent unrequested object %s", short(id))
			}
			ks, err := Children(o.typ, o.data)
			if err != nil {
				return err
			}
			if lazy && o.typ == TypeTree { // file contents are fetched when first read
				t, err := parseTree(o.data)
				if err != nil {
					return err
				}
				ks = ks[:0]
				for _, e := range t.Entries {
					if e.Mode == "dir" {
						ks = append(ks, e.ID)
					}
				}
			}
			if _, err := stage.PutObject(o.typ, o.data); err != nil {
				return err
			}
			kids[id] = ks
			level = append(level, ks...)
			return nil
		})
		if err != nil {
			return 0, err
		}
	}
	stored := map[string]bool{}
	var store func(string) error
	store = func(id string) error {
		if !got[id] || stored[id] {
			return nil // already present locally
		}
		stored[id] = true
		for _, k := range kids[id] {
			if err := store(k); err != nil {
				return err
			}
		}
		if err := os.MkdirAll(filepath.Dir(r.objPath(id)), 0o755); err != nil {
			return err
		}
		return os.Rename(stage.objPath(id), r.objPath(id)) // same format, same filesystem
	}
	return len(stored), store(tip)
}

func serve(s *Server, addr string) error {
	if !isLoopback(addr) && s.token == "" && len(s.scopes) == 0 {
		return errors.New("refusing to listen on a non-loopback address without --token (or SPRIG_TOKEN) or --tokens")
	}
	srv := &http.Server{Addr: addr, Handler: s.Handler(), ReadHeaderTimeout: 10 * time.Second}
	what := s.repo.Dir
	if s.upstream != nil {
		what = "cache of " + strings.Join(s.upstream.urls, ", ")
	}
	if s.follow != nil {
		what += " (read-only standby of " + strings.Join(s.follow.urls, ", ") + ")"
	}
	fmt.Fprintf(stdout, "serving %s on http://%s\n", what, addr)
	return srv.ListenAndServe()
}

func isLoopback(addr string) bool {
	host := addr
	if i := strings.LastIndex(addr, ":"); i >= 0 {
		host = addr[:i]
	}
	return host == "127.0.0.1" || host == "localhost" || host == "[::1]"
}
