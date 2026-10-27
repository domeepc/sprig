package main

import (
	"bytes"
	"compress/zlib"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
)

// Object types. Everything in the store is one of these, addressed by the
// SHA-256 of "<type> <len>\x00<data>".
const (
	TypeBlob   = "blob"
	TypeTree   = "tree"
	TypeChange = "change"
)

var (
	idRe      = regexp.MustCompile(`^[0-9a-f]{64}$`)
	refNameRe = regexp.MustCompile(`^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*$`)
)

func validRefName(n string) bool {
	if !refNameRe.MatchString(n) {
		return false
	}
	for _, seg := range strings.Split(n, "/") {
		if strings.HasPrefix(seg, ".") || strings.HasSuffix(seg, ".lock") {
			return false
		}
	}
	return true
}

func HashObject(typ string, data []byte) string {
	h := sha256.New()
	fmt.Fprintf(h, "%s %d\x00", typ, len(data))
	h.Write(data)
	return hex.EncodeToString(h.Sum(nil))
}

func short(id string) string {
	if len(id) > 12 {
		return id[:12]
	}
	return id
}

// ---------------------------------------------------------------------------
// Change objects: the unit of history. Unlike a git commit it carries
// structured provenance (who/what produced it, why, and what verified it),
// and unresolved merge conflicts are stored as data inside the change.

type Meta struct {
	Intent string            `json:"intent,omitempty"` // what was asked / why
	Agent  string            `json:"agent,omitempty"`  // tool or agent that produced it
	Model  string            `json:"model,omitempty"`  // model identifier, if any
	Checks map[string]string `json:"checks,omitempty"` // e.g. tests=pass, review=approved
	GitSHA string            `json:"git_sha,omitempty"`
}

type Conflict struct {
	Path   string `json:"path"`
	Base   string `json:"base,omitempty"`   // blob ids; empty = absent on that side
	Ours   string `json:"ours,omitempty"`   //
	Theirs string `json:"theirs,omitempty"` //
	Result string `json:"result"`           // blob written into the tree at merge time
}

type Change struct {
	Parents   []string   `json:"parents"`
	Tree      string     `json:"tree"`
	Author    string     `json:"author"`
	Time      int64      `json:"time"`
	TZ        string     `json:"tz,omitempty"`
	Message   string     `json:"message"`
	Meta      Meta       `json:"meta"`
	Conflicts []Conflict `json:"conflicts,omitempty"`
}

// ---------------------------------------------------------------------------

type Repo struct {
	Root string // working tree root ("" for a bare repo)
	Dir  string // storage directory
	Bare bool
}

type Config struct {
	Author  string            `json:"author,omitempty"`
	Remotes map[string]string `json:"remotes,omitempty"`
}

func InitRepo(root string, bare bool, branch string) (*Repo, error) {
	r := &Repo{Root: root, Bare: bare}
	if bare {
		r.Dir = root
		r.Root = ""
	} else {
		r.Dir = filepath.Join(root, ".sprig")
	}
	if _, err := os.Stat(filepath.Join(r.Dir, "objects")); err == nil {
		return nil, fmt.Errorf("repository already exists in %s", r.Dir)
	}
	for _, d := range []string{"objects", "refs"} {
		if err := os.MkdirAll(filepath.Join(r.Dir, d), 0o755); err != nil {
			return nil, err
		}
	}
	if bare {
		if err := os.WriteFile(filepath.Join(r.Dir, "BARE"), []byte("1\n"), 0o644); err != nil {
			return nil, err
		}
	}
	if branch == "" {
		branch = "main"
	}
	if err := r.SetHeadRef("heads/" + branch); err != nil {
		return nil, err
	}
	return r, nil
}

func OpenRepo(path string) (*Repo, error) {
	abs, err := filepath.Abs(path)
	if err != nil {
		return nil, err
	}
	if st, err := os.Stat(filepath.Join(abs, ".sprig", "objects")); err == nil && st.IsDir() {
		return &Repo{Root: abs, Dir: filepath.Join(abs, ".sprig")}, nil
	}
	if _, err := os.Stat(filepath.Join(abs, "BARE")); err == nil {
		return &Repo{Dir: abs, Bare: true}, nil
	}
	return nil, fmt.Errorf("not a sprig repository: %s", path)
}

func FindRepo() (*Repo, error) {
	dir, err := os.Getwd()
	if err != nil {
		return nil, err
	}
	for {
		if r, err := OpenRepo(dir); err == nil {
			return r, nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return nil, errors.New("not inside a sprig repository (run `sprig init`)")
		}
		dir = parent
	}
}

// ---------------------------------------------------------------------------
// Objects

func (r *Repo) objPath(id string) string {
	return filepath.Join(r.Dir, "objects", id[:2], id[2:])
}

func (r *Repo) HasObject(id string) bool {
	if !idRe.MatchString(id) {
		return false
	}
	_, err := os.Stat(r.objPath(id))
	return err == nil
}

func (r *Repo) PutObject(typ string, data []byte) (string, error) {
	id := HashObject(typ, data)
	p := r.objPath(id)
	if _, err := os.Stat(p); err == nil {
		return id, nil
	}
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return "", err
	}
	var buf bytes.Buffer
	zw, _ := zlib.NewWriterLevel(&buf, zlib.BestSpeed)
	fmt.Fprintf(zw, "%s %d\x00", typ, len(data))
	zw.Write(data)
	if err := zw.Close(); err != nil {
		return "", err
	}
	tmp, err := os.CreateTemp(filepath.Dir(p), ".tmp-*")
	if err != nil {
		return "", err
	}
	if _, err := tmp.Write(buf.Bytes()); err != nil {
		tmp.Close()
		os.Remove(tmp.Name())
		return "", err
	}
	if err := tmp.Close(); err != nil {
		os.Remove(tmp.Name())
		return "", err
	}
	if err := os.Rename(tmp.Name(), p); err != nil {
		os.Remove(tmp.Name())
		return "", err
	}
	return id, nil
}

func (r *Repo) GetObject(id string) (string, []byte, error) {
	if !idRe.MatchString(id) {
		return "", nil, fmt.Errorf("invalid object id %q", id)
	}
	f, err := os.Open(r.objPath(id))
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) {
			return "", nil, fmt.Errorf("object %s not found", short(id))
		}
		return "", nil, err
	}
	defer f.Close()
	zr, err := zlib.NewReader(f)
	if err != nil {
		return "", nil, fmt.Errorf("object %s corrupt: %w", short(id), err)
	}
	raw, err := io.ReadAll(zr)
	if err != nil {
		return "", nil, fmt.Errorf("object %s corrupt: %w", short(id), err)
	}
	i := bytes.IndexByte(raw, 0)
	if i < 0 {
		return "", nil, fmt.Errorf("object %s corrupt: no header", short(id))
	}
	parts := strings.SplitN(string(raw[:i]), " ", 2)
	typ, data := parts[0], raw[i+1:]
	if HashObject(typ, data) != id {
		return "", nil, fmt.Errorf("object %s corrupt: hash mismatch", short(id))
	}
	return typ, data, nil
}

func (r *Repo) PutBlob(data []byte) (string, error) { return r.PutObject(TypeBlob, data) }

func (r *Repo) GetBlob(id string) ([]byte, error) {
	typ, data, err := r.GetObject(id)
	if err != nil {
		return nil, err
	}
	if typ != TypeBlob {
		return nil, fmt.Errorf("object %s is a %s, not a blob", short(id), typ)
	}
	return data, nil
}

func (r *Repo) PutChange(c *Change) (string, error) {
	if c.Parents == nil {
		c.Parents = []string{}
	}
	data, err := json.Marshal(c)
	if err != nil {
		return "", err
	}
	return r.PutObject(TypeChange, data)
}

func (r *Repo) GetChange(id string) (*Change, error) {
	typ, data, err := r.GetObject(id)
	if err != nil {
		return nil, err
	}
	if typ != TypeChange {
		return nil, fmt.Errorf("object %s is a %s, not a change", short(id), typ)
	}
	var c Change
	if err := json.Unmarshal(data, &c); err != nil {
		return nil, fmt.Errorf("change %s: %w", short(id), err)
	}
	return &c, nil
}

// Children validates a serialized object and returns the ids it references.
// The server uses it to guarantee closure: an object is only accepted once
// everything it points at is already present.
func Children(typ string, data []byte) ([]string, error) {
	switch typ {
	case TypeBlob:
		return nil, nil
	case TypeTree:
		t, err := parseTree(data)
		if err != nil {
			return nil, err
		}
		ids := make([]string, 0, len(t.Entries))
		for _, e := range t.Entries {
			ids = append(ids, e.ID)
		}
		return ids, nil
	case TypeChange:
		var c Change
		if err := json.Unmarshal(data, &c); err != nil {
			return nil, fmt.Errorf("bad change: %w", err)
		}
		var ids []string
		for _, p := range c.Parents {
			if !idRe.MatchString(p) {
				return nil, fmt.Errorf("bad parent id %q", p)
			}
			ids = append(ids, p)
		}
		if !idRe.MatchString(c.Tree) {
			return nil, fmt.Errorf("bad tree id %q", c.Tree)
		}
		ids = append(ids, c.Tree)
		for _, cf := range c.Conflicts {
			if !validPath(cf.Path) {
				return nil, fmt.Errorf("bad conflict path %q", cf.Path)
			}
			for _, b := range []string{cf.Base, cf.Ours, cf.Theirs, cf.Result} {
				if b == "" {
					continue
				}
				if !idRe.MatchString(b) {
					return nil, fmt.Errorf("bad conflict blob id %q", b)
				}
				ids = append(ids, b)
			}
		}
		return ids, nil
	}
	return nil, fmt.Errorf("unknown object type %q", typ)
}

func validPath(p string) bool {
	if p == "" || strings.HasPrefix(p, "/") {
		return false
	}
	for _, seg := range strings.Split(p, "/") {
		if !validName(seg) {
			return false
		}
	}
	return true
}

// ---------------------------------------------------------------------------
// Refs and HEAD

func (r *Repo) refPath(name string) string {
	return filepath.Join(r.Dir, "refs", filepath.FromSlash(name))
}

func (r *Repo) ReadRef(name string) (string, bool) {
	if !validRefName(name) {
		return "", false
	}
	b, err := os.ReadFile(r.refPath(name))
	if err != nil {
		return "", false
	}
	id := strings.TrimSpace(string(b))
	if !idRe.MatchString(id) {
		return "", false
	}
	return id, true
}

func atomicWrite(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), ".tmp-*")
	if err != nil {
		return err
	}
	if _, err := tmp.Write(data); err != nil {
		tmp.Close()
		os.Remove(tmp.Name())
		return err
	}
	if err := tmp.Close(); err != nil {
		os.Remove(tmp.Name())
		return err
	}
	return os.Rename(tmp.Name(), path)
}

func (r *Repo) WriteRef(name, id string) error {
	if !validRefName(name) {
		return fmt.Errorf("invalid ref name %q", name)
	}
	if !idRe.MatchString(id) {
		return fmt.Errorf("invalid id %q", id)
	}
	return atomicWrite(r.refPath(name), []byte(id+"\n"))
}

func (r *Repo) DeleteRef(name string) error {
	if !validRefName(name) {
		return fmt.Errorf("invalid ref name %q", name)
	}
	err := os.Remove(r.refPath(name))
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	return err
}

func (r *Repo) ListRefs() map[string]string {
	out := map[string]string{}
	base := filepath.Join(r.Dir, "refs")
	filepath.WalkDir(base, func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() || strings.HasPrefix(d.Name(), ".tmp-") {
			return nil
		}
		rel, _ := filepath.Rel(base, p)
		name := filepath.ToSlash(rel)
		if id, ok := r.ReadRef(name); ok {
			out[name] = id
		}
		return nil
	})
	return out
}

func (r *Repo) headPath() string { return filepath.Join(r.Dir, "HEAD") }

func (r *Repo) ReadHeadRaw() string {
	b, _ := os.ReadFile(r.headPath())
	return strings.TrimSpace(string(b))
}

func (r *Repo) SetHeadRef(ref string) error {
	return atomicWrite(r.headPath(), []byte("ref: "+ref+"\n"))
}

func (r *Repo) SetHeadDetached(id string) error {
	return atomicWrite(r.headPath(), []byte(id+"\n"))
}

// Head returns the symbolic ref ("" if detached) and the change id HEAD
// points to ("" for an unborn branch).
func (r *Repo) Head() (ref, id string) {
	raw := r.ReadHeadRaw()
	if strings.HasPrefix(raw, "ref: ") {
		ref = strings.TrimPrefix(raw, "ref: ")
		id, _ = r.ReadRef(ref)
		return ref, id
	}
	if idRe.MatchString(raw) {
		return "", raw
	}
	return "", ""
}

func (r *Repo) AdvanceHead(id string) error {
	ref, _ := r.Head()
	if ref != "" {
		return r.WriteRef(ref, id)
	}
	return r.SetHeadDetached(id)
}

// ---------------------------------------------------------------------------
// Operation log: every command that moves refs/HEAD is recorded with the
// state before and after, so any of them can be undone in one step.

type State struct {
	Head string            `json:"head"`
	Refs map[string]string `json:"refs"`
}

type Op struct {
	N      int    `json:"n"`
	Time   int64  `json:"time"`
	Cmd    string `json:"cmd"`
	Before State  `json:"before"`
	After  State  `json:"after"`
}

func (r *Repo) CaptureState() State {
	return State{Head: r.ReadHeadRaw(), Refs: r.ListRefs()}
}

func statesEqual(a, b State) bool {
	if a.Head != b.Head || len(a.Refs) != len(b.Refs) {
		return false
	}
	for k, v := range a.Refs {
		if b.Refs[k] != v {
			return false
		}
	}
	return true
}

func (r *Repo) RestoreState(s State) error {
	cur := r.ListRefs()
	for name := range cur {
		if _, ok := s.Refs[name]; !ok {
			if err := r.DeleteRef(name); err != nil {
				return err
			}
		}
	}
	for name, id := range s.Refs {
		if err := r.WriteRef(name, id); err != nil {
			return err
		}
	}
	return atomicWrite(r.headPath(), []byte(s.Head+"\n"))
}

func (r *Repo) opsPath() string { return filepath.Join(r.Dir, "oplog.jsonl") }

func (r *Repo) Ops() ([]Op, error) {
	b, err := os.ReadFile(r.opsPath())
	if errors.Is(err, fs.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var ops []Op
	for _, line := range bytes.Split(b, []byte("\n")) {
		if len(bytes.TrimSpace(line)) == 0 {
			continue
		}
		var op Op
		if err := json.Unmarshal(line, &op); err != nil {
			return nil, fmt.Errorf("oplog corrupt: %w", err)
		}
		ops = append(ops, op)
	}
	return ops, nil
}

func (r *Repo) recordOp(cmd string, before State) error {
	after := r.CaptureState()
	if statesEqual(before, after) {
		return nil
	}
	ops, err := r.Ops()
	if err != nil {
		return err
	}
	op := Op{N: len(ops) + 1, Time: time.Now().Unix(), Cmd: cmd, Before: before, After: after}
	line, _ := json.Marshal(op)
	f, err := os.OpenFile(r.opsPath(), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	defer f.Close()
	_, err = f.Write(append(line, '\n'))
	return err
}

// Do runs fn and records the resulting ref/HEAD movement in the oplog,
// even if fn failed part-way.
func (r *Repo) Do(cmd string, fn func() error) error {
	before := r.CaptureState()
	err := fn()
	if rerr := r.recordOp(cmd, before); rerr != nil && err == nil {
		err = rerr
	}
	return err
}

// ---------------------------------------------------------------------------
// Config

func (r *Repo) configPath() string { return filepath.Join(r.Dir, "config.json") }

func (r *Repo) LoadConfig() Config {
	var c Config
	if b, err := os.ReadFile(r.configPath()); err == nil {
		json.Unmarshal(b, &c)
	}
	if c.Remotes == nil {
		c.Remotes = map[string]string{}
	}
	return c
}

func (r *Repo) SaveConfig(c Config) error {
	b, _ := json.MarshalIndent(c, "", "  ")
	return atomicWrite(r.configPath(), append(b, '\n'))
}

// ---------------------------------------------------------------------------
// Revision resolution

func (r *Repo) ResolveRev(s string) (string, error) {
	s = strings.TrimSpace(s)
	steps := 0
	if i := strings.LastIndex(s, "~"); i >= 0 {
		n := 1
		if rest := s[i+1:]; rest != "" {
			if _, err := fmt.Sscanf(rest, "%d", &n); err != nil {
				return "", fmt.Errorf("bad revision %q", s)
			}
		}
		steps = n
		s = s[:i]
	}
	id, err := r.resolveBase(s)
	if err != nil {
		return "", err
	}
	for ; steps > 0; steps-- {
		c, err := r.GetChange(id)
		if err != nil {
			return "", err
		}
		if len(c.Parents) == 0 {
			return "", fmt.Errorf("%s has no parent", short(id))
		}
		id = c.Parents[0]
	}
	return id, nil
}

func (r *Repo) resolveBase(s string) (string, error) {
	if s == "" || s == "HEAD" || s == "@" {
		_, id := r.Head()
		if id == "" {
			return "", errors.New("no changes yet")
		}
		return id, nil
	}
	for _, cand := range []string{"heads/" + s, "remotes/" + s, s} {
		if id, ok := r.ReadRef(cand); ok {
			return id, nil
		}
	}
	if idRe.MatchString(s) {
		if !r.HasObject(s) {
			return "", fmt.Errorf("unknown change %s", short(s))
		}
		return s, nil
	}
	if len(s) >= 4 && len(s) < 64 && regexp.MustCompile(`^[0-9a-f]+$`).MatchString(s) {
		dir := filepath.Join(r.Dir, "objects", s[:2])
		ents, _ := os.ReadDir(dir)
		var found []string
		for _, e := range ents {
			if strings.HasPrefix(e.Name(), s[2:]) {
				id := s[:2] + e.Name()
				if typ, _, err := r.GetObject(id); err == nil && typ == TypeChange {
					found = append(found, id)
				}
			}
		}
		sort.Strings(found)
		switch len(found) {
		case 1:
			return found[0], nil
		case 0:
		default:
			return "", fmt.Errorf("ambiguous change prefix %q", s)
		}
	}
	return "", fmt.Errorf("unknown revision %q", s)
}
