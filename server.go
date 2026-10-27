package main

import (
	"bytes"
	"context"
	"crypto/subtle"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"
)

// ---------------------------------------------------------------------------
// Server: the agent-facing side. Everything a CI job or tool needs to ask
// about the repo (history, files, refs) is an API call; no local clone.

type Server struct {
	repo     *Repo
	token    string              // full access
	scopes   map[string][]string // token -> path prefixes it may land in
	upstream *Remote             // set when this server is a cache in front of another
}

const maxObjectSize = 256 << 20

func NewServer(r *Repo, token string) *Server { return &Server{repo: r, token: token} }

type scopeKey struct{}

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
		u, _ := url.Parse(s.upstream.URL)
		mux.Handle("/v1/", &httputil.ReverseProxy{Rewrite: func(pr *httputil.ProxyRequest) { pr.SetURL(u) }})
		mux.HandleFunc("GET /v1/object/{id}", s.handleGetObject)
		mux.HandleFunc("GET /v1/changes/{id}", s.handleChange)
		return s.auth(mux)
	}
	mux.HandleFunc("GET /v1/refs", s.handleRefs)
	mux.HandleFunc("GET /v1/object/{id}", s.handleGetObject)
	mux.HandleFunc("PUT /v1/object/{id}", s.handlePutObject)
	mux.HandleFunc("POST /v1/objects/missing", s.handleMissing)
	mux.HandleFunc("POST /v1/refs/{name...}", s.handleUpdateRef)
	mux.HandleFunc("POST /v1/land", s.handleLand)
	mux.HandleFunc("GET /v1/log", s.handleLog)
	mux.HandleFunc("GET /v1/changes/{id}", s.handleChange)
	mux.HandleFunc("GET /v1/file", s.handleFile)
	mux.HandleFunc("GET /v1/ls", s.handleLs)
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

func (s *Server) handleRefs(w http.ResponseWriter, req *http.Request) {
	writeJSON(w, 200, map[string]any{"refs": s.repo.ListRefs()})
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
	if !idRe.MatchString(id) {
		httpErr(w, 400, "bad id")
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
	id, typ := req.PathValue("id"), req.Header.Get("X-Sprig-Type")
	if !idRe.MatchString(id) {
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
	kids, err := Children(typ, data)
	if err != nil {
		httpErr(w, 400, "%v", err)
		return
	}
	for _, k := range kids {
		if !s.repo.HasObject(k) {
			httpErr(w, 409, "missing dependency %s", short(k))
			return
		}
	}
	if _, err := s.repo.PutObject(typ, data); err != nil {
		httpErr(w, 500, "%v", err)
		return
	}
	writeJSON(w, 200, map[string]string{"id": id})
}

func (s *Server) handleMissing(w http.ResponseWriter, req *http.Request) {
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
		if !idRe.MatchString(id) {
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
	if _, scoped := req.Context().Value(scopeKey{}).([]string); scoped {
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
	if u.Base != "" && !idRe.MatchString(u.Base) {
		httpErr(w, 400, "bad base id")
		return
	}
	baseTree, err := s.treeOf(u.Base)
	if err != nil {
		httpErr(w, 400, "unknown base change %s", short(u.Base))
		return
	}
	scope, scoped := req.Context().Value(scopeKey{}).([]string)
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
	if u.Author == "" {
		u.Author = "unknown <unknown@localhost>"
	}
	for retries := 0; retries < maxLandRetries; retries++ {
		tip, _ := s.repo.ReadRef(name)
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
		merged, conflicts, err := s.repo.MergeFlat(base, ours, theirs, "landing", u.Branch)
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
		for p := range u.Files {
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
			Time: now.Unix(), TZ: now.Format("-0700"), Message: u.Message, Meta: u.Meta})
		if err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		unlock, err := s.repo.lockRefs()
		if err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		cur, _ := s.repo.ReadRef(name)
		if cur != tip {
			unlock()
			continue // someone landed meanwhile: re-merge onto the new tip
		}
		err = s.repo.WriteRef(name, id)
		unlock()
		if err != nil {
			httpErr(w, 500, "%v", err)
			return
		}
		writeJSON(w, 200, LandResult{id, tip, retries})
		return
	}
	httpErr(w, 503, "%s moved %d times during landing; try again", u.Branch, maxLandRetries)
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
	if idRe.MatchString(id) {
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
		if i := strings.Index(rest, "/"); i >= 0 {
			seen[rest[:i]] = "dir"
		} else {
			seen[rest] = fe.Mode
		}
	}
	writeJSON(w, 200, map[string]any{"entries": seen})
}

// ---------------------------------------------------------------------------
// Client

type Remote struct {
	Name  string
	URL   string
	Token string
	hc    *http.Client
}

func (r *Repo) Remote(name string) (*Remote, error) {
	cfg := r.LoadConfig()
	u, ok := cfg.Remotes[name]
	if !ok {
		return nil, fmt.Errorf("no such remote %q (add one with `sprig remote add`)", name)
	}
	return newRemote(name, u), nil
}

func newRemote(name, u string) *Remote {
	return &Remote{Name: name, URL: strings.TrimRight(u, "/"), Token: os.Getenv("SPRIG_TOKEN"),
		hc: &http.Client{Timeout: 5 * time.Minute}}
}

func (rm *Remote) do(method, path string, body []byte, hdr map[string]string) ([]byte, http.Header, error) {
	req, err := http.NewRequest(method, rm.URL+path, bytes.NewReader(body))
	if err != nil {
		return nil, nil, err
	}
	if rm.Token != "" {
		req.Header.Set("Authorization", "Bearer "+rm.Token)
	}
	for k, v := range hdr {
		req.Header.Set(k, v)
	}
	resp, err := rm.hc.Do(req)
	if err != nil {
		return nil, nil, err
	}
	defer resp.Body.Close()
	if strings.HasPrefix(resp.Header.Get("Content-Type"), "text/html") { // sprig servers only send JSON or raw bytes
		return nil, nil, &remoteError{resp.StatusCode, rm.URL + " is not a sprig server; to publish to GitHub use `sprig export-git` and `git push`"}
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
	sent := 0
	for _, id := range order { // dependency order: the server enforces closure
		if !need[id] {
			continue
		}
		typ, data, err := r.GetObject(id)
		if err != nil {
			return "", err
		}
		if err := rm.PutObject(id, typ, data); err != nil {
			return "", err
		}
		sent++
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
	for _, d := range DiffFlat(base, cur) {
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
	res, err := rm.Land(&LandRequest{Branch: branch, Base: head, Files: files,
		Message: msg, Author: r.authorString(), Meta: meta})
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
	lazy := r.LoadConfig().Lazy != ""
	n := 0
	var fetch func(string) error
	fetch = func(id string) error {
		if r.HasObject(id) {
			return nil
		}
		typ, data, err := rm.GetObject(id)
		if err != nil {
			return err
		}
		kids, err := Children(typ, data)
		if err != nil {
			return err
		}
		if lazy && typ == TypeTree { // file contents are fetched when first read
			t, err := parseTree(data)
			if err != nil {
				return err
			}
			kids = kids[:0]
			for _, e := range t.Entries {
				if e.Mode == "dir" {
					kids = append(kids, e.ID)
				}
			}
		}
		for _, k := range kids {
			if err := fetch(k); err != nil {
				return err
			}
		}
		if _, err := r.PutObject(typ, data); err != nil {
			return err
		}
		n++
		return nil
	}
	if err := fetch(tip); err != nil {
		return "", n, err
	}
	return tip, n, r.WriteRef("remotes/"+rm.Name+"/"+branch, tip)
}

func serve(s *Server, addr string) error {
	if !isLoopback(addr) && s.token == "" && len(s.scopes) == 0 {
		return errors.New("refusing to listen on a non-loopback address without --token (or SPRIG_TOKEN) or --tokens")
	}
	srv := &http.Server{Addr: addr, Handler: s.Handler(), ReadHeaderTimeout: 10 * time.Second}
	what := s.repo.Dir
	if s.upstream != nil {
		what = "cache of " + s.upstream.URL
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
