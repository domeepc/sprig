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
	"strings"
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
		if !validName(e.Name) || !validMode(e.Mode) || !idRe.MatchString(e.ID) {
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
	return out, r.flatten(treeID, "", out)
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
	Kind string // A, M, D
	Old  FileEnt
	New  FileEnt
}

func DiffFlat(a, b Flat) []Delta {
	var out []Delta
	for p, ae := range a {
		if be, ok := b[p]; !ok {
			out = append(out, Delta{p, "D", ae, FileEnt{}})
		} else if ae != be {
			out = append(out, Delta{p, "M", ae, be})
		}
	}
	for p, be := range b {
		if _, ok := a[p]; !ok {
			out = append(out, Delta{p, "A", FileEnt{}, be})
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Path < out[j].Path })
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
	pats := r.loadIgnore()
	flat := Flat{}
	err := filepath.WalkDir(r.Root, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(r.Root, p)
		rel = filepath.ToSlash(rel)
		if rel == "." {
			return nil
		}
		if d.IsDir() {
			if d.Name() == ".sprig" || d.Name() == ".git" || ignored(pats, rel) {
				return filepath.SkipDir
			}
			return nil
		}
		if ignored(pats, rel) {
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		var data []byte
		mode := "file"
		switch {
		case info.Mode()&fs.ModeSymlink != 0:
			target, err := os.Readlink(p)
			if err != nil {
				return err
			}
			data, mode = []byte(target), "link"
		case info.Mode().IsRegular():
			data, err = os.ReadFile(p)
			if err != nil {
				return err
			}
			if info.Mode()&0o111 != 0 {
				mode = "exec"
			}
		default:
			return nil // sockets, devices, ...
		}
		var id string
		if store {
			id, err = r.PutBlob(data)
			if err != nil {
				return err
			}
		} else {
			id = HashObject(TypeBlob, data)
		}
		flat[rel] = FileEnt{mode, id}
		return nil
	})
	return flat, err
}

// Materialize makes the working tree match `to`, given that it currently
// matches `from`. Ignored files are left alone.
func (r *Repo) Materialize(from, to Flat) error {
	var del []string
	for p := range from {
		if _, ok := to[p]; !ok {
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
		paths = append(paths, p)
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
