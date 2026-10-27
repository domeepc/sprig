package main

import (
	"encoding/json"
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
)

// Projected history for read-scoped tokens. Such a token sees the repository
// as if it only ever contained its paths: every change is rebuilt with a tree
// holding just those paths, changes that touched nothing there are dropped,
// and the results are stored as ordinary objects. Its refs, log, changes and
// object requests are all answered from that projection, so it can clone and
// fetch normally, and nothing outside its paths, not even commit messages,
// reaches it. A landing from a projected clone names a projected base, which
// is mapped back to the real change before merging.
//
// The mapping (real change -> projected change) and the set of objects the
// scope may fetch are kept per scope under projections/ and extended
// incrementally.

type projection struct {
	mu      sync.Mutex
	read    []string
	path    string
	M       map[string]string `json:"map"`     // real change -> projected ("" = nothing in scope yet)
	Allowed map[string]bool   `json:"allowed"` // objects this scope may fetch
	back    map[string]string // projected change -> real change
	trees   map[string]string // "tree dir" -> projected tree (memo)
	dirty   bool
}

var projections sync.Map // repo dir + scope -> *projection

func (r *Repo) projectionFor(read []string) *projection {
	sorted := append([]string{}, read...)
	sort.Strings(sorted)
	key := HashObject("scope", []byte(strings.Join(sorted, "\n")))[:16]
	if v, ok := projections.Load(r.Dir + " " + key); ok {
		return v.(*projection)
	}
	pj := &projection{read: sorted, path: filepath.Join(r.Dir, "projections", key+".json"),
		M: map[string]string{}, Allowed: map[string]bool{}, back: map[string]string{}, trees: map[string]string{}}
	if b, err := os.ReadFile(pj.path); err == nil {
		json.Unmarshal(b, pj)
		for real, p := range pj.M {
			if p != "" {
				pj.back[p] = real
			}
		}
	}
	v, _ := projections.LoadOrStore(r.Dir+" "+key, pj)
	return v.(*projection)
}

func (pj *projection) save() {
	if !pj.dirty {
		return
	}
	if b, err := json.Marshal(pj); err == nil && writeFileAtomic(pj.path, b, false) == nil {
		pj.dirty = false
	}
}

// Project maps a real change to its projection ("" when nothing in scope
// exists yet at that point in history). A projected id maps to itself.
func (pj *projection) Project(r *Repo, id string) (string, error) {
	pj.mu.Lock()
	defer pj.mu.Unlock()
	defer pj.save()
	return pj.project(r, id)
}

func (pj *projection) project(r *Repo, id string) (string, error) {
	if p, ok := pj.M[id]; ok {
		return p, nil
	}
	if _, ok := pj.back[id]; ok {
		return id, nil
	}
	c, err := r.GetChange(id)
	if err != nil {
		return "", err
	}
	var parents []string
	for _, p := range c.Parents {
		pp, err := pj.project(r, p)
		if err != nil {
			return "", err
		}
		if pp != "" && !contains(parents, pp) {
			parents = append(parents, pp)
		}
	}
	tree, err := pj.tree(r, c.Tree, "")
	if err != nil {
		return "", err
	}
	// Nothing in scope changed: this change disappears into its parent.
	if len(parents) <= 1 {
		parentTree := ""
		if len(parents) == 1 {
			pc, err := r.GetChange(parents[0])
			if err != nil {
				return "", err
			}
			parentTree = pc.Tree
		}
		empty, _ := r.PutTree(&Tree{})
		if parentTree == tree || len(parents) == 0 && tree == empty {
			pj.M[id] = firstOr(parents)
			pj.dirty = true
			return pj.M[id], nil
		}
	}
	pc := &Change{Parents: parents, Tree: tree, Author: c.Author, Time: c.Time, TZ: c.TZ,
		Message: c.Message, Meta: c.Meta, Projected: id}
	for _, cf := range c.Conflicts {
		if inScope(pj.read, cf.Path) {
			pc.Conflicts = append(pc.Conflicts, cf)
		}
	}
	pid, err := r.PutChange(pc)
	if err != nil {
		return "", err
	}
	pj.M[id], pj.back[pid], pj.Allowed[pid], pj.dirty = pid, id, true, true
	return pid, nil
}

func firstOr(list []string) string {
	if len(list) > 0 {
		return list[0]
	}
	return ""
}

func contains(list []string, s string) bool { return indexOf(list, s) >= 0 }

// tree returns treeID (the directory at dir) cut down to the scope's paths.
func (pj *projection) tree(r *Repo, treeID, dir string) (string, error) {
	memo := treeID + " " + dir
	if id, ok := pj.trees[memo]; ok {
		return id, nil
	}
	t, err := r.GetTree(treeID)
	if err != nil {
		return "", err
	}
	out := &Tree{}
	for _, e := range t.Entries {
		full := e.Name
		if dir != "" {
			full = dir + "/" + e.Name
		}
		switch {
		case inScope(pj.read, full):
			out.Entries = append(out.Entries, e)
			if err := pj.allow(r, e.ID, e.Mode); err != nil {
				return "", err
			}
		case e.Mode == "dir" && pj.leadsIn(full):
			sub, err := pj.tree(r, e.ID, full)
			if err != nil {
				return "", err
			}
			if st, err := r.GetTree(sub); err == nil && len(st.Entries) > 0 {
				out.Entries = append(out.Entries, Entry{e.Name, "dir", sub})
			}
		}
	}
	id, err := r.PutTree(out)
	if err != nil {
		return "", err
	}
	pj.trees[memo], pj.Allowed[id], pj.dirty = id, true, true
	return id, nil
}

// leadsIn reports whether a directory contains part of the scope.
func (pj *projection) leadsIn(dir string) bool {
	for _, pre := range pj.read {
		if strings.HasPrefix(pre, dir+"/") {
			return true
		}
	}
	return false
}

// allow marks an in-scope object, and everything under it, fetchable.
func (pj *projection) allow(r *Repo, id, mode string) error {
	if pj.Allowed[id] {
		return nil
	}
	pj.Allowed[id], pj.dirty = true, true
	if mode != "dir" {
		return nil
	}
	t, err := r.GetTree(id)
	if err != nil {
		return err
	}
	for _, e := range t.Entries {
		if err := pj.allow(r, e.ID, e.Mode); err != nil {
			return err
		}
	}
	return nil
}

func (pj *projection) may(id string) bool {
	pj.mu.Lock()
	defer pj.mu.Unlock()
	return pj.Allowed[id]
}

// Real maps a projected change back to the real one ("" if unknown).
func (pj *projection) Real(id string) string {
	pj.mu.Lock()
	defer pj.mu.Unlock()
	if id == "" {
		return ""
	}
	if real, ok := pj.back[id]; ok {
		return real
	}
	return ""
}

// projectRefs is what a read-scoped token sees as refs.
func (pj *projection) projectRefs(r *Repo, refs map[string]string) (map[string]string, error) {
	out := map[string]string{}
	for name, id := range refs {
		p, err := pj.Project(r, id)
		if err != nil {
			return nil, err
		}
		if p != "" {
			out[name] = p
		}
	}
	return out, nil
}

// viewOf returns the request's projection, or nil for tokens that may read
// everything.
func (s *Server) viewOf(req *http.Request) *projection {
	if sc, ok := scopeOf(req); ok && sc.Read != nil {
		return s.repo.projectionFor(sc.Read)
	}
	return nil
}

var errNotInScope = errors.New("not readable with this token")
