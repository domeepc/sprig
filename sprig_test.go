package main

import (
	"bytes"
	"compress/zlib"
	"crypto/ed25519"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"
)

// sp runs a sprig command in the current directory and returns its output.
func sp(t *testing.T, args ...string) (string, error) {
	t.Helper()
	var buf bytes.Buffer
	old := stdout
	stdout = &buf
	defer func() { stdout = old }()
	err := run(args[0], args[1:])
	return buf.String(), err
}

func must(t *testing.T, args ...string) string {
	t.Helper()
	out, err := sp(t, args...)
	if err != nil {
		t.Fatalf("sprig %s: %v\n%s", strings.Join(args, " "), err, out)
	}
	return out
}

func write(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func read(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

func newRepo(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	t.Chdir(dir)
	t.Setenv("SPRIG_AUTHOR", "Test <test@example.com>")
	t.Setenv("SPRIG_AGENT", "")
	t.Setenv("SPRIG_MODEL", "")
	t.Setenv("SPRIG_KEY", filepath.Join(dir, "no-key")) // never sign with the developer's real key
	t.Setenv("SPRIG_NO_AUTOGC", "1")                    // the test binary must not spawn itself as `sprig gc`
	must(t, "init")
	return dir
}

func lines(s string) []string { return splitLines([]byte(s)) }

func TestMerge3(t *testing.T) {
	cases := []struct {
		name               string
		base, ours, theirs string
		want               string
		conflicts          int
	}{
		{"disjoint edits", "a\nb\nc\nd\ne\n", "A\nb\nc\nd\ne\n", "a\nb\nc\nd\nE\n", "A\nb\nc\nd\nE\n", 0},
		{"same edit both sides", "a\nb\n", "a\nX\n", "a\nX\n", "a\nX\n", 0},
		{"only theirs changed", "a\nb\n", "a\nb\n", "a\nb\nc\n", "a\nb\nc\n", 0},
		{"conflict", "a\nb\nc\n", "a\nX\nc\n", "a\nY\nc\n", "a\n<<<<<<< ours\nX\n=======\nY\n>>>>>>> theirs\nc\n", 1},
		{"both insert different", "a\n", "a\nx\n", "a\ny\n", "a\n<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\n", 1},
		{"one deletes other untouched", "a\nb\nc\n", "a\nc\n", "a\nb\nc\n", "a\nc\n", 0},
		{"add vs delete elsewhere", "a\nb\nc\nd\ne\nf\n", "a\nb\nc\nd\ne\nf\ng\n", "b\nc\nd\ne\nf\n", "b\nc\nd\ne\nf\ng\n", 0},
		{"empty base both add same", "", "x\n", "x\n", "x\n", 0},
		{"no trailing newline conflict", "a", "b", "c", "<<<<<<< ours\nb\n=======\nc\n>>>>>>> theirs\n", 1},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			res := Merge3(lines(c.base), lines(c.ours), lines(c.theirs), "ours", "theirs")
			if got := strings.Join(res.Lines, ""); got != c.want || res.Conflicts != c.conflicts {
				t.Fatalf("got (%d conflicts)\n%q\nwant (%d)\n%q", res.Conflicts, got, c.conflicts, c.want)
			}
		})
	}
}

func TestDiffRoundTrip(t *testing.T) {
	a := lines("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n")
	b := lines("one\n2\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\n")
	d := UnifiedDiff("f", a, b, 1)
	for _, want := range []string{"--- a/f", "+++ b/f", "-two\n", "+2\n", "+eleven\n", "@@ -1,3 +1,3 @@", "@@ -10,1 +10,2 @@"} {
		if !strings.Contains(d, want) {
			t.Errorf("diff missing %q:\n%s", want, d)
		}
	}
	if UnifiedDiff("f", a, a, 3) != "" {
		t.Error("identical inputs should produce no diff")
	}
}

func TestStoreIntegrity(t *testing.T) {
	dir := newRepo(t)
	r, _ := OpenRepo(dir)
	id, err := r.PutBlob([]byte("hello"))
	if err != nil {
		t.Fatal(err)
	}
	if got, err := r.GetBlob(id); err != nil || string(got) != "hello" {
		t.Fatalf("roundtrip: %q %v", got, err)
	}
	// Overwrite the object with a different valid-looking one: hash check must catch it.
	other, _ := r.PutBlob([]byte("evil"))
	b, _ := os.ReadFile(r.objPath(other))
	os.WriteFile(r.objPath(id), b, 0o644)
	dropCaches() // a cached copy is the verified original; this checks what is on disk
	if _, err := r.GetBlob(id); err == nil || !strings.Contains(err.Error(), "corrupt") {
		t.Fatalf("tampering not detected: %v", err)
	}
}

func TestMaliciousTreeRejected(t *testing.T) {
	for _, name := range []string{"..", ".", "a/b", "", "x\x00y"} {
		tree := Tree{Entries: []Entry{{name, "file", HashObject(TypeBlob, nil)}}}
		data, _ := json.Marshal(tree)
		if _, err := Children(TypeTree, data); err == nil {
			t.Errorf("tree entry name %q accepted", name)
		}
	}
}

func TestCommitLogStatus(t *testing.T) {
	dir := newRepo(t)
	write(t, "a.txt", "hello\n")
	write(t, "sub/b.txt", "world\n")
	if out := must(t, "status"); !strings.Contains(out, "A a.txt") || !strings.Contains(out, "A sub/b.txt") {
		t.Fatalf("status: %s", out)
	}
	must(t, "commit", "-m", "first", "--intent", "bootstrap", "--agent", "claude", "--model", "m1", "--check", "tests=pass")
	if out := must(t, "status"); !strings.Contains(out, "working tree clean") {
		t.Fatalf("status: %s", out)
	}
	if _, err := sp(t, "commit", "-m", "again"); err == nil {
		t.Fatal("empty commit should fail")
	}
	write(t, "a.txt", "hello there\n")
	must(t, "commit", "-m", "second")

	out := must(t, "log", "--json")
	var items []struct {
		ID     string
		Change Change
	}
	if err := json.Unmarshal([]byte(out), &items); err != nil || len(items) != 2 {
		t.Fatalf("log json: %v %s", err, out)
	}
	if items[1].Change.Meta.Intent != "bootstrap" || items[1].Change.Meta.Checks["tests"] != "pass" || items[1].Change.Meta.Agent != "claude" {
		t.Fatalf("metadata lost: %+v", items[1].Change.Meta)
	}
	if out := must(t, "log", "--agent", "claude"); !strings.Contains(out, "first") || strings.Contains(out, "second") {
		t.Fatalf("agent filter: %s", out)
	}
	if out := must(t, "show", "HEAD"); !strings.Contains(out, "-hello\n") || !strings.Contains(out, "+hello there\n") {
		t.Fatalf("show: %s", out)
	}
	if out := must(t, "diff", "HEAD~1", "HEAD"); !strings.Contains(out, "+hello there") {
		t.Fatalf("diff: %s", out)
	}
	_ = dir
}

func TestIgnore(t *testing.T) {
	newRepo(t)
	write(t, ".sprigignore", "*.log\nbuild/\n")
	write(t, "x.log", "noise")
	write(t, "build/out.bin", "noise")
	write(t, "keep.txt", "keep")
	must(t, "commit", "-m", "x")
	out := must(t, "show")
	if strings.Contains(out, "added: x.log") || strings.Contains(out, "build/out.bin") || !strings.Contains(out, "added: keep.txt") {
		t.Fatalf("ignore not honoured:\n%s", out)
	}
}

func TestBranchCheckoutAndExecBit(t *testing.T) {
	newRepo(t)
	write(t, "f.txt", "base\n")
	write(t, "run.sh", "#!/bin/sh\n")
	os.Chmod("run.sh", 0o755)
	must(t, "commit", "-m", "base")
	must(t, "checkout", "-b", "feature")
	write(t, "f.txt", "feature\n")
	write(t, "new.txt", "new\n")
	os.Remove("run.sh")
	must(t, "commit", "-m", "feature work")
	must(t, "checkout", "main")
	if read(t, "f.txt") != "base\n" {
		t.Fatal("f.txt not restored")
	}
	if _, err := os.Stat("new.txt"); err == nil {
		t.Fatal("new.txt should be gone on main")
	}
	if st, err := os.Stat("run.sh"); err != nil || st.Mode()&0o111 == 0 {
		t.Fatal("exec bit lost")
	}
	// dirty tree blocks checkout; -f discards
	write(t, "f.txt", "dirty\n")
	if _, err := sp(t, "checkout", "feature"); err == nil {
		t.Fatal("checkout should refuse a dirty tree")
	}
	must(t, "checkout", "-f", "feature")
	if read(t, "f.txt") != "feature\n" {
		t.Fatal("forced checkout did not discard")
	}
}

func TestMergeConflictResolveUndo(t *testing.T) {
	newRepo(t)
	write(t, "poem.txt", "line1\nline2\nline3\n")
	write(t, "other.txt", "same\n")
	must(t, "commit", "-m", "base")
	must(t, "checkout", "-b", "side")
	write(t, "poem.txt", "line1\nSIDE\nline3\n")
	write(t, "other.txt", "side only\n")
	must(t, "commit", "-m", "side edit")
	must(t, "checkout", "main")
	write(t, "poem.txt", "line1\nMAIN\nline3\n")
	must(t, "commit", "-m", "main edit")

	out, err := sp(t, "merge", "side")
	var ee *exitError
	if !errors.As(err, &ee) || ee.code != 3 {
		t.Fatalf("expected exit 3 for conflicted merge, got %v\n%s", err, out)
	}
	if !strings.Contains(out, "CONFLICT poem.txt") {
		t.Fatalf("merge output: %s", out)
	}
	if got := read(t, "poem.txt"); !strings.Contains(got, "<<<<<<< main") || !strings.Contains(got, ">>>>>>> side") {
		t.Fatalf("no markers: %q", got)
	}
	if read(t, "other.txt") != "side only\n" {
		t.Fatal("non-conflicting file should merge cleanly")
	}
	// the conflict is data: it is part of HEAD and visible without touching files
	if out := must(t, "conflicts"); !strings.HasPrefix(out, "poem.txt") {
		t.Fatalf("conflicts: %s", out)
	}
	if out := must(t, "status"); !strings.Contains(out, "C poem.txt") {
		t.Fatalf("status: %s", out)
	}
	// committing without touching the file keeps the conflict
	must(t, "commit", "--allow-empty", "-m", "wip")
	if out := must(t, "conflicts"); !strings.HasPrefix(out, "poem.txt") {
		t.Fatalf("conflict should carry forward: %q", out)
	}
	must(t, "resolve", "--theirs", "poem.txt")
	if read(t, "poem.txt") != "line1\nSIDE\nline3\n" {
		t.Fatalf("resolve --theirs wrong: %q", read(t, "poem.txt"))
	}
	must(t, "commit", "-m", "resolved")
	if out := must(t, "conflicts"); out != "" {
		t.Fatalf("conflict should be cleared: %q", out)
	}
	// undo the resolve commit, then redo it by undoing the undo
	must(t, "undo")
	if out := must(t, "conflicts"); !strings.HasPrefix(out, "poem.txt") {
		t.Fatalf("undo should bring the conflict back: %q", out)
	}
	if !strings.Contains(read(t, "poem.txt"), "SIDE") {
		t.Fatal("working tree should be restored by undo")
	}
	must(t, "undo")
	if out := must(t, "conflicts"); out != "" {
		t.Fatalf("redo should clear conflicts again: %q", out)
	}
	if out := must(t, "ops"); !strings.Contains(out, "merge side") || !strings.Contains(out, "undo") {
		t.Fatalf("ops: %s", out)
	}
}

func TestFastForwardAndUpToDate(t *testing.T) {
	newRepo(t)
	write(t, "a", "1\n")
	must(t, "commit", "-m", "one")
	must(t, "checkout", "-b", "f")
	write(t, "a", "2\n")
	must(t, "commit", "-m", "two")
	must(t, "checkout", "main")
	if out := must(t, "merge", "f"); !strings.Contains(out, "fast-forward") {
		t.Fatalf("%s", out)
	}
	if read(t, "a") != "2\n" {
		t.Fatal("fast-forward did not update tree")
	}
	if out := must(t, "merge", "f"); !strings.Contains(out, "up to date") {
		t.Fatalf("%s", out)
	}
}

func TestDeleteVsModifyConflict(t *testing.T) {
	newRepo(t)
	write(t, "a", "1\n")
	write(t, "b", "keep\n")
	must(t, "commit", "-m", "base")
	must(t, "checkout", "-b", "del")
	os.Remove("a")
	must(t, "commit", "-m", "delete a")
	must(t, "checkout", "main")
	write(t, "a", "modified\n")
	must(t, "commit", "-m", "modify a")
	if _, err := sp(t, "merge", "del"); err == nil {
		t.Fatal("expected conflict")
	}
	if read(t, "a") != "modified\n" {
		t.Fatal("modified side should be kept")
	}
	must(t, "resolve", "--theirs", "a") // theirs deleted it
	if _, err := os.Stat("a"); err == nil {
		t.Fatal("a should be removed")
	}
}

func TestAudit(t *testing.T) {
	newRepo(t)
	write(t, "a", "1\n")
	must(t, "commit", "-m", "human change")
	write(t, "a", "2\n")
	must(t, "commit", "-m", "agent unreviewed", "--agent", "bot")
	write(t, "a", "3\n")
	must(t, "commit", "-m", "agent reviewed", "--agent", "bot", "--check", "review=approved")
	out, err := sp(t, "audit", "--json")
	var ee *exitError
	if !errors.As(err, &ee) || ee.code != 1 {
		t.Fatalf("audit should exit 1, got %v", err)
	}
	var found []map[string]string
	json.Unmarshal([]byte(out), &found)
	if len(found) != 1 || found[0]["message"] != "agent unreviewed" {
		t.Fatalf("audit findings: %s", out)
	}
}

// ---------------------------------------------------------------------------
// Remote

func TestPushPullAndServerPolicy(t *testing.T) {
	// bare server repo
	serverDir := t.TempDir()
	srvRepo, err := InitRepo(serverDir, true, "main")
	if err != nil {
		t.Fatal(err)
	}
	ts := httptest.NewServer(NewServer(srvRepo, "secret").Handler())
	defer ts.Close()
	t.Setenv("SPRIG_TOKEN", "secret")

	// client A
	a := newRepo(t)
	must(t, "remote", "add", "origin", ts.URL)
	write(t, "a.txt", "from A\n")
	write(t, "d/x.txt", "nested\n")
	must(t, "commit", "-m", "A1")
	if out := must(t, "push"); !strings.Contains(out, "pushed main") {
		t.Fatalf("%s", out)
	}
	if out := must(t, "push"); !strings.Contains(out, "up to date") {
		t.Fatalf("%s", out)
	}
	if id, ok := srvRepo.ReadRef("heads/main"); !ok || id == "" {
		t.Fatal("server ref missing")
	}

	// server-side agent API: read a file without any clone
	rm, _ := func() (*Remote, error) { r, _ := OpenRepo(a); return r.Remote("origin") }()
	data, _, err := rm.do("GET", "/v1/file?ref=main&path=d/x.txt", nil, nil)
	if err != nil || string(data) != "nested\n" {
		t.Fatalf("api file: %q %v", data, err)
	}
	logData, _, err := rm.do("GET", "/v1/log?ref=main&limit=5", nil, nil)
	if err != nil || !strings.Contains(string(logData), "A1") {
		t.Fatalf("api log: %s %v", logData, err)
	}

	// wrong token is rejected
	bad := *rm
	bad.Token = "nope"
	if _, _, err := bad.do("GET", "/v1/refs", nil, nil); err == nil {
		t.Fatal("bad token accepted")
	}

	// client B pulls (fast-forward into an unborn-compatible clone)
	b := t.TempDir()
	t.Chdir(b)
	must(t, "init")
	must(t, "remote", "add", "origin", ts.URL)
	must(t, "pull") // into an unborn branch: adopts the remote history
	if read(t, "a.txt") != "from A\n" || read(t, "d/x.txt") != "nested\n" {
		t.Fatal("pull did not materialize files")
	}
	write(t, "a.txt", "from B\n")
	must(t, "commit", "-m", "B1")
	must(t, "push")

	// A is now behind; A edits independently -> push rejected (non-fast-forward)
	t.Chdir(a)
	write(t, "other.txt", "A side\n")
	must(t, "commit", "-m", "A2")
	if _, err := sp(t, "push"); err == nil || !strings.Contains(err.Error(), "non-fast-forward") {
		t.Fatalf("expected non-fast-forward rejection, got %v", err)
	}
	// pull merges B's work, then push succeeds
	must(t, "pull")
	if read(t, "a.txt") != "from B\n" || read(t, "other.txt") != "A side\n" {
		t.Fatal("pull did not merge both sides")
	}
	must(t, "push")

	// a conflicted tip is refused by the server's policy gate
	t.Chdir(b)
	must(t, "pull")
	write(t, "a.txt", "B conflicting\n")
	must(t, "commit", "-m", "B2")
	t.Chdir(a)
	write(t, "a.txt", "A conflicting\n")
	must(t, "commit", "-m", "A3")
	must(t, "push") // A wins the race
	t.Chdir(b)
	if _, err := sp(t, "fetch"); err != nil {
		t.Fatal(err)
	}
	if _, err := sp(t, "merge", "origin/main"); err == nil {
		t.Fatal("expected conflict")
	}
	if out, err := sp(t, "push"); err == nil || !strings.Contains(err.Error(), "unresolved conflict") {
		t.Fatalf("server should refuse a conflicted tip: %v %s", err, out)
	}
	must(t, "resolve", "--ours", "a.txt")
	must(t, "commit", "-m", "resolved")
	must(t, "push")
}

func TestServerRejectsBadObjects(t *testing.T) {
	srvRepo, _ := InitRepo(t.TempDir(), true, "main")
	ts := httptest.NewServer(NewServer(srvRepo, "").Handler())
	defer ts.Close()
	rm := &Remote{URL: ts.URL, hc: ts.Client()}

	blob := []byte("data")
	good := HashObject(TypeBlob, blob)
	if err := rm.PutObject(good, TypeBlob, []byte("different")); err == nil {
		t.Error("hash mismatch accepted")
	}
	if err := rm.PutObject(good, TypeBlob, blob); err != nil {
		t.Fatal(err)
	}
	// tree referencing a blob the server does not have
	tree, _ := json.Marshal(Tree{Entries: []Entry{{"f", "file", HashObject(TypeBlob, []byte("absent"))}}})
	if err := rm.PutObject(HashObject(TypeTree, tree), TypeTree, tree); err == nil || !strings.Contains(err.Error(), "missing dependency") {
		t.Errorf("tree with missing child accepted: %v", err)
	}
	// ref names that try to escape
	for _, name := range []string{"../../etc/passwd", "heads/.hidden", "a//b"} {
		body, _ := json.Marshal(refUpdate{})
		if _, _, err := rm.do("POST", "/v1/refs/"+name, body, nil); err == nil {
			t.Errorf("ref name %q accepted", name)
		}
	}
	if err := rm.UpdateRef("heads/x", "", good, false); err == nil {
		t.Error("ref pointing at a blob accepted")
	}
}

// ---------------------------------------------------------------------------
// git import

func git(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	cmd.Env = append(os.Environ(),
		"GIT_AUTHOR_NAME=Gina", "GIT_AUTHOR_EMAIL=gina@example.com",
		"GIT_COMMITTER_NAME=Gina", "GIT_COMMITTER_EMAIL=gina@example.com",
		"GIT_AUTHOR_DATE=2024-03-01T12:00:00+0100", "GIT_COMMITTER_DATE=2024-03-01T12:00:00+0100")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v\n%s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

func TestImportGit(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git not installed")
	}
	g := t.TempDir()
	git(t, g, "init", "-q", "-b", "main")
	write(t, filepath.Join(g, "README.md"), "# hi\n")
	write(t, filepath.Join(g, "src/main.go"), "package main\n")
	write(t, filepath.Join(g, "run.sh"), "#!/bin/sh\n")
	os.Chmod(filepath.Join(g, "run.sh"), 0o755)
	os.Symlink("README.md", filepath.Join(g, "link"))
	git(t, g, "add", ".")
	git(t, g, "commit", "-q", "-m", "initial\n\nbody text")
	git(t, g, "checkout", "-q", "-b", "feature")
	write(t, filepath.Join(g, "feature.txt"), "feat\n")
	git(t, g, "add", ".")
	git(t, g, "commit", "-q", "-m", "feature")
	git(t, g, "checkout", "-q", "main")
	write(t, filepath.Join(g, "src/main.go"), "package main\n// main\n")
	git(t, g, "commit", "-q", "-am", "main change")
	git(t, g, "merge", "-q", "--no-ff", "-m", "merge feature", "feature")
	git(t, g, "rm", "-q", "README.md")
	git(t, g, "commit", "-q", "-m", "drop readme")

	newRepo(t)
	out := must(t, "import-git", g)
	if !strings.Contains(out, "imported 5 changes") {
		t.Fatalf("import output: %s", out)
	}
	// HEAD adopted and checked out
	if read(t, "feature.txt") != "feat\n" || read(t, "src/main.go") != "package main\n// main\n" {
		t.Fatal("working tree not materialized")
	}
	if _, err := os.Stat("README.md"); err == nil {
		t.Fatal("README.md should be gone at tip")
	}
	if st, err := os.Stat("run.sh"); err != nil || st.Mode()&0o111 == 0 {
		t.Fatal("exec bit not preserved")
	}
	if target, err := os.Readlink("link"); err != nil || target != "README.md" {
		t.Fatalf("symlink not preserved: %q %v", target, err)
	}
	// history shape: merge change has two parents; metadata kept
	logOut := must(t, "log", "--json", "-n", "10")
	var items []struct {
		ID     string
		Change Change
	}
	json.Unmarshal([]byte(logOut), &items)
	if len(items) != 5 {
		t.Fatalf("log has %d changes", len(items))
	}
	merges := 0
	for _, it := range items {
		if len(it.Change.Parents) == 2 {
			merges++
		}
		if it.Change.Author != "Gina <gina@example.com>" || it.Change.Meta.GitSHA == "" || it.Change.Time != 1709290800 {
			t.Fatalf("metadata not preserved: %+v", it.Change)
		}
	}
	if merges != 1 {
		t.Fatalf("expected 1 merge change, got %d", merges)
	}
	first := items[len(items)-1].Change
	if first.Message != "initial\n\nbody text" {
		t.Fatalf("message mangled: %q", first.Message)
	}
	// other branch imported too, and re-import is idempotent
	if out := must(t, "branch"); !strings.Contains(out, "feature") {
		t.Fatalf("branches: %s", out)
	}
	before := must(t, "log", "--json", "-n", "10")
	must(t, "import-git", g)
	if after := must(t, "log", "--json", "-n", "10"); after != before {
		t.Fatal("re-import changed history (should be deterministic)")
	}
	// the merged tree must equal what git has at HEAD
	gitFiles := strings.Fields(git(t, g, "ls-files"))
	var ours []string
	r, _ := FindRepo()
	_, head := r.Head()
	flat, _ := r.ChangeFlat(head)
	for p := range flat {
		ours = append(ours, p)
	}
	if len(ours) != len(gitFiles) {
		t.Fatalf("file sets differ: git=%v sprig=%v", gitFiles, ours)
	}
}

func TestExportGitRoundTrip(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git not installed")
	}
	// "github" is a bare repo with one commit; clone is the local transport.
	hub := t.TempDir()
	git(t, hub, "init", "-q", "--bare", "-b", "main")
	seed := t.TempDir()
	git(t, seed, "init", "-q", "-b", "main")
	write(t, filepath.Join(seed, "a.txt"), "one\n")
	git(t, seed, "add", ".")
	git(t, seed, "commit", "-q", "-m", "seed")
	git(t, seed, "push", "-q", hub, "main")
	seedSHA := git(t, seed, "rev-parse", "HEAD")
	clone := filepath.Join(t.TempDir(), "clone.git")
	git(t, ".", "clone", "-q", "--bare", hub, clone)

	newRepo(t)
	must(t, "import-git", clone)
	write(t, "a.txt", "one\ntwo\n")
	write(t, "dir/b.sh", "#!/bin/sh\n")
	os.Chmod("dir/b.sh", 0o755)
	must(t, "commit", "-m", "from sprig", "--intent", "add two", "--agent", "claude", "--check", "tests=pass")
	out := must(t, "export-git", clone)
	if !strings.Contains(out, "wrote 1 commit(s)") {
		t.Fatalf("export output: %s", out)
	}
	tip := git(t, clone, "rev-parse", "main")
	if p := git(t, clone, "rev-parse", "main^"); p != seedSHA {
		t.Fatalf("imported history not reused: parent %s, want %s", p, seedSHA)
	}
	if got := git(t, clone, "show", "main:a.txt"); got != "one\ntwo" {
		t.Fatalf("a.txt = %q", got)
	}
	if ls := git(t, clone, "ls-tree", "-r", "main", "dir/b.sh"); !strings.HasPrefix(ls, "100755") {
		t.Fatalf("exec bit lost: %s", ls)
	}
	msg := git(t, clone, "log", "-1", "--format=%B", "main")
	for _, want := range []string{"Sprig-Intent: add two", "Sprig-Agent: claude", "Sprig-Check: tests=pass", "Sprig-Change: "} {
		if !strings.Contains(msg, want) {
			t.Fatalf("message lacks %q:\n%s", want, msg)
		}
	}
	if out := must(t, "export-git", clone); !strings.Contains(out, "wrote 0 commit(s)") {
		t.Fatalf("re-export rewrote known commits: %s", out)
	}
	if again := git(t, clone, "rev-parse", "main"); again != tip {
		t.Fatal("re-export changed the commit (should be deterministic)")
	}
	if refs := git(t, clone, "for-each-ref", exportRef); refs != "" {
		t.Fatalf("scratch ref left behind: %s", refs)
	}
	git(t, clone, "push", "-q", "origin", "main")

	// someone else builds on it in git; importing again fast-forwards
	other := t.TempDir()
	git(t, other, "clone", "-q", hub, ".")
	write(t, filepath.Join(other, "c.txt"), "c\n")
	git(t, other, "add", ".")
	git(t, other, "commit", "-q", "-m", "from git")
	git(t, other, "push", "-q", "origin", "main")
	git(t, clone, "fetch", "-q", "origin", "+refs/heads/*:refs/heads/*")
	r, _ := FindRepo()
	_, mine := r.Head()
	out = must(t, "import-git", clone)
	if strings.Contains(out, "warning") {
		t.Fatalf("import did not fast-forward: %s", out)
	}
	newTip, _ := r.ReadRef("heads/main")
	c, _ := r.GetChange(newTip)
	if len(c.Parents) != 1 || c.Parents[0] != mine {
		t.Fatalf("exported commit not mapped back to its change: parents %v, want %s", c.Parents, mine)
	}
	if read(t, "c.txt") != "c\n" {
		t.Fatal("working tree not moved with the fast-forwarded branch")
	}

	// a conflicted tip is not exported
	must(t, "checkout", "-b", "x")
	r.WriteRef("heads/x", must2(t, r, &Change{Parents: []string{newTip}, Tree: c.Tree, Author: "T <t@x>", Message: "bad",
		Conflicts: []Conflict{{Path: "a.txt"}}}))
	if out := must(t, "export-git", clone); !strings.Contains(out, `branch "x" has 1 unresolved`) {
		t.Fatalf("conflicted branch exported: %s", out)
	}
}

func must2(t *testing.T, r *Repo, c *Change) string {
	t.Helper()
	id, err := r.PutChange(c)
	if err != nil {
		t.Fatal(err)
	}
	return id
}

func TestLand(t *testing.T) {
	srvRepo, err := InitRepo(t.TempDir(), true, "main")
	if err != nil {
		t.Fatal(err)
	}
	ts := httptest.NewServer(NewServer(srvRepo, "").Handler())
	defer ts.Close()

	a := newRepo(t)
	must(t, "remote", "add", "origin", ts.URL)
	write(t, "a.txt", "1\n2\n3\n")
	write(t, "b.txt", "b\n")
	must(t, "commit", "-m", "seed")
	must(t, "push")
	b := t.TempDir()
	t.Chdir(b)
	must(t, "init")
	must(t, "remote", "add", "origin", ts.URL)
	must(t, "pull")

	// A lands first; B, still on the old base, lands an unrelated edit and is rebased
	t.Chdir(a)
	write(t, "a.txt", "1\nA\n3\n")
	must(t, "land", "-m", "A edit", "--agent", "claude", "--check", "tests=pass")
	aTip, _ := srvRepo.ReadRef("heads/main")
	t.Chdir(b)
	write(t, "b.txt", "B\n")
	if out := must(t, "land", "-m", "B edit"); !strings.Contains(out, "landed on "+short(aTip)) {
		t.Fatalf("B not rebased onto A: %s", out)
	}
	if read(t, "a.txt") != "1\nA\n3\n" || read(t, "b.txt") != "B\n" {
		t.Fatal("B's working tree not moved to the landed change")
	}
	if out := must(t, "status"); strings.Contains(out, "a.txt") || strings.Contains(out, "b.txt") {
		t.Fatalf("working tree dirty after land: %s", out)
	}
	r, _ := FindRepo()
	tip, _ := srvRepo.ReadRef("heads/main")
	if local, _ := r.ReadRef("heads/main"); local != tip {
		t.Fatal("local branch not at landed change")
	}
	c, _ := srvRepo.GetChange(aTip)
	if c.Meta.Agent != "claude" || c.Meta.Checks["tests"] != "pass" {
		t.Fatalf("provenance lost: %+v", c.Meta)
	}

	// A is stale and edits b.txt, which B changed since A's base -> refused
	t.Chdir(a)
	write(t, "b.txt", "A wants b\n")
	if _, err := sp(t, "land", "-m", "clash"); err == nil || !strings.Contains(err.Error(), "b.txt") {
		t.Fatalf("expected conflict on b.txt, got %v", err)
	}
	if now, _ := srvRepo.ReadRef("heads/main"); now != tip {
		t.Fatal("conflicting land moved the branch")
	}

	// clone-free agents: 20 concurrent landings from the same stale base all land
	rm := &Remote{URL: ts.URL, hc: ts.Client()}
	errs := make(chan error, 20)
	for i := range 20 {
		go func() {
			_, err := rm.Land(&LandRequest{Branch: "main", Base: aTip, Message: fmt.Sprint("agent ", i),
				Files: map[string]*LandFile{fmt.Sprintf("agents/%d.txt", i): {Mode: "file", Data: []byte("hi\n")}}})
			errs <- err
		}()
	}
	for range 20 {
		if err := <-errs; err != nil {
			t.Fatal(err)
		}
	}
	end, _ := srvRepo.ReadRef("heads/main")
	flat, _ := srvRepo.ChangeFlat(end)
	if len(flat) != 22 {
		t.Fatalf("expected 22 files after concurrent landings, got %d", len(flat))
	}
	n := 0
	srvRepo.Walk(end, func(id string, c *Change) bool { n++; return len(c.Parents) <= 1 })
	if n != 23 { // seed, A, B, 20 agents; all linear
		t.Fatalf("history has %d changes, want 23 linear", n)
	}
}

// seedServer starts a server over a bare repo holding a.txt and d/x.txt on main.
func seedServer(t *testing.T, s *Server) (string, *Repo) {
	t.Helper()
	ts := httptest.NewServer(s.Handler())
	t.Cleanup(ts.Close)
	t.Cleanup(s.Stop) // runs first: ends long-polls so Close needn't wait
	newRepo(t)
	must(t, "remote", "add", "origin", ts.URL)
	write(t, "a.txt", "a\n")
	write(t, "d/x.txt", "x\n")
	must(t, "commit", "-m", "seed")
	must(t, "push")
	return ts.URL, s.repo
}

func land(t *testing.T, url, token string, files map[string]*LandFile) error {
	t.Helper()
	rm := newRemote("t", url)
	rm.Token = token
	refs, err := rm.Refs()
	if err != nil {
		return err
	}
	_, err = rm.Land(&LandRequest{Branch: "main", Base: refs["heads/main"], Message: "m", Files: files})
	return err
}

func TestLandPaths(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	url, repo := seedServer(t, NewServer(srv, ""))
	f := &LandFile{Mode: "file", Data: []byte("y\n")}
	if err := land(t, url, "", map[string]*LandFile{"a.txt/y": f}); err == nil {
		t.Fatal("file used as a directory accepted")
	}
	if err := land(t, url, "", map[string]*LandFile{"d": f}); err == nil || !strings.Contains(err.Error(), "directory") {
		t.Fatalf("directory overwritten by a file: %v", err)
	}
	if err := land(t, url, "", map[string]*LandFile{"d/x.txt": nil, "e/f/g.txt": f}); err != nil {
		t.Fatal(err)
	}
	tip, _ := repo.ReadRef("heads/main")
	flat, _ := repo.ChangeFlat(tip)
	if _, ok := flat["d/x.txt"]; ok || len(flat) != 2 || flat["e/f/g.txt"].ID == "" {
		t.Fatalf("tree after land: %v", flat)
	}
	c, _ := repo.GetChange(tip)
	if root, _ := repo.GetTree(c.Tree); len(root.Entries) != 2 { // empty d/ dropped
		t.Fatalf("root entries: %+v", root.Entries)
	}
}

func TestScopedTokens(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	s := NewServer(srv, "full-access-token")
	s.scopes = map[string]Scope{"docs-only-token-123": {Write: []string{"d"}},
		"read-d-only-token-12": {Read: []string{"d"}}}
	t.Setenv("SPRIG_TOKEN", "full-access-token")
	url, _ := seedServer(t, s)
	f := &LandFile{Mode: "file", Data: []byte("new\n")}
	if err := land(t, url, "docs-only-token-123", map[string]*LandFile{"d/x.txt": f}); err != nil {
		t.Fatalf("in-scope land refused: %v", err)
	}
	if err := land(t, url, "docs-only-token-123", map[string]*LandFile{"a.txt": f}); err == nil || !strings.Contains(err.Error(), "403") {
		t.Fatalf("out-of-scope land: %v", err)
	}
	if err := land(t, url, "docs-only-token-123", map[string]*LandFile{"dx.txt": f}); err == nil {
		t.Fatal("prefix matched a sibling name")
	}
	rm := newRemote("t", url)
	rm.Token = "docs-only-token-123"
	if err := rm.UpdateRef("heads/main", "", "", true); err == nil || !strings.Contains(err.Error(), "403") {
		t.Fatalf("scoped token moved a ref: %v", err)
	}
	if err := land(t, url, "wrong", map[string]*LandFile{"d/x.txt": f}); err == nil || !strings.Contains(err.Error(), "401") {
		t.Fatalf("bad token: %v", err)
	}
	// read scope: only d/ is visible, and raw objects are off limits
	ro := newRemote("t", url)
	ro.Token = "read-d-only-token-12"
	if data, _, err := ro.do("GET", "/v1/file?path=d/x.txt", nil, nil); err != nil || string(data) != "new\n" {
		t.Fatalf("in-scope read: %q %v", data, err)
	}
	if _, _, err := ro.do("GET", "/v1/file?path=a.txt", nil, nil); err == nil || !strings.Contains(err.Error(), "403") {
		t.Fatalf("out-of-scope read: %v", err)
	}
	if data, _, err := ro.do("GET", "/v1/ls", nil, nil); err != nil || strings.Contains(string(data), "a.txt") || !strings.Contains(string(data), `"d"`) {
		t.Fatalf("ls not filtered: %s %v", data, err)
	}
	refs, _ := ro.Refs()
	if _, _, err := ro.GetObject(refs["heads/main"]); err != nil {
		t.Fatalf("projected tip not fetchable: %v", err)
	}
	realTip, _ := s.repo.ReadRef("heads/main")
	for _, id := range []string{realTip, HashObject(TypeBlob, []byte("a\n"))} {
		if _, _, err := ro.GetObject(id); err == nil || !strings.Contains(err.Error(), "403") {
			t.Fatalf("read-limited token fetched out-of-scope object %s: %v", short(id), err)
		}
		if err := ro.GetObjects([]string{id}, func(string, object) error { return nil }); err == nil {
			t.Fatal("read-limited token batch-fetched an out-of-scope object")
		}
	}
	if err := land(t, url, "read-d-only-token-12", map[string]*LandFile{"d/x.txt": f}); err == nil {
		t.Fatal("read-only token landed")
	}
}

func TestTwoServersOneRepo(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	url1, repo := seedServer(t, NewServer(srv, ""))
	ts2 := httptest.NewServer(NewServer(&Repo{Dir: srv.Dir, Bare: true}, "").Handler())
	defer ts2.Close()
	errs := make(chan error, 20)
	for i := range 20 {
		url := url1
		if i%2 == 1 {
			url = ts2.URL
		}
		go func() {
			errs <- land(t, url, "", map[string]*LandFile{fmt.Sprintf("n/%d", i): {Mode: "file", Data: []byte{byte(i)}}})
		}()
	}
	for range 20 {
		if err := <-errs; err != nil {
			t.Fatal(err)
		}
	}
	tip, _ := repo.ReadRef("heads/main")
	if flat, _ := repo.ChangeFlat(tip); len(flat) != 22 {
		t.Fatalf("lost landings across servers: %d files", len(flat))
	}
}

func TestCacheServer(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	url, repo := seedServer(t, NewServer(srv, ""))
	cacheRepo, _ := InitRepo(t.TempDir(), true, "main")
	cache := NewServer(cacheRepo, "")
	cache.upstream = newRemote("upstream", url)
	cts := httptest.NewServer(cache.Handler())
	defer cts.Close()
	defer cache.Stop()

	t.Chdir(t.TempDir())
	must(t, "clone", cts.URL, "w")
	t.Chdir("w")
	if read(t, "d/x.txt") != "x\n" {
		t.Fatal("clone through cache failed")
	}
	tip, _ := repo.ReadRef("heads/main")
	if !cacheRepo.HasObject(tip) {
		t.Fatal("cache did not keep the change")
	}
	write(t, "a.txt", "via cache\n")
	must(t, "land", "-m", "through cache")
	newTip, _ := repo.ReadRef("heads/main")
	if flat, _ := repo.ChangeFlat(newTip); flat["a.txt"].ID != HashObject(TypeBlob, []byte("via cache\n")) {
		t.Fatal("land through cache did not reach upstream")
	}
}

func TestLazySparseClone(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	url, repo := seedServer(t, NewServer(srv, ""))
	t.Chdir(t.TempDir())
	must(t, "clone", "--path", "d", url, "w")
	t.Chdir("w")
	if _, err := os.Stat("a.txt"); err == nil {
		t.Fatal("file outside --path was checked out")
	}
	r, _ := FindRepo()
	if r.HasObject(HashObject(TypeBlob, []byte("a\n"))) {
		t.Fatal("blob outside the view was downloaded")
	}
	if out := must(t, "status"); strings.Contains(out, "a.txt") {
		t.Fatalf("file outside view shows as changed: %s", out)
	}
	// reading history outside the view fetches on demand
	if out := must(t, "show", "HEAD"); !strings.Contains(out, "+a") {
		t.Fatalf("show did not load a.txt lazily: %s", out)
	}
	write(t, "d/x.txt", "x2\n")
	must(t, "land", "-m", "sparse edit")
	tip, _ := repo.ReadRef("heads/main")
	flat, _ := repo.ChangeFlat(tip)
	if len(flat) != 2 || flat["d/x.txt"].ID != HashObject(TypeBlob, []byte("x2\n")) {
		t.Fatalf("server tree after sparse land: %v", flat)
	}
	// commit + push from a lazy clone works without downloading everything
	write(t, "d/y.txt", "y\n")
	must(t, "commit", "-m", "local")
	must(t, "push")
	tip, _ = repo.ReadRef("heads/main")
	if flat, _ := repo.ChangeFlat(tip); flat["a.txt"].ID == "" || flat["d/y.txt"].ID == "" {
		t.Fatalf("push from sparse clone lost files: %v", flat)
	}
}

func TestImportGitDiverged(t *testing.T) {
	if _, err := exec.LookPath("git"); err != nil {
		t.Skip("git not installed")
	}
	g := t.TempDir()
	git(t, g, "init", "-q", "-b", "main")
	write(t, filepath.Join(g, "a.txt"), "1\n2\n")
	git(t, g, "add", ".")
	git(t, g, "commit", "-q", "-m", "seed")
	newRepo(t)
	must(t, "import-git", g)
	write(t, "b.txt", "sprig side\n")
	must(t, "commit", "-m", "sprig work")
	write(t, filepath.Join(g, "a.txt"), "1\n2\ngit side\n")
	git(t, g, "commit", "-q", "-am", "git work")
	if out := must(t, "import-git", g); !strings.Contains(out, "git/main") {
		t.Fatalf("diverged branch not kept: %s", out)
	}
	must(t, "merge", "git/main")
	if read(t, "a.txt") != "1\n2\ngit side\n" || read(t, "b.txt") != "sprig side\n" {
		t.Fatal("merging the diverged git branch lost a side")
	}
	write(t, "c.txt", "more\n")
	must(t, "commit", "-m", "ahead of git")
	if out := must(t, "import-git", g); strings.Contains(out, "warning") {
		t.Fatalf("branch that is only ahead of git reported as diverged: %s", out)
	}
}

func TestRenames(t *testing.T) {
	newRepo(t)
	write(t, "old.txt", "same content\n")
	must(t, "commit", "-m", "one")
	os.Rename("old.txt", "new.txt")
	if out := must(t, "status"); !strings.Contains(out, "R old.txt -> new.txt") {
		t.Fatalf("status: %s", out)
	}
	must(t, "commit", "-m", "rename")
	if out := must(t, "show"); !strings.Contains(out, "renamed: old.txt -> new.txt") {
		t.Fatalf("show: %s", out)
	}
	write(t, "big.go", "package x\n\nfunc A() {}\nfunc B() {}\nfunc C() {}\nfunc D() {}\n")
	must(t, "commit", "-m", "add big")
	os.Remove("big.go")
	write(t, "moved.go", "package x\n\nfunc A() {}\nfunc B() {}\nfunc C() {}\nfunc E() {}\n")
	if out := must(t, "status"); !strings.Contains(out, "R big.go -> moved.go (edited)") {
		t.Fatalf("rename with edit not detected: %s", out)
	}
	must(t, "commit", "-m", "move and edit")
	if out := must(t, "show"); !strings.Contains(out, "renamed: big.go -> moved.go") || !strings.Contains(out, "+func E() {}") {
		t.Fatalf("show of rename+edit: %s", out)
	}
}

func TestCrissCrossMerge(t *testing.T) {
	newRepo(t)
	write(t, "f", "1\n2\n3\n")
	must(t, "commit", "-m", "base")
	must(t, "checkout", "-b", "y")
	write(t, "f", "1\n2\ny\n")
	must(t, "commit", "-m", "y1")
	must(t, "checkout", "main")
	write(t, "f", "x\n2\n3\n")
	must(t, "commit", "-m", "x1")
	x1, _ := FindRepo()
	_, x1id := x1.Head()
	must(t, "merge", "--no-ff", "y") // main: x,2,y
	must(t, "checkout", "y")
	must(t, "merge", "--no-ff", x1id) // y: x,2,y, via the other parent order
	write(t, "f", "x\n2\nyy\n")
	must(t, "commit", "-m", "y3")
	must(t, "checkout", "main")
	write(t, "f", "xx\n2\ny\n")
	must(t, "commit", "-m", "x3")
	// either single base conflicts here; the merged virtual base does not
	if out, err := sp(t, "merge", "y"); err != nil {
		t.Fatalf("criss-cross merge conflicted: %v\n%s", err, out)
	}
	if got := read(t, "f"); got != "xx\n2\nyy\n" {
		t.Fatalf("merged f = %q", got)
	}
}

func TestSparseWiden(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	url, _ := seedServer(t, NewServer(srv, ""))
	t.Chdir(t.TempDir())
	must(t, "clone", "--path", "d", url, "w")
	t.Chdir("w")
	must(t, "sparse", ".")
	if read(t, "a.txt") != "a\n" {
		t.Fatal("widening did not check out a.txt")
	}
	must(t, "sparse", "d")
	if _, err := os.Stat("a.txt"); err == nil {
		t.Fatal("narrowing did not remove a.txt")
	}
	if out := must(t, "status"); !strings.Contains(out, "clean") {
		t.Fatalf("status after narrowing: %s", out)
	}
}

func TestBatchedTransfer(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	var single, batch int
	h := NewServer(srv, "").Handler()
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		switch {
		case strings.HasPrefix(req.URL.Path, "/v1/object/"):
			single++
		case strings.HasPrefix(req.URL.Path, "/v1/objects/get"), strings.HasPrefix(req.URL.Path, "/v1/objects/put"):
			batch++
		}
		h.ServeHTTP(w, req)
	}))
	defer ts.Close()
	newRepo(t)
	must(t, "remote", "add", "origin", ts.URL)
	for i := range 60 {
		write(t, fmt.Sprintf("d%d/f%d.txt", i%5, i), fmt.Sprint(i))
	}
	must(t, "commit", "-m", "many")
	must(t, "push")
	b := t.TempDir()
	t.Chdir(b)
	must(t, "init")
	must(t, "remote", "add", "origin", ts.URL)
	must(t, "pull")
	if read(t, "d3/f8.txt") != "8" {
		t.Fatal("pull incomplete")
	}
	if single != 0 || batch > 5 { // 1 put + one get per tree level
		t.Fatalf("requests: %d single, %d batched", single, batch)
	}
}

func TestSignedChanges(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	url, repo := seedServer(t, NewServer(srv, ""))
	out := must(t, "keygen")
	pub := strings.Fields(out[strings.Index(out, "public key: "):])[2]
	must(t, "trust", pub, "me")
	write(t, "a.txt", "signed\n")
	t.Setenv("SPRIG_AGENT", "claude")
	must(t, "commit", "-m", "signed commit", "--check", "review=approved")
	if out := must(t, "show"); !strings.Contains(out, "Signed: me (trusted)") {
		t.Fatalf("show: %s", out)
	}
	must(t, "audit", "--signed")

	// a signed landing verifies on the server's change
	write(t, "d/x.txt", "landed\n")
	must(t, "push")
	write(t, "d/x.txt", "landed again\n")
	must(t, "land", "-m", "signed land", "--check", "review=approved")
	tip, _ := repo.ReadRef("heads/main")
	c, _ := repo.GetChange(tip)
	if key, err := repo.VerifyChange(c); err != nil || key != pub {
		t.Fatalf("landed change: key %q err %v", key, err)
	}
	must(t, "audit", "--signed")

	// tampering is caught: same signature, different message or extra path
	forged := *c
	forged.Message = "not what was signed"
	if _, err := repo.VerifyChange(&forged); err == nil {
		t.Fatal("altered message verified")
	}
	flat, _ := repo.Flatten(c.Tree)
	flat["sneaky.txt"] = flat["a.txt"]
	forged = *c
	forged.Tree, _ = repo.BuildTree(flat)
	if _, err := repo.VerifyChange(&forged); err == nil || !strings.Contains(err.Error(), "unsigned path") {
		t.Fatalf("extra path verified: %v", err)
	}
	// unsigned agent change fails audit --signed
	os.Setenv("SPRIG_KEY", filepath.Join(t.TempDir(), "none"))
	write(t, "a.txt", "unsigned\n")
	must(t, "commit", "-m", "unsigned", "--check", "review=approved")
	if out, err := sp(t, "audit", "--signed"); err == nil || !strings.Contains(out, "unsigned") {
		t.Fatalf("unsigned change passed audit: %v %s", err, out)
	}
	_ = url
}

func TestCommitPaths(t *testing.T) {
	newRepo(t)
	write(t, "a.txt", "a\n")
	write(t, "d/b.txt", "b\n")
	must(t, "commit", "-m", "only d", "d")
	r, _ := FindRepo()
	_, head := r.Head()
	if flat, _ := r.ChangeFlat(head); len(flat) != 1 || flat["d/b.txt"].ID == "" {
		t.Fatalf("path commit took %v", flat)
	}
	os.Remove("d/b.txt")
	write(t, "a.txt", "a2\n")
	must(t, "commit", "-m", "drop b", "d/b.txt")
	_, head = r.Head()
	if flat, _ := r.ChangeFlat(head); len(flat) != 0 {
		t.Fatalf("deletion not committed alone: %v", flat)
	}
	if out := must(t, "status"); !strings.Contains(out, "A a.txt") {
		t.Fatalf("a.txt should still be uncommitted: %s", out)
	}
}

func TestCheckoutNewBranchKeepsWork(t *testing.T) {
	newRepo(t)
	write(t, "a.txt", "a\n")
	must(t, "commit", "-m", "one")
	write(t, "a.txt", "edited\n")
	must(t, "checkout", "-b", "topic")
	if read(t, "a.txt") != "edited\n" {
		t.Fatal("uncommitted edit lost")
	}
	if out := must(t, "status"); !strings.Contains(out, "On branch topic") || !strings.Contains(out, "M a.txt") {
		t.Fatalf("status: %s", out)
	}
}

func TestCommitHunks(t *testing.T) {
	newRepo(t)
	var lines []string
	for i := range 20 {
		lines = append(lines, fmt.Sprint(i))
	}
	orig := strings.Join(lines, "\n") + "\n"
	write(t, "f.txt", orig)
	must(t, "commit", "-m", "base")
	edited := strings.Replace(strings.Replace(orig, "1\n", "ONE\n", 1), "18\n", "EIGHTEEN\n", 1)
	write(t, "f.txt", edited)
	if out := must(t, "diff"); !strings.Contains(out, "hunk 1") || !strings.Contains(out, "hunk 2") {
		t.Fatalf("diff should show two numbered hunks: %s", out)
	}
	must(t, "commit", "-m", "first hunk only", "--hunk", "f.txt:2")
	r, _ := FindRepo()
	_, head := r.Head()
	flat, _ := r.ChangeFlat(head)
	got, _ := r.GetBlob(flat["f.txt"].ID)
	if want := strings.Replace(orig, "18\n", "EIGHTEEN\n", 1); string(got) != want {
		t.Fatalf("committed %q", got)
	}
	if read(t, "f.txt") != edited {
		t.Fatal("working file changed by a hunk commit")
	}
	if _, err := sp(t, "commit", "-m", "x", "--hunk", "f.txt:5"); err == nil {
		t.Fatal("missing hunk accepted")
	}
}

func TestCachesLandOnSeparateDisks(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	var swaps int
	var mu sync.Mutex
	ms := NewServer(srv, "")
	h := ms.Handler()
	main := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		if req.URL.Path == "/v1/land" {
			t.Error("landing reached the main server; the cache should merge it")
		}
		if strings.HasPrefix(req.URL.Path, "/v1/refs/") {
			mu.Lock()
			swaps++
			mu.Unlock()
		}
		h.ServeHTTP(w, req)
	}))
	defer main.Close()
	defer ms.Stop()
	newRepo(t)
	must(t, "remote", "add", "origin", main.URL)
	write(t, "a.txt", "a\n")
	must(t, "commit", "-m", "seed")
	must(t, "push")
	swaps = 0
	var caches []string
	for range 2 { // each cache has its own, initially empty, store
		cr, _ := InitRepo(t.TempDir(), true, "main")
		c := NewServer(cr, "")
		c.upstream = newRemote("upstream", main.URL)
		ts := httptest.NewServer(c.Handler())
		defer ts.Close()
		defer c.Stop()
		caches = append(caches, ts.URL)
	}
	errs := make(chan error, 20)
	for i := range 20 {
		go func() {
			errs <- land(t, caches[i%2], "", map[string]*LandFile{fmt.Sprintf("n/%d", i): {Mode: "file", Data: []byte{byte(i)}}})
		}()
	}
	for range 20 {
		if err := <-errs; err != nil {
			t.Fatal(err)
		}
	}
	tip, _ := srv.ReadRef("heads/main")
	flat, err := srv.ChangeFlat(tip)
	if err != nil || len(flat) != 21 {
		t.Fatalf("main server has %d files (%v)", len(flat), err)
	}
	if swaps < 20 {
		t.Fatalf("expected one ref swap per landing, saw %d", swaps)
	}
}

func TestGoAwareMerge(t *testing.T) {
	base := "package x\n\nimport \"strings\"\n\n// A says hi.\nfunc A() string { return strings.ToUpper(\"a\") }\n\nfunc B() int { return 1 }\n"
	ours := "package x\n\nimport (\n\t\"fmt\"\n\t\"strings\"\n)\n\n// A says hi.\nfunc A() string { return fmt.Sprint(strings.ToUpper(\"a\")) }\n\nfunc B() int { return 1 }\n\nfunc C() {}\n"
	theirs := "package x\n\nimport (\n\t\"os\"\n\t\"strings\"\n)\n\n// A says hi.\nfunc A() string { return strings.ToUpper(\"a\") }\n\nfunc B() int { return len(os.Args) }\n\nfunc D() {}\n"
	if res := Merge3(splitLines([]byte(base)), splitLines([]byte(ours)), splitLines([]byte(theirs)), "o", "t"); res.Conflicts == 0 {
		t.Fatal("test needs a case the line merge cannot do")
	}
	got, ok := goMerge([]byte(base), []byte(ours), []byte(theirs))
	if !ok {
		t.Fatal("Go-aware merge gave up")
	}
	for _, want := range []string{`"fmt"`, `"os"`, `"strings"`, "fmt.Sprint(", "len(os.Args)", "func C()", "func D()", "// A says hi."} {
		if !strings.Contains(string(got), want) {
			t.Fatalf("merged file lacks %s:\n%s", want, got)
		}
	}
	// same function changed incompatibly: no silent pick
	o2 := strings.Replace(base, "return 1", "return 2", 1)
	t2 := strings.Replace(base, "return 1", "return 3", 1)
	if _, ok := goMerge([]byte(base), []byte(o2), []byte(t2)); ok {
		t.Fatal("conflicting edits to one function merged")
	}
	// end to end through sprig merge
	newRepo(t)
	write(t, "x.go", base)
	must(t, "commit", "-m", "base")
	must(t, "checkout", "-b", "other")
	write(t, "x.go", theirs)
	must(t, "commit", "-m", "theirs")
	must(t, "checkout", "main")
	write(t, "x.go", ours)
	must(t, "commit", "-m", "ours")
	if out, err := sp(t, "merge", "other"); err != nil {
		t.Fatalf("merge conflicted: %v %s", err, out)
	}
	if !strings.Contains(read(t, "x.go"), "func D()") {
		t.Fatal("merge result missing theirs")
	}
}

func TestGCPacks(t *testing.T) {
	if d := makeDelta([]byte("a\nbb\ncc\n"), []byte("a\nbb\nXX\ncc\n")); true {
		got, err := applyDelta([]byte("a\nbb\ncc\n"), d)
		if err != nil || string(got) != "a\nbb\nXX\ncc\n" {
			t.Fatalf("delta round trip: %q %v", got, err)
		}
	}
	srv, _ := InitRepo(t.TempDir(), true, "main")
	url, _ := seedServer(t, NewServer(srv, ""))
	var body strings.Builder
	for i := range 400 {
		fmt.Fprintf(&body, "%d %s\n", i, HashObject("x", []byte(fmt.Sprint(i)))) // poorly compressible
	}
	for i := range 15 { // many versions of one file, plus a revert
		write(t, "big.txt", body.String()+strings.Repeat("edit\n", i%7))
		must(t, "commit", "-m", fmt.Sprint("v", i))
	}
	before := must(t, "log", "--json", "-n", "100")
	out := must(t, "gc")
	r, _ := FindRepo()
	var kb1, kb2 int
	fmt.Sscanf(out[strings.Index(out, ": ")+2:], "%d KB -> %d KB", &kb1, &kb2)
	if !strings.Contains(out, "as deltas") || kb2*3 > kb1 {
		t.Fatalf("gc did not shrink storage much: %s", out)
	}
	if ents, _ := os.ReadDir(filepath.Join(r.Dir, "objects")); len(ents) != 0 {
		t.Fatalf("loose objects left: %d dirs", len(ents))
	}
	if after := must(t, "log", "--json", "-n", "100"); after != before {
		t.Fatal("history reads differently after gc")
	}
	if out := must(t, "status"); !strings.Contains(out, "clean") {
		t.Fatalf("status after gc: %s", out)
	}
	_, head := r.Head()
	if _, err := r.ResolveRev(head[:10]); err != nil {
		t.Fatalf("short id of a packed change: %v", err)
	}
	// keeps working: new commits, a second gc, and serving from a packed repo
	write(t, "big.txt", "rewritten\n")
	must(t, "commit", "-m", "after gc")
	must(t, "gc")
	must(t, "push")
	if _, err := srv.GC(false); err != nil {
		t.Fatal(err)
	}
	t.Chdir(t.TempDir())
	must(t, "clone", url, "w")
	if read(t, "w/big.txt") != "rewritten\n" {
		t.Fatal("clone from a packed server failed")
	}
}

func TestStatCacheRacyEdit(t *testing.T) {
	newRepo(t)
	write(t, "a.txt", "one\n")
	must(t, "commit", "-m", "one")
	// same size, same mtime, within the racy window: must still be seen
	st, _ := os.Stat("a.txt")
	write(t, "a.txt", "two\n")
	os.Chtimes("a.txt", st.ModTime(), st.ModTime())
	if out := must(t, "status"); !strings.Contains(out, "M a.txt") {
		t.Fatalf("racy edit missed: %s", out)
	}
}

func TestWatcherMatchesWalk(t *testing.T) {
	newRepo(t)
	t.Setenv("SPRIG_NO_WATCH", "")
	for i := range 30 {
		write(t, fmt.Sprintf("d%d/f%d.txt", i%3, i), fmt.Sprint(i))
	}
	must(t, "commit", "-m", "base")
	r, _ := FindRepo()
	stop := make(chan struct{})
	ready := make(chan struct{})
	go r.Watch(stop, func() { close(ready) })
	defer close(stop)
	<-ready
	time.Sleep(1100 * time.Millisecond) // let the cache's save time fall inside what the watcher vouches for
	must(t, "status")                   // saves a cache the watcher can vouch for
	write(t, "d1/f1.txt", "changed")
	os.Remove("d2/f2.txt")
	write(t, "newdir/sub/x.txt", "new")
	os.Rename("d0/f0.txt", "d0/g0.txt")
	snapWith := func(watch string) Flat {
		t.Setenv("SPRIG_NO_WATCH", watch)
		f, err := r.Snapshot(false)
		if err != nil {
			t.Fatal(err)
		}
		return f
	}
	deadline := time.Now().Add(3 * time.Second)
	for {
		cache := r.loadStatCache()
		dirty, ok, _ := r.askWatcher(cache)
		if ok && slices.Contains(dirty, "newdir") && slices.Contains(dirty, "d0/g0.txt") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("watcher never reported the changes: %v %v", dirty, ok)
		}
		time.Sleep(20 * time.Millisecond)
	}
	walked := snapWith("1")
	watched := snapWith("")
	if len(walked) != len(watched) {
		t.Fatalf("watcher snapshot has %d files, walk %d", len(watched), len(walked))
	}
	for p, fe := range walked {
		if watched[p] != fe {
			t.Fatalf("%s differs: watch %v walk %v", p, watched[p], fe)
		}
	}
	if _, ok := watched["newdir/sub/x.txt"]; !ok {
		t.Fatal("file in a new directory missed")
	}
}

func TestPackFormats(t *testing.T) {
	// both delta encoders round-trip, and the block one finds mid-line matches
	base := []byte(strings.Repeat("0123456789abcdef", 40))
	target := append(append([]byte{}, base[:300]...), append([]byte("INSERTED"), base[300:]...)...)
	for _, d := range [][]byte{makeDelta(base, target), blockDelta(base, target), bestDelta(base, target)} {
		if got, err := applyDelta(base, d); err != nil || !bytes.Equal(got, target) {
			t.Fatalf("delta round trip failed: %v", err)
		}
	}
	if d := blockDelta(base, target); len(d) > 40 {
		t.Fatalf("block delta too big: %d bytes", len(d))
	}
	// compact trees expand to the exact JSON
	tr, _ := json.Marshal(Tree{Entries: []Entry{{"a<b>.go", "file", HashObject("blob", nil)}, {"d", "dir", HashObject("tree", nil)}}})
	c, ok := compactTree(tr)
	if !ok || len(c) >= len(tr)/2 {
		t.Fatalf("compact tree: ok=%v %d vs %d bytes", ok, len(c), len(tr))
	}
	if back, err := expandTree(c); err != nil || !bytes.Equal(back, tr) {
		t.Fatalf("compact tree round trip: %s %v", back, err)
	}

	// a v1 (zlib) pack written by older sprig stays readable, and repack converts it
	newRepo(t)
	write(t, "a.txt", "v1 pack content\n")
	must(t, "commit", "-m", "one")
	r, _ := FindRepo()
	_, head := r.Head()
	c1, _ := r.GetChange(head)
	var pk, idx bytes.Buffer
	var ids []string
	filepath.WalkDir(filepath.Join(r.Dir, "objects"), func(p string, d fs.DirEntry, err error) error {
		if !d.IsDir() {
			ids = append(ids, filepath.Base(filepath.Dir(p))+d.Name())
		}
		return nil
	})
	sort.Strings(ids)
	for _, id := range ids {
		typ, data, _ := r.getLocal(id)
		raw, _ := hex.DecodeString(id)
		idx.Write(raw)
		binary.Write(&idx, binary.BigEndian, uint64(pk.Len()))
		zw := zlib.NewWriter(&pk)
		fmt.Fprintf(zw, "%s - %d\x00", typ, len(data))
		zw.Write(data)
		zw.Close()
	}
	os.MkdirAll(r.packDir(), 0o755)
	os.WriteFile(filepath.Join(r.packDir(), "pack-old.pack"), pk.Bytes(), 0o644)
	os.WriteFile(filepath.Join(r.packDir(), "pack-old.idx"), idx.Bytes(), 0o644)
	os.RemoveAll(filepath.Join(r.Dir, "objects"))
	os.MkdirAll(filepath.Join(r.Dir, "objects"), 0o755)
	r.packList(true)
	if got, err := r.GetChange(head); err != nil || got.Tree != c1.Tree {
		t.Fatalf("v1 pack unreadable: %v", err)
	}
	for i := range 10 { // more packs than repackAt: the next gc merges them
		write(t, "a.txt", fmt.Sprint("version ", i, "\n"))
		must(t, "commit", "-m", fmt.Sprint(i))
		must(t, "gc")
	}
	if n := len(r.packList(true)); n > 2 {
		t.Fatalf("%d packs left; gc should have merged them", n)
	}
	must(t, "gc", "--repack")
	if ps := r.packList(true); len(ps) != 1 || !ps[0].v2 {
		t.Fatalf("repack left %d packs", len(ps))
	}
	if out := must(t, "log", "-n", "20"); strings.Count(out, "change ") != 11 {
		t.Fatalf("history lost in repack: %s", out)
	}
}

func TestAutoGC(t *testing.T) {
	newRepo(t)
	t.Setenv("SPRIG_NO_AUTOGC", "")
	var ran int
	old := startGC
	startGC = func(r *Repo) { ran++; r.GC(false) }
	defer func() { startGC = old }()
	write(t, "a.txt", "a")
	must(t, "commit", "-m", "small")
	if ran != 0 {
		t.Fatal("auto gc ran for a tiny repo")
	}
	r, _ := FindRepo()
	os.MkdirAll(filepath.Join(r.Dir, "objects", "17"), 0o755)
	for i := range autoGCLoose/256 + 1 { // fake a full fan-out dir; they are not valid ids, so gc skips them
		os.WriteFile(filepath.Join(r.Dir, "objects", "17", fmt.Sprintf("x%d", i)), nil, 0o644)
	}
	write(t, "a.txt", "b")
	must(t, "commit", "-m", "trigger")
	if ran != 1 {
		t.Fatalf("auto gc ran %d times", ran)
	}
	if _, err := os.Stat(filepath.Join(r.Dir, "gc.lock")); err == nil {
		t.Fatal("gc.lock left behind")
	}
}

func TestRenameAwareMerge(t *testing.T) {
	newRepo(t)
	body := ""
	for i := range 30 {
		body += fmt.Sprintf("line %d\n", i)
	}
	write(t, "lib/util.txt", body)
	write(t, "lib/other.txt", "other\n")
	must(t, "commit", "-m", "base")
	must(t, "checkout", "-b", "edit")
	write(t, "lib/util.txt", strings.Replace(body, "line 5\n", "line FIVE\n", 1))
	write(t, "lib/new.txt", "added on the edit branch\n")
	must(t, "commit", "-m", "edit util, add to lib")
	must(t, "checkout", "main")
	os.MkdirAll("pkg", 0o755)
	os.Rename("lib/util.txt", "pkg/util.txt")
	os.Rename("lib/other.txt", "pkg/other.txt")
	write(t, "pkg/util.txt", strings.Replace(body, "line 25\n", "line 25!\n", 1))
	must(t, "commit", "-m", "move lib to pkg, tweak")
	if out, err := sp(t, "merge", "edit"); err != nil {
		t.Fatalf("rename vs edit conflicted: %v %s", err, out)
	}
	got := read(t, "pkg/util.txt")
	if !strings.Contains(got, "line FIVE") || !strings.Contains(got, "line 25!") {
		t.Fatalf("edit not carried into the renamed file:\n%s", got)
	}
	if _, err := os.Stat("lib/util.txt"); err == nil {
		t.Fatal("old path came back")
	}
	if read(t, "pkg/new.txt") != "added on the edit branch\n" {
		t.Fatal("file added to the moved directory did not follow it")
	}
	if out := must(t, "status", "-M", "0"); !strings.Contains(out, "clean") {
		t.Fatalf("status: %s", out)
	}
}

func TestLandFollowsRename(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	url, repo := seedServer(t, NewServer(srv, ""))
	body := ""
	for i := range 30 {
		body += fmt.Sprintf("row %d\n", i)
	}
	write(t, "a.txt", body)
	must(t, "commit", "-m", "long a")
	must(t, "push")
	refs, _ := newRemote("t", url).Refs()
	stale := refs["heads/main"]
	// someone renames a.txt
	os.Rename("a.txt", "b.txt")
	must(t, "commit", "-m", "rename")
	must(t, "push")
	// a signed landing edits a.txt against the old base
	priv := ed25519.NewKeyFromSeed(make([]byte, 32))
	edited := strings.Replace(body, "row 3\n", "row three\n", 1)
	proof := &LandProof{Branch: "main", Base: stale, Files: map[string]string{"a.txt": HashObject(TypeBlob, []byte(edited))}}
	lr := &LandRequest{Branch: "main", Base: stale, Message: "edit a", Author: "x <x@y>",
		Files: map[string]*LandFile{"a.txt": {Mode: "file", Data: []byte(edited)}}}
	lr.Sig = sign(priv, landPayload(proof, lr.Message, lr.Author, lr.Meta))
	res, err := newRemote("t", url).Land(lr)
	if err != nil {
		t.Fatalf("landing on a renamed file: %v", err)
	}
	flat, _ := repo.ChangeFlat(res.ID)
	if _, ok := flat["a.txt"]; ok {
		t.Fatal("landing resurrected the old path")
	}
	if b, _ := repo.GetBlob(flat["b.txt"].ID); !strings.Contains(string(b), "row three") {
		t.Fatal("edit did not land in the renamed file")
	}
	c, _ := repo.GetChange(res.ID)
	if _, err := repo.VerifyChange(c); err != nil || c.Sig.Moved["a.txt"] != "b.txt" {
		t.Fatalf("signature with a followed rename: %v (moved %v)", err, c.Sig.Moved)
	}
	forged := *c
	sig := *c.Sig
	sig.Moved = map[string]string{"a.txt": "x.txt"}
	forged.Sig = &sig
	if _, err := repo.VerifyChange(&forged); err == nil {
		t.Fatal("made-up move claim verified")
	}
}

func TestRenameDetectionScales(t *testing.T) {
	newRepo(t)
	for i := range 1000 {
		write(t, fmt.Sprintf("old/f%d.txt", i), fmt.Sprintf("file %d\nshared header\n%s\n", i, strings.Repeat(fmt.Sprint(i, " "), 20)))
	}
	must(t, "commit", "-m", "many")
	r, _ := FindRepo()
	_, head := r.Head()
	a, _ := r.ChangeFlat(head)
	b := Flat{}
	for p, fe := range a {
		b["new/"+strings.TrimPrefix(p, "old/")] = fe
	}
	for i := 0; i < 1000; i += 2 { // half of them edited as well
		p := fmt.Sprintf("new/f%d.txt", i)
		data, _ := r.GetBlob(b[p].ID)
		id, _ := r.PutBlob(append(data, "edited\n"...))
		b[p] = FileEnt{"file", id}
	}
	start := time.Now()
	ds := r.withRenames(DiffFlat(a, b))
	took := time.Since(start)
	renamed := 0
	for _, d := range ds {
		if d.Kind == "R" && strings.TrimPrefix(d.From, "old/") == strings.TrimPrefix(d.Path, "new/") {
			renamed++
		}
	}
	if renamed != 1000 {
		t.Fatalf("found %d of 1000 renames", renamed)
	}
	if took > 300*time.Millisecond { // generous for -race; ~50 ms normally
		t.Fatalf("rename detection took %v", took)
	}
	t.Logf("1000 renames (500 edited) in %v", took)
}

func TestStandbyLongPollReflogFailover(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	srv.Durable = true
	url, repo := seedServer(t, NewServer(srv, ""))
	rm := newRemote("t", url)
	refs, etag, err := rm.WaitRefs("")
	if err != nil || refs["heads/main"] == "" {
		t.Fatalf("refs: %v %v", refs, err)
	}

	// long-poll returns as soon as a push lands
	got := make(chan string, 1)
	go func() {
		r2, _, _ := rm.WaitRefs(etag)
		got <- r2["heads/main"]
	}()
	time.Sleep(50 * time.Millisecond)
	write(t, "a.txt", "pushed\n")
	must(t, "commit", "-m", "wake up")
	start := time.Now()
	must(t, "push")
	select {
	case tip := <-got:
		if now, _ := repo.ReadRef("heads/main"); tip != now {
			t.Fatal("long-poll returned stale refs")
		}
		if time.Since(start) > 2*time.Second {
			t.Fatal("long-poll was slow to wake")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("long-poll never woke")
	}

	// the reflog has the push and a landing
	if err := land(t, url, "", map[string]*LandFile{"d/y.txt": {Mode: "file", Data: []byte("y")}}); err != nil {
		t.Fatal(err)
	}
	log, _ := repo.Reflog()
	if len(log) < 2 || log[len(log)-2].Via != "push" || log[len(log)-1].Via != "land" {
		t.Fatalf("reflog: %+v", log)
	}

	// failover: a dead first server is skipped
	if refs, err := newRemote("t", "http://127.0.0.1:1,"+url).Refs(); err != nil || refs["heads/main"] == "" {
		t.Fatalf("failover: %v", err)
	}

	// a standby mirrors the main server and refuses writes
	sb, _ := InitRepo(t.TempDir(), true, "main")
	ss := NewServer(sb, "")
	ss.follow = newRemote("follow", url)
	sts := httptest.NewServer(ss.Handler())
	defer sts.Close()
	defer ss.Stop()
	want, _ := repo.ReadRef("heads/main")
	deadline := time.Now().Add(5 * time.Second)
	for {
		if id, _ := sb.ReadRef("heads/main"); id == want {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("standby never caught up")
		}
		time.Sleep(20 * time.Millisecond)
	}
	must(t, "pull") // the landing above moved main on the server
	write(t, "a.txt", "after standby\n")
	must(t, "commit", "-m", "followed")
	must(t, "push")
	want, _ = repo.ReadRef("heads/main")
	for deadline = time.Now().Add(5 * time.Second); ; {
		if id, _ := sb.ReadRef("heads/main"); id == want {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("standby did not follow a new push")
		}
		time.Sleep(20 * time.Millisecond)
	}
	if flat, err := sb.ChangeFlat(want); err != nil || flat["a.txt"].ID == "" {
		t.Fatalf("standby lacks objects: %v", err)
	}
	if err := land(t, sts.URL, "", map[string]*LandFile{"z": {Mode: "file", Data: []byte("z")}}); err == nil || !strings.Contains(err.Error(), "503") {
		t.Fatalf("standby accepted a landing: %v", err)
	}
	t.Chdir(t.TempDir())
	must(t, "clone", sts.URL, "w")
	if read(t, "w/a.txt") != "after standby\n" {
		t.Fatal("clone from standby is stale")
	}
}

func TestTreeSitterMerge(t *testing.T) {
	cases := []struct {
		file, base, ours, theirs string
		want                     []string
	}{
		{"m.py",
			"import os\n\n\ndef a():\n    return 1\n",
			"import os\nimport sys\n\n\ndef a():\n    return 1\n\n\ndef b():\n    return sys.argv\n",
			"import os\nimport json\n\n\ndef a():\n    return 1\n\n\ndef c():\n    return json.dumps({})\n",
			[]string{"import sys", "import json", "def b()", "def c()"}},
		{"m.js",
			"import a from 'a';\n\nfunction one() { return 1; }\n",
			"import a from 'a';\nimport b from 'b';\n\nfunction one() { return 1; }\n\nfunction two() { return b; }\n",
			"import a from 'a';\nimport c from 'c';\n\nfunction one() { return 1; }\n\nexport const three = c;\n",
			[]string{"import b from 'b'", "import c from 'c'", "function two()", "export const three"}},
		{"m.ts",
			"export interface A { x: number }\n",
			"export interface A { x: number }\n\nexport type B = string;\n",
			"export interface A { x: number }\n\nexport function c(): number { return 1; }\n",
			[]string{"type B", "function c()"}},
		{"m.rs",
			"use std::io;\n\nfn a() {}\n",
			"use std::io;\nuse std::fs;\n\nfn a() {}\n\nfn b() {}\n",
			"use std::io;\nuse std::env;\n\nfn a() {}\n\n#[test]\nfn c() {}\n",
			[]string{"use std::fs;", "use std::env;", "fn b()", "#[test]\nfn c()"}},
		{"M.java",
			"package p;\n\nimport java.util.List;\n\nclass M {\n    int a() { return 1; }\n}\n",
			"package p;\n\nimport java.util.List;\nimport java.util.Map;\n\nclass M {\n    int a() { return 1; }\n\n    int b() { return 2; }\n}\n",
			"package p;\n\nimport java.util.List;\nimport java.util.Set;\n\nclass M {\n    int a() { return 1; }\n\n    int c() { return 3; }\n}\n",
			[]string{"import java.util.Map;", "import java.util.Set;", "int b()", "int c()"}},
		{"k.py", // members of one class, changed on both sides
			"class K:\n    def a(self):\n        return 1\n\n    def b(self):\n        return 2\n",
			"class K:\n    def a(self):\n        return 10\n\n    def b(self):\n        return 2\n",
			"class K:\n    def a(self):\n        return 1\n\n    def b(self):\n        return 20\n\n    def c(self):\n        return 3\n",
			[]string{"return 10", "return 20", "def c(self)"}},
	}
	for _, c := range cases {
		lines := Merge3(splitLines([]byte(c.base)), splitLines([]byte(c.ours)), splitLines([]byte(c.theirs)), "o", "t")
		got, ok := codeMerge(c.file, []byte(c.base), []byte(c.ours), []byte(c.theirs))
		if !ok {
			t.Errorf("%s: structural merge gave up (line merge conflicts: %d)", c.file, lines.Conflicts)
			continue
		}
		for _, w := range c.want {
			if !strings.Contains(string(got), w) {
				t.Errorf("%s: result lacks %q:\n%s", c.file, w, got)
			}
		}
	}
	// the same function changed incompatibly still conflicts
	b := "def f():\n    return 1\n"
	if _, ok := codeMerge("x.py", []byte(b), []byte("def f():\n    return 2\n"), []byte("def f():\n    return 3\n")); ok {
		t.Error("conflicting edits to one Python function merged")
	}
	jb := "class M {\n    int a() { return 1; }\n}\n"
	if _, ok := codeMerge("M.java", []byte(jb), []byte(strings.Replace(jb, "1", "2", 1)), []byte(strings.Replace(jb, "1", "3", 1))); ok {
		t.Error("conflicting edits to one Java method merged")
	}
	// unknown language: no structural merge
	if _, ok := codeMerge("x.txt", []byte("a\n"), []byte("b\n"), []byte("c\n")); ok {
		t.Error("plain text was merged structurally")
	}
}

func TestProjectedClone(t *testing.T) {
	srv, _ := InitRepo(t.TempDir(), true, "main")
	s := NewServer(srv, "full-access-token")
	s.scopes = map[string]Scope{"api-team-token-1234": {Read: []string{"api"}, Write: []string{"api"}}}
	t.Setenv("SPRIG_TOKEN", "full-access-token")
	url, repo := seedServer(t, s)
	write(t, "api/h.go", "package api\n")
	must(t, "commit", "-m", "add api")
	write(t, "secret/plan.txt", "do not leak\n")
	must(t, "commit", "-m", "secret project codename")
	write(t, "api/h.go", "package api\n// v2\n")
	must(t, "commit", "-m", "api v2")
	must(t, "push")

	t.Setenv("SPRIG_TOKEN", "api-team-token-1234")
	t.Chdir(t.TempDir())
	must(t, "clone", url, "w")
	t.Chdir("w")
	if read(t, "api/h.go") != "package api\n// v2\n" {
		t.Fatal("projected clone lacks the api files")
	}
	for _, p := range []string{"a.txt", "secret/plan.txt", "d/x.txt"} {
		if _, err := os.Stat(p); err == nil {
			t.Fatalf("out-of-scope %s was cloned", p)
		}
	}
	log := must(t, "log", "-n", "20")
	if strings.Contains(log, "secret project") || strings.Contains(log, "seed") || strings.Count(log, "change ") != 2 {
		t.Fatalf("projected history should hold only the two api changes:\n%s", log)
	}
	if out := must(t, "show"); !strings.Contains(out, "Projected from") {
		t.Fatalf("projected change not marked: %s", out)
	}
	// land from the projected clone; the real repo gets it on top of everything
	write(t, "api/h.go", "package api\n// v3\n")
	must(t, "land", "-m", "api v3")
	tip, _ := repo.ReadRef("heads/main")
	real, _ := repo.ChangeFlat(tip)
	if b, _ := repo.GetBlob(real["api/h.go"].ID); string(b) != "package api\n// v3\n" || real["secret/plan.txt"].ID == "" {
		t.Fatalf("landing from a projection broke the real tree: %v", real)
	}
	if out := must(t, "status"); !strings.Contains(out, "clean") {
		t.Fatalf("projected clone not clean after land: %s", out)
	}
	// and the projection is persisted: a fresh server process gives the same ids
	refs1, _ := newRemote("t", url).Refs()
	projections.Range(func(k, _ any) bool { projections.Delete(k); return true })
	refs2, _ := newRemote("t", url).Refs()
	if refs1["heads/main"] != refs2["heads/main"] || refs1["heads/main"] == "" {
		t.Fatalf("projection changed after reload: %v vs %v", refs1, refs2)
	}
}

func TestDiffTreesMatchesFlat(t *testing.T) {
	newRepo(t)
	r, _ := FindRepo()
	mk := func(files map[string]string) (string, Flat) {
		f := Flat{}
		for p, c := range files {
			id, _ := r.PutBlob([]byte(c))
			f[p] = FileEnt{"file", id}
		}
		tree, err := r.BuildTree(f)
		if err != nil {
			t.Fatal(err)
		}
		return tree, f
	}
	ta, fa := mk(map[string]string{"a": "1", "d/x": "2", "d/y": "3", "e/f/g": "4", "same/s": "5", "file-then-dir": "6"})
	tb, fb := mk(map[string]string{"a": "1!", "d/x": "2", "e/new": "7", "same/s": "5", "file-then-dir/inside": "8", "n/m": "9"})
	got, err := r.DiffTrees(ta, tb)
	if err != nil {
		t.Fatal(err)
	}
	want := DiffFlat(fa, fb)
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("tree diff differs from flat diff:\n got %v\nwant %v", got, want)
	}
	if empty, _ := r.DiffTrees("", tb); len(empty) != len(fb) {
		t.Fatalf("diff from nothing: %d entries", len(empty))
	}
}

func TestStatusFastPathMatchesFull(t *testing.T) {
	newRepo(t)
	t.Setenv("SPRIG_NO_WATCH", "")
	for i := range 40 {
		write(t, fmt.Sprintf("d%d/f%d.txt", i%4, i), fmt.Sprint(i))
	}
	write(t, "keep.txt", "k")
	must(t, "commit", "-m", "base")
	r, _ := FindRepo()
	stop := make(chan struct{})
	ready := make(chan struct{})
	go r.Watch(stop, func() { close(ready) })
	defer close(stop)
	<-ready
	time.Sleep(1100 * time.Millisecond) // the memo's start time must fall inside what the watcher vouches for
	must(t, "status")                   // writes the memo
	check := func(step string) {
		t.Helper()
		time.Sleep(150 * time.Millisecond) // let the watcher see the events
		t.Setenv("SPRIG_NO_WATCH", "1")
		full := must(t, "status", "--json")
		t.Setenv("SPRIG_NO_WATCH", "")
		fast := must(t, "status", "--json")
		if fast != full {
			t.Fatalf("%s: fast status differs\nfast: %s\nfull: %s", step, fast, full)
		}
	}
	write(t, "d1/f1.txt", "edited")
	check("edit")
	os.Remove("d2/f2.txt")
	check("delete")
	write(t, "newdir/a/b.txt", "new")
	check("new directory")
	write(t, "d1/f1.txt", "1") // back to HEAD's content
	check("revert")
	os.RemoveAll("d3")
	check("delete directory")
	write(t, ".sprigignore", "newdir\n")
	check("ignore file changed")
	// the fast path really is taken: with a clean tree, no full listing is needed
	if out := must(t, "status"); !strings.Contains(out, "d2/f2.txt") {
		t.Fatalf("status: %s", out)
	}
}

func TestStatusAfterOtherCommandSavesCache(t *testing.T) {
	newRepo(t)
	t.Setenv("SPRIG_NO_WATCH", "1")
	write(t, "f.txt", "one\n")
	write(t, "g.txt", "g\n")
	must(t, "commit", "-m", "base")
	past := time.Now().Add(-time.Minute)
	os.Chtimes("f.txt", past, past)
	os.Chtimes("g.txt", past, past)
	must(t, "status") // memo: clean
	must(t, "status") // and the fast walk agrees
	write(t, "f.txt", "two\n")
	os.Chtimes("f.txt", past.Add(time.Second), past.Add(time.Second))
	must(t, "diff") // saves the stat cache with f's new state
	if out := must(t, "status"); !strings.Contains(out, "M f.txt") {
		t.Fatalf("edit hidden by a cache another command saved: %s", out)
	}
	if out := must(t, "status"); !strings.Contains(out, "M f.txt") {
		t.Fatalf("edit lost on the second status: %s", out)
	}
	os.Remove("g.txt")
	if out := must(t, "status"); !strings.Contains(out, "D g.txt") || !strings.Contains(out, "M f.txt") {
		t.Fatalf("deletion missed by the stat walk: %s", out)
	}
}
