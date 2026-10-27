package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

type Entry struct {
	Name string `json:"name"`
	Mode string `json:"mode"` // file | exec | link | dir
	ID   string `json:"id"`
}

type Tree struct {
	Entries []Entry `json:"entries"`
}

type FileEnt struct {
	Mode string
	ID   string
}

// Flat is a whole tree as path -> file entry. Most operations (diff, merge,
// status, checkout) are simplest over the flat form.
type Flat map[string]FileEnt

func validMode(m string) bool {
	switch m {
	case "file", "exec", "link", "dir":
		return true
	}
	return false
}

func validName(n string) bool {
	return n != "" && n != "." && n != ".." && !strings.ContainsAny(n, "/\x00")
}

func parseTree(data []byte) (*Tree, error) {
	var t Tree
	if err := json.Unmarshal(data, &t); err != nil {
		return nil, fmt.Errorf("bad tree: %w", err)
	}
	seen := map[string]bool{}
	for _, e := range t.Entries {
		if !validName(e.Name) || !validMode(e.Mode) || !isID(e.ID) {
			return nil, fmt.Errorf("bad tree entry %+v", e)
		}
		if seen[e.Name] {
			return nil, fmt.Errorf("duplicate tree entry %q", e.Name)
		}
		seen[e.Name] = true
	}
	return &t, nil
}

func (r *Repo) PutTree(t *Tree) (string, error) {
	sort.Slice(t.Entries, func(i, j int) bool { return t.Entries[i].Name < t.Entries[j].Name })
	if t.Entries == nil {
		t.Entries = []Entry{}
	}
	data, err := json.Marshal(t)
	if err != nil {
		return "", err
	}
	return r.PutObject(TypeTree, data)
}

func (r *Repo) GetTree(id string) (*Tree, error) {
	typ, data, err := r.GetObject(id)
	if err != nil {
		return nil, err
	}
	if typ != TypeTree {
		return nil, fmt.Errorf("object %s is a %s, not a tree", short(id), typ)
	}
	return parseTree(data)
}

func (r *Repo) Flatten(treeID string) (Flat, error) {
	out := Flat{}
	if treeID == "" {
		return out, nil
	}
	if !r.Bare {
		if f, ok := r.loadFlatMemo(treeID); ok {
			return f, nil
		}
	}
	err := r.flatten(treeID, "", out)
	if err == nil && !r.Bare {
		r.saveFlatMemo(treeID, out)
	}
	return out, err
}

// The flat memo keeps the most recently flattened tree (almost always HEAD's)
// so status doesn't decode every tree object each run.
func (r *Repo) flatMemoPath() string { return filepath.Join(r.Dir, "flatmemo") }

func (r *Repo) loadFlatMemo(treeID string) (Flat, bool) {
	b, err := os.ReadFile(r.flatMemoPath())
	if err != nil {
		return nil, false
	}
	lines := strings.Split(strings.TrimRight(string(b), "\n"), "\n")
	if lines[0] != treeID {
		return nil, false
	}
	out := make(Flat, len(lines))
	for _, l := range lines[1:] {
		f := strings.Split(l, "\t")
		if len(f) != 3 {
			return nil, false
		}
		out[f[0]] = FileEnt{f[1], f[2]}
	}
	return out, true
}

func (r *Repo) saveFlatMemo(treeID string, f Flat) {
	var b strings.Builder
	b.WriteString(treeID + "\n")
	for p, fe := range f {
		if strings.ContainsAny(p, "\t\n") {
			return
		}
		b.WriteString(p + "\t" + fe.Mode + "\t" + fe.ID + "\n")
	}
	atomicWrite(r.flatMemoPath(), []byte(b.String()))
}

func (r *Repo) flatten(id, prefix string, out Flat) error {
	t, err := r.GetTree(id)
	if err != nil {
		return err
	}
	for _, e := range t.Entries {
		p := prefix + e.Name
		if e.Mode == "dir" {
			if err := r.flatten(e.ID, p+"/", out); err != nil {
				return err
			}
		} else {
			out[p] = FileEnt{e.Mode, e.ID}
		}
	}
	return nil
}

type node struct {
	files map[string]FileEnt
	dirs  map[string]*node
}

func newNode() *node { return &node{files: map[string]FileEnt{}, dirs: map[string]*node{}} }

func (r *Repo) BuildTree(flat Flat) (string, error) {
	root := newNode()
	for p, fe := range flat {
		if !validPath(p) {
			return "", fmt.Errorf("invalid path %q", p)
		}
		parts := strings.Split(p, "/")
		n := root
		for _, d := range parts[:len(parts)-1] {
			c := n.dirs[d]
			if c == nil {
				c = newNode()
				n.dirs[d] = c
			}
			n = c
		}
		n.files[parts[len(parts)-1]] = fe
	}
	return r.writeNode(root)
}

func (r *Repo) writeNode(n *node) (string, error) {
	t := &Tree{}
	for name, fe := range n.files {
		if _, clash := n.dirs[name]; clash {
			return "", fmt.Errorf("path %q is both a file and a directory", name)
		}
		t.Entries = append(t.Entries, Entry{name, fe.Mode, fe.ID})
	}
	for name, c := range n.dirs {
		id, err := r.writeNode(c)
		if err != nil {
			return "", err
		}
		t.Entries = append(t.Entries, Entry{name, "dir", id})
	}
	return r.PutTree(t)
}

// lookup finds path p in a tree without flattening it ("" = empty tree).
// A directory is returned with mode "dir".
func (r *Repo) lookup(treeID, p string) (FileEnt, bool, error) {
	for treeID != "" {
		t, err := r.GetTree(treeID)
		if err != nil {
			return FileEnt{}, false, err
		}
		name, rest, deeper := strings.Cut(p, "/")
		treeID = ""
		for _, e := range t.Entries {
			if e.Name != name {
				continue
			}
			if !deeper {
				return FileEnt{e.Mode, e.ID}, true, nil
			}
			if e.Mode == "dir" {
				treeID, p = e.ID, rest
			}
			break
		}
	}
	return FileEnt{}, false, nil
}

// updateTree returns treeID with the given paths set (nil removes one),
// rewriting only the directories along those paths. Directories left empty
// are dropped. Callers must not pass a path that is a directory.
func (r *Repo) updateTree(treeID string, edits map[string]*FileEnt) (string, error) {
	id, _, err := r.updateNode(treeID, edits)
	return id, err
}

func (r *Repo) updateNode(treeID string, edits map[string]*FileEnt) (string, int, error) {
	byName := map[string]Entry{}
	if treeID != "" {
		t, err := r.GetTree(treeID)
		if err != nil {
			return "", 0, err
		}
		for _, e := range t.Entries {
			byName[e.Name] = e
		}
	}
	sub := map[string]map[string]*FileEnt{}
	for p, fe := range edits {
		name, rest, deeper := strings.Cut(p, "/")
		switch {
		case deeper:
			if sub[name] == nil {
				sub[name] = map[string]*FileEnt{}
			}
			sub[name][rest] = fe
		case fe == nil:
			delete(byName, name)
		default:
			byName[name] = Entry{name, fe.Mode, fe.ID}
		}
	}
	for name, ed := range sub {
		e, ok := byName[name]
		if ok && e.Mode != "dir" {
			return "", 0, fmt.Errorf("path %q is both a file and a directory", name)
		}
		id, n, err := r.updateNode(e.ID, ed)
		if err != nil {
			return "", 0, err
		}
		if n == 0 {
			delete(byName, name)
		} else {
			byName[name] = Entry{name, "dir", id}
		}
	}
	t := &Tree{}
	for _, e := range byName {
		t.Entries = append(t.Entries, e)
	}
	id, err := r.PutTree(t)
	return id, len(t.Entries), err
}

func (r *Repo) ChangeFlat(changeID string) (Flat, error) {
	if changeID == "" {
		return Flat{}, nil
	}
	c, err := r.GetChange(changeID)
	if err != nil {
		return nil, err
	}
	return r.Flatten(c.Tree)
}

// ---------------------------------------------------------------------------
// Delta between two flat trees

type Delta struct {
	Path string
	Kind string // A, M, D, or R (renamed, for display only)
	Old  FileEnt
	New  FileEnt
	From string // old path of a rename
}

func DiffFlat(a, b Flat) []Delta {
	var out []Delta
	for p, ae := range a {
		if be, ok := b[p]; !ok {
			out = append(out, Delta{p, "D", ae, FileEnt{}, ""})
		} else if ae != be {
			out = append(out, Delta{p, "M", ae, be, ""})
		}
	}
	for p, be := range b {
		if _, ok := a[p]; !ok {
			out = append(out, Delta{p, "A", FileEnt{}, be, ""})
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Path < out[j].Path })
	return out
}

// withRenames pairs deleted and added files into renames: identical content
// first, then files sharing at least half their lines (a rename plus an edit).
func (r *Repo) withRenames(ds []Delta) []Delta {
	gone := map[string][]int{} // blob id -> indexes of deletions
	for i, d := range ds {
		if d.Kind == "D" {
			gone[d.Old.ID] = append(gone[d.Old.ID], i)
		}
	}
	drop := map[int]bool{}
	var adds, dels []int
	for i, d := range ds {
		if d.Kind != "A" || len(gone[d.New.ID]) == 0 {
			continue
		}
		j := gone[d.New.ID][0]
		gone[d.New.ID] = gone[d.New.ID][1:]
		ds[i] = Delta{d.Path, "R", ds[j].Old, d.New, ds[j].Path}
		drop[j] = true
	}
	for i, d := range ds {
		switch {
		case d.Kind == "A":
			adds = append(adds, i)
		case d.Kind == "D" && !drop[i]:
			dels = append(dels, i)
		}
	}
	// shortcut: similarity is only tried for up to 50x50 candidates, to keep big diffs fast.
	if len(adds) > 0 && len(dels) > 0 && len(adds) <= 50 && len(dels) <= 50 {
		lines := map[int]map[string]int{}
		count := func(i int, fe FileEnt) map[string]int {
			if m, ok := lines[i]; ok {
				return m
			}
			data, err := r.GetBlob(fe.ID)
			if err != nil && !r.Bare { // a working-tree file not in the store yet
				data, err = os.ReadFile(r.abs(ds[i].Path))
			}
			var m map[string]int
			if err == nil && len(data) < 1<<20 && !isBinary(data) {
				m = map[string]int{}
				for _, l := range splitLines(data) {
					m[l]++
				}
			}
			lines[i] = m
			return m
		}
		type pair struct{ a, d, score int }
		var pairs []pair
		for _, a := range adds {
			am := count(a, ds[a].New)
			for _, d := range dels {
				dm := count(d, ds[d].Old)
				if len(am) == 0 || len(dm) == 0 || ds[a].New.Mode != ds[d].Old.Mode {
					continue
				}
				common, total := 0, 0
				for l, n := range am {
					common += min(n, dm[l])
					total += n
				}
				for _, n := range dm {
					total += n
				}
				if score := 200 * common / total; score >= 50 {
					pairs = append(pairs, pair{a, d, score})
				}
			}
		}
		sort.Slice(pairs, func(i, j int) bool { return pairs[i].score > pairs[j].score })
		used := map[int]bool{}
		for _, p := range pairs {
			if used[p.a] || used[p.d] {
				continue
			}
			used[p.a], used[p.d], drop[p.d] = true, true, true
			ds[p.a] = Delta{ds[p.a].Path, "R", ds[p.d].Old, ds[p.a].New, ds[p.d].Path}
		}
	}
	out := ds[:0]
	for i, d := range ds {
		if !drop[i] {
			out = append(out, d)
		}
	}
	return out
}

// ---------------------------------------------------------------------------
// Working tree

func (r *Repo) abs(rel string) string { return filepath.Join(r.Root, filepath.FromSlash(rel)) }

func (r *Repo) loadIgnore() []string {
	b, err := os.ReadFile(filepath.Join(r.Root, ".sprigignore"))
	if err != nil {
		return nil
	}
	var pats []string
	for _, line := range strings.Split(string(b), "\n") {
		line = strings.TrimSpace(line)
		if line != "" && !strings.HasPrefix(line, "#") {
			pats = append(pats, line)
		}
	}
	return pats
}

func ignored(pats []string, rel string) bool {
	base := path.Base(rel)
	for _, p := range pats {
		p = strings.TrimSuffix(p, "/")
		if ok, _ := path.Match(p, rel); ok {
			return true
		}
		if ok, _ := path.Match(p, base); ok {
			return true
		}
		if strings.HasPrefix(rel, p+"/") {
			return true
		}
	}
	return false
}

// Snapshot reads the working tree into a Flat. With store=true the file
// contents are written to the object store; otherwise they are only hashed.
func (r *Repo) Snapshot(store bool) (Flat, error) {
	if r.Bare {
		return nil, errors.New("bare repository has no working tree")
	}
	cache := r.loadStatCache()
	n := len(cache.Files)
	s := &snapper{r: r, store: store, pats: r.loadIgnore(), cfg: r.LoadConfig(), cache: cache, flat: make(Flat, n),
		fresh: statCache{Written: time.Now().UnixNano(), Files: make(map[string]statEntry, n), dirty: n == 0}}
	var err error
	dirty, ok, watched := r.askWatcher(cache)
	if ok {
		err = s.fromWatcher(dirty)
	} else {
		err = s.walk(r.Root, "")
		s.fresh.dirty = s.fresh.dirty || watched // a save the watcher can vouch for next time
	}
	if err == nil && (s.fresh.dirty || len(s.fresh.Files) != len(cache.Files)) {
		r.saveStatCache(s.fresh)
	}
	flat := s.flat
	if err != nil || len(s.cfg.Sparse) == 0 {
		return flat, err
	}
	// outside a sparse view, files are whatever HEAD says
	_, head := r.Head()
	hf, err := r.ChangeFlat(head)
	for p, fe := range hf {
		if !s.cfg.inView(p) {
			flat[p] = fe
		}
	}
	return flat, err
}

// snapper reads the working tree into a Flat, several directories at once.
type snapper struct {
	r     *Repo
	store bool
	pats  []string
	cfg   Config
	cache statCache

	mu    sync.Mutex
	flat  Flat
	fresh statCache
}

func (s *snapper) skipDir(name, rel string) bool {
	return name == ".sprig" || name == ".git" || ignored(s.pats, rel) || !s.cfg.dirInView(rel)
}

func (s *snapper) skipFile(rel string) bool { return ignored(s.pats, rel) || !s.cfg.inView(rel) }

// walk snapshots everything under dir (rel is its repo-relative path, "" for
// the root), handing subdirectories to other goroutines while slots are free.
func (s *snapper) walk(dir, rel string) error {
	var wg sync.WaitGroup
	sem := make(chan struct{}, 8)
	var errMu sync.Mutex
	var first error
	fail := func(err error) {
		errMu.Lock()
		if first == nil {
			first = err
		}
		errMu.Unlock()
	}
	var visit func(dir, rel string)
	visit = func(dir, rel string) {
		ents, err := os.ReadDir(dir)
		if err != nil {
			if !errors.Is(err, fs.ErrNotExist) {
				fail(err)
			}
			return
		}
		for _, d := range ents {
			crel := d.Name()
			if rel != "" {
				crel = rel + "/" + d.Name()
			}
			cdir := filepath.Join(dir, d.Name())
			if d.IsDir() {
				if s.skipDir(d.Name(), crel) {
					continue
				}
				select {
				case sem <- struct{}{}:
					wg.Add(1)
					go func() {
						defer wg.Done()
						defer func() { <-sem }()
						visit(cdir, crel)
					}()
				default:
					visit(cdir, crel)
				}
				continue
			}
			if s.skipFile(crel) {
				continue
			}
			info, err := d.Info()
			if err != nil {
				if !errors.Is(err, fs.ErrNotExist) { // deleted while walking: just absent
					fail(err)
				}
				continue
			}
			if err := s.file(cdir, crel, info); err != nil {
				fail(err)
			}
		}
	}
	visit(dir, rel)
	wg.Wait()
	return first
}

// file records one working file, reusing the stat cache's hash when the
// file's size, mtime and mode are unchanged.
func (s *snapper) file(abs, rel string, info fs.FileInfo) error {
	mode := "file"
	switch {
	case info.Mode()&fs.ModeSymlink != 0:
		mode = "link"
	case !info.Mode().IsRegular():
		return nil // sockets, devices, ...
	case info.Mode()&0o111 != 0:
		mode = "exec"
	}
	// Entries written less than a second before the cache was saved are not
	// trusted, since an edit within the same timestamp tick would be
	// invisible ("racily clean", as git calls it).
	st := statEntry{info.Size(), info.ModTime().UnixNano(), mode, ""}
	if e, ok := s.cache.Files[rel]; ok && e.Size == st.Size && e.MTime == st.MTime && e.Mode == mode &&
		e.MTime < s.cache.Written-int64(time.Second) && (!s.store || s.r.HasObject(e.ID)) {
		s.put(rel, e, false)
		return nil
	}
	var data []byte
	var err error
	if mode == "link" {
		target, err := os.Readlink(abs)
		if err != nil {
			return err
		}
		data = []byte(target)
	} else if data, err = os.ReadFile(abs); err != nil {
		return err
	}
	if s.store {
		if st.ID, err = s.r.PutBlob(data); err != nil {
			return err
		}
	} else {
		st.ID = HashObject(TypeBlob, data)
	}
	s.put(rel, st, true)
	return nil
}

// fromWatcher builds the snapshot from the stat cache plus the paths a
// watcher reported changed; only those are looked at on disk. Cache entries
// are trusted even inside the racy window, since the watcher covers it.
func (s *snapper) fromWatcher(dirty []string) error {
	for p, e := range s.cache.Files {
		if !s.skipFile(p) {
			s.flat[p] = FileEnt{e.Mode, e.ID}
			s.fresh.Files[p] = e
		}
	}
	for _, d := range dirty {
		for p := range s.flat { // the path, or a directory's old contents, may be gone
			if p == d || strings.HasPrefix(p, d+"/") {
				delete(s.flat, p)
				delete(s.fresh.Files, p)
				s.fresh.dirty = true
			}
		}
	}
	for _, d := range dirty {
		abs := s.r.abs(d)
		info, err := os.Lstat(abs)
		switch {
		case errors.Is(err, fs.ErrNotExist):
		case err != nil:
			return err
		case info.IsDir():
			if !s.skipDir(info.Name(), d) {
				if err := s.walk(abs, d); err != nil {
					return err
				}
			}
		case !s.skipFile(d):
			if err := s.file(abs, d, info); err != nil {
				return err
			}
		}
	}
	return nil
}

func (s *snapper) put(rel string, e statEntry, changed bool) {
	s.mu.Lock()
	s.flat[rel] = FileEnt{e.Mode, e.ID}
	s.fresh.Files[rel] = e
	s.fresh.dirty = s.fresh.dirty || changed
	s.mu.Unlock()
}

// The stat cache remembers each working file's size, mtime and hash so that
// status and commit only read files that may have changed. It is one
// tab-separated line per file, after a first line holding the save time.
type statEntry struct {
	Size  int64
	MTime int64
	Mode  string
	ID    string
}

type statCache struct {
	Written int64
	Files   map[string]statEntry
	dirty   bool // an entry was added or changed since loading
}

func (r *Repo) statCachePath() string { return filepath.Join(r.Dir, "statcache") }

func (r *Repo) loadStatCache() statCache {
	c := statCache{Files: map[string]statEntry{}}
	b, err := os.ReadFile(r.statCachePath())
	if err != nil {
		return c
	}
	lines := strings.Split(string(b), "\n")
	c.Files = make(map[string]statEntry, len(lines))
	c.Written, _ = strconv.ParseInt(lines[0], 10, 64)
	for _, l := range lines[1:] {
		f := strings.Split(l, "\t")
		if len(f) != 5 {
			continue
		}
		size, err1 := strconv.ParseInt(f[1], 10, 64)
		mtime, err2 := strconv.ParseInt(f[2], 10, 64)
		if err1 == nil && err2 == nil {
			c.Files[f[0]] = statEntry{size, mtime, f[3], f[4]}
		}
	}
	return c
}

func (r *Repo) saveStatCache(c statCache) {
	var b strings.Builder
	fmt.Fprintf(&b, "%d\n", c.Written)
	for p, e := range c.Files {
		if !strings.ContainsAny(p, "\t\n") {
			fmt.Fprintf(&b, "%s\t%d\t%d\t%s\t%s\n", p, e.Size, e.MTime, e.Mode, e.ID)
		}
	}
	atomicWrite(r.statCachePath(), []byte(b.String())) // best effort: losing it only costs a re-read
}

// dirInView reports whether a directory holds anything in the sparse view.
func (c Config) dirInView(dir string) bool {
	if c.inView(dir) {
		return true
	}
	for _, pre := range c.Sparse {
		if strings.HasPrefix(pre, dir+"/") {
			return true
		}
	}
	return false
}

// Materialize makes the working tree match `to`, given that it currently
// matches `from`. Ignored files and files outside a sparse view are left alone.
func (r *Repo) Materialize(from, to Flat) error {
	cfg := r.LoadConfig()
	var del []string
	for p := range from {
		if _, ok := to[p]; !ok && cfg.inView(p) {
			del = append(del, p)
		}
	}
	sort.Strings(del)
	for _, p := range del {
		a := r.abs(p)
		if err := os.Remove(a); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return err
		}
		r.pruneDirs(filepath.Dir(a))
	}
	paths := make([]string, 0, len(to))
	for p := range to {
		if cfg.inView(p) {
			paths = append(paths, p)
		}
	}
	sort.Strings(paths)
	for _, p := range paths {
		te := to[p]
		if fe, ok := from[p]; ok && fe == te {
			continue
		}
		if err := r.writeFile(p, te); err != nil {
			return err
		}
	}
	return nil
}

func (r *Repo) pruneDirs(dir string) {
	for dir != r.Root && strings.HasPrefix(dir, r.Root) {
		if err := os.Remove(dir); err != nil {
			return
		}
		dir = filepath.Dir(dir)
	}
}

func (r *Repo) writeFile(rel string, fe FileEnt) error {
	a := r.abs(rel)
	if !strings.HasPrefix(a, r.Root+string(filepath.Separator)) {
		return fmt.Errorf("refusing to write outside the repository: %s", rel)
	}
	// Clear anything in the way: a file where a directory must go, or a
	// directory where a file must go.
	parts := strings.Split(rel, "/")
	cur := r.Root
	for _, seg := range parts[:len(parts)-1] {
		cur = filepath.Join(cur, seg)
		if st, err := os.Lstat(cur); err == nil && !st.IsDir() {
			if err := os.Remove(cur); err != nil {
				return err
			}
		}
	}
	if err := os.MkdirAll(filepath.Dir(a), 0o755); err != nil {
		return err
	}
	if st, err := os.Lstat(a); err == nil && st.IsDir() {
		if err := os.RemoveAll(a); err != nil {
			return err
		}
	}
	data, err := r.GetBlob(fe.ID)
	if err != nil {
		return err
	}
	if fe.Mode == "link" {
		os.Remove(a)
		return os.Symlink(string(data), a)
	}
	perm := os.FileMode(0o644)
	if fe.Mode == "exec" {
		perm = 0o755
	}
	if err := os.WriteFile(a, data, perm); err != nil {
		return err
	}
	return os.Chmod(a, perm)
}

// IsDirty reports whether the working tree differs from the given flat tree.
func (r *Repo) workingMatches(want Flat) (bool, []Delta, error) {
	cur, err := r.Snapshot(false)
	if err != nil {
		return false, nil, err
	}
	d := DiffFlat(want, cur)
	return len(d) == 0, d, nil
}

func hasMarkers(data []byte) bool {
	return bytes.HasPrefix(data, []byte("<<<<<<< ")) || bytes.Contains(data, []byte("\n<<<<<<< ")) ||
		bytes.HasPrefix(data, []byte(">>>>>>> ")) || bytes.Contains(data, []byte("\n>>>>>>> "))
}
