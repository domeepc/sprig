package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
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
	s.scopes = map[string][]string{"docs-only-token-123": {"d"}}
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
