package main

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"time"
)

// Status in time proportional to what changed. After each status, a small
// memo records HEAD's tree, when the check started, and which paths differed
// from HEAD. Next time, if HEAD's tree is the same and a `sprig watch` can
// vouch for everything since then, only those paths and the ones the watcher
// saw change are looked at: each is compared against HEAD with a direct tree
// lookup, so neither the working tree nor HEAD is listed in full. Otherwise
// (no watcher, HEAD moved, a sparse view) status compares everything.

type statusMemo struct {
	Written int64    `json:"written"` // when that status began looking at files
	Tree    string   `json:"tree"`
	Changed []string `json:"changed"`
	Cache   int64    `json:"cache"` // the stat cache save it is consistent with
}

func (r *Repo) statusMemoPath() string { return filepath.Join(r.Dir, "statusmemo") }

// statusDeltas is what status reports: the working tree against HEAD.
func (r *Repo) statusDeltas(head string) ([]Delta, error) {
	start := time.Now().UnixNano()
	tree := ""
	if head != "" {
		c, err := r.GetChange(head)
		if err != nil {
			return nil, err
		}
		tree = c.Tree
	}
	cfg := r.LoadConfig()
	if len(cfg.Sparse) == 0 {
		var m statusMemo
		if b, err := os.ReadFile(r.statusMemoPath()); err == nil && json.Unmarshal(b, &m) == nil && m.Tree == tree {
			if dirty, ok, _ := r.askWatcherSince(m.Written); ok {
				if ds, err := r.checkPaths(tree, append(m.Changed, dirty...), cfg); err == nil {
					r.saveStatusMemo(start, tree, ds)
					return ds, nil
				}
			}
			// No watcher: stat everything, but compare against HEAD only what
			// the stat cache can't vouch for.
			if ds, ok, err := r.statusByStat(tree, m, cfg); err != nil || ok {
				if err == nil {
					r.saveStatusMemo(start, tree, ds)
				}
				return ds, err
			}
		}
	}
	hf, err := r.ChangeFlat(head)
	if err != nil {
		return nil, err
	}
	_, ds, err := r.workingMatches(hf)
	if err == nil && len(cfg.Sparse) == 0 {
		r.saveStatusMemo(start, tree, ds)
	}
	return ds, err
}

func (r *Repo) saveStatusMemo(start int64, tree string, ds []Delta) {
	m := statusMemo{Written: start, Tree: tree, Changed: []string{}, Cache: r.statCacheWritten()}
	for _, d := range ds {
		m.Changed = append(m.Changed, d.Path)
	}
	if b, err := json.Marshal(m); err == nil {
		atomicWrite(r.statusMemoPath(), b)
	}
}

// checkPaths compares just these paths (files or directories, present or
// not) between the working tree and HEAD's tree.
func (r *Repo) checkPaths(tree string, paths []string, cfg Config) ([]Delta, error) {
	sort.Strings(paths)
	paths = slices.Compact(paths)
	s := &snapper{r: r, pats: r.loadIgnore(), cfg: cfg, flat: Flat{}, fresh: statCache{Files: map[string]statEntry{}}}
	head := Flat{} // HEAD's entries at and under the paths
	var done []string
	under := func(p string) bool {
		for _, d := range done {
			if p == d || strings.HasPrefix(p, d+"/") {
				return true
			}
		}
		return false
	}
	for _, p := range paths {
		if under(p) || !validPath(p) {
			continue
		}
		done = append(done, p)
		fe, ok, err := r.lookup(tree, p)
		if err != nil {
			return nil, err
		}
		if ok && fe.Mode == "dir" {
			sub, err := r.Flatten(fe.ID)
			if err != nil {
				return nil, err
			}
			for q, e := range sub {
				head[p+"/"+q] = e
			}
		} else if ok {
			head[p] = fe
		}
		abs := r.abs(p)
		info, err := os.Lstat(abs)
		switch {
		case errors.Is(err, fs.ErrNotExist):
		case err != nil:
			return nil, err
		case info.IsDir():
			if !s.skipDir(info.Name(), p) {
				if err := s.walk(abs, p); err != nil {
					return nil, err
				}
			}
		case !s.skipFile(p):
			if err := s.file(abs, p, statOf(info)); err != nil {
				return nil, err
			}
		}
	}
	return DiffFlat(head, s.flat), nil
}

// statusByStat walks the working tree with the stat cache. A file whose
// size, mtime and mode match the cache, and that matched HEAD last time, is
// unchanged against HEAD by construction; only the rest (re-read, new,
// vanished, or different last time) is compared against HEAD's tree.
func (r *Repo) statusByStat(tree string, m statusMemo, cfg Config) ([]Delta, bool, error) {
	cache := r.loadStatCache()
	if len(cache.Files) == 0 || cache.Written != m.Cache {
		// Another command saved the cache since: an entry may record a file
		// as it is now while the memo knows it as it was. Compare everything.
		return nil, false, nil
	}
	s := &snapper{r: r, pats: r.loadIgnore(), cfg: cfg, cache: cache, flat: make(Flat, len(cache.Files)),
		fresh: statCache{Written: time.Now().UnixNano(), Files: make(map[string]statEntry, len(cache.Files))}}
	if err := s.walk(r.Root, ""); err != nil {
		return nil, false, err
	}
	cands := append(append([]string{}, m.Changed...), s.reread...)
	for p := range cache.Files {
		if _, seen := s.fresh.Files[p]; !seen {
			cands = append(cands, p) // gone from disk (or now ignored)
		}
	}
	if s.fresh.dirty || len(s.fresh.Files) != len(cache.Files) {
		r.saveStatCache(s.fresh)
	}
	ds, err := r.checkPaths(tree, cands, cfg)
	return ds, err == nil, err
}

// statCacheWritten reads just the stat cache's save time.
func (r *Repo) statCacheWritten() int64 {
	f, err := os.Open(r.statCachePath())
	if err != nil {
		return 0
	}
	defer f.Close()
	b := make([]byte, len(statCacheMagic)+binary.MaxVarintLen64)
	n, _ := io.ReadFull(f, b)
	if n < len(statCacheMagic) || string(b[:len(statCacheMagic)]) != statCacheMagic {
		return 0
	}
	v, k := binary.Uvarint(b[len(statCacheMagic):n])
	if k <= 0 {
		return 0
	}
	return int64(v)
}
