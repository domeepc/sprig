package main

import (
	"bufio"
	"bytes"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os/exec"
	"slices"
	"strconv"
	"strings"
)

// gitReader talks to a long-lived `git cat-file --batch` process so that
// importing does not fork once per object.
type gitReader struct {
	cmd *exec.Cmd
	in  io.WriteCloser
	out *bufio.Reader
}

func newGitReader(dir string) (*gitReader, error) {
	cmd := exec.Command("git", "-C", dir, "cat-file", "--batch")
	in, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	out, err := cmd.StdoutPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("cannot run git: %w", err)
	}
	return &gitReader{cmd, in, bufio.NewReaderSize(out, 1<<20)}, nil
}

func (g *gitReader) close() {
	g.in.Close()
	g.cmd.Wait()
}

func (g *gitReader) read(sha string) (string, []byte, error) {
	if _, err := fmt.Fprintf(g.in, "%s\n", sha); err != nil {
		return "", nil, err
	}
	line, err := g.out.ReadString('\n')
	if err != nil {
		return "", nil, err
	}
	f := strings.Fields(line)
	if len(f) == 2 && f[1] == "missing" {
		return "", nil, fmt.Errorf("git object %s missing", sha)
	}
	if len(f) != 3 {
		return "", nil, fmt.Errorf("unexpected git output %q", line)
	}
	size, err := strconv.Atoi(f[2])
	if err != nil {
		return "", nil, err
	}
	data := make([]byte, size)
	if _, err := io.ReadFull(g.out, data); err != nil {
		return "", nil, err
	}
	g.out.ReadByte() // trailing newline
	return f[1], data, nil
}

func gitOutput(dir string, args ...string) (string, error) {
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	out, err := cmd.Output()
	if err != nil {
		return "", fmt.Errorf("git %s: %v: %s", strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	}
	return strings.TrimRight(string(out), "\n"), nil
}

type ImportResult struct {
	Changes  int
	Blobs    int
	Skipped  int // submodule entries
	Branches map[string]string
	Warnings []string
}

type importer struct {
	r       *Repo
	g       *gitReader
	rawLen  int
	trees   map[string]string // git tree sha -> sprig tree id
	blobs   map[string]string // git blob sha -> sprig blob id
	changes map[string]string // git commit sha -> sprig change id
	res     *ImportResult
}

// ImportGit converts the branches of a git repository into sprig changes.
// It is deterministic: re-importing the same repository yields the same ids.
func (r *Repo) ImportGit(gitDir string, force bool) (*ImportResult, error) {
	res := &ImportResult{Branches: map[string]string{}}
	refsOut, err := gitOutput(gitDir, "for-each-ref", "--format=%(refname:short) %(objectname)", "refs/heads")
	if err != nil {
		return nil, err
	}
	type br struct{ name, sha string }
	var branches []br
	var tips []string
	for _, line := range strings.Split(refsOut, "\n") {
		f := strings.Fields(line)
		if len(f) == 2 {
			branches = append(branches, br{f[0], f[1]})
			tips = append(tips, f[1])
		}
	}
	if len(branches) == 0 {
		return nil, errors.New("git repository has no branches with commits")
	}
	rawLen := 20
	if f, err := gitOutput(gitDir, "rev-parse", "--show-object-format"); err == nil && f == "sha256" {
		rawLen = 32
	}
	g, err := newGitReader(gitDir)
	if err != nil {
		return nil, err
	}
	defer g.close()
	imp := &importer{r: r, g: g, rawLen: rawLen, res: res,
		trees: map[string]string{}, blobs: map[string]string{}, changes: map[string]string{}}

	revs, err := gitOutput(gitDir, append([]string{"rev-list", "--topo-order", "--reverse", "--parents"}, tips...)...)
	if err != nil {
		return nil, err
	}
	err = r.Do("import-git", func() error {
		_, headBefore := r.Head()
		for _, line := range strings.Split(revs, "\n") {
			f := strings.Fields(line)
			if len(f) == 0 {
				continue
			}
			if err := imp.importCommit(f[0], f[1:]); err != nil {
				return fmt.Errorf("commit %s: %w", f[0][:min(10, len(f[0]))], err)
			}
		}
		for _, b := range branches {
			id := imp.changes[b.sha]
			name := "heads/" + b.name
			if cur, ok := r.ReadRef(name); ok && !force {
				if behind, err := r.IsAncestor(id, cur); err == nil && behind {
					continue // sprig already has everything git has here
				}
				if ff, err := r.IsAncestor(cur, id); err != nil || !ff {
					// diverged: keep git's side as git/<branch> so it can be merged
					if err := r.WriteRef("remotes/git/"+b.name, id); err != nil {
						return err
					}
					res.Warnings = append(res.Warnings, fmt.Sprintf("branch %q has diverged; git's version is at git/%s (run `sprig merge git/%s`, or --force to overwrite)", b.name, b.name, b.name))
					continue
				}
			}
			// Moving the checked-out branch must move the working tree with it.
			if headRef, cur := r.Head(); headRef == name && !r.Bare && cur != "" && cur != id {
				from, err := r.ChangeFlat(cur)
				if err != nil {
					return err
				}
				if clean, _, err := r.workingMatches(from); err != nil {
					return err
				} else if !clean {
					res.Warnings = append(res.Warnings, fmt.Sprintf("branch %q is checked out with uncommitted changes; skipped (commit first)", b.name))
					continue
				}
				to, err := r.ChangeFlat(id)
				if err != nil {
					return err
				}
				if err := r.Materialize(from, to); err != nil {
					return err
				}
			}
			if !validRefName(name) {
				res.Warnings = append(res.Warnings, fmt.Sprintf("branch %q has a name sprig does not accept; skipped", b.name))
				continue
			}
			if err := r.WriteRef(name, id); err != nil {
				return err
			}
			res.Branches[b.name] = id
		}
		// A fresh repo adopts the imported HEAD branch and checks it out,
		// as long as that cannot overwrite files that are already there.
		if headBefore == "" && !r.Bare {
			head, err := gitOutput(gitDir, "symbolic-ref", "--short", "HEAD")
			if err != nil {
				return nil // detached or unborn: leave HEAD alone
			}
			id, ok := res.Branches[head]
			if !ok {
				return nil
			}
			cur, err := r.Snapshot(false)
			if err != nil {
				return err
			}
			if len(cur) > 0 {
				res.Warnings = append(res.Warnings, fmt.Sprintf("working tree is not empty; not checking out %q (run `sprig checkout -f %s` to do so)", head, head))
				return nil
			}
			flat, err := r.ChangeFlat(id)
			if err != nil {
				return err
			}
			if err := r.Materialize(cur, flat); err != nil {
				return err
			}
			return r.SetHeadRef("heads/" + head)
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	res.Changes = len(imp.changes)
	res.Blobs = len(imp.blobs)
	return res, nil
}

func (im *importer) importCommit(sha string, parents []string) error {
	typ, data, err := im.g.read(sha)
	if err != nil {
		return err
	}
	if typ != "commit" {
		return fmt.Errorf("%s is a %s, not a commit", sha, typ)
	}
	head, msg, _ := strings.Cut(string(data), "\n\n")
	var treeSha, author string
	var ts int64
	var tz string
	for _, line := range strings.Split(head, "\n") {
		switch {
		case strings.HasPrefix(line, "tree "):
			treeSha = strings.TrimPrefix(line, "tree ")
		case strings.HasPrefix(line, "author "):
			rest := strings.TrimPrefix(line, "author ")
			f := strings.Fields(rest)
			if len(f) >= 3 {
				author = strings.Join(f[:len(f)-2], " ")
				ts, _ = strconv.ParseInt(f[len(f)-2], 10, 64)
				tz = f[len(f)-1]
			} else {
				author = rest
			}
		}
	}
	if treeSha == "" {
		return errors.New("commit has no tree")
	}
	tree, err := im.importTree(treeSha)
	if err != nil {
		return err
	}
	var pids []string
	for _, p := range parents {
		id, ok := im.changes[p]
		if !ok {
			return fmt.Errorf("parent %s not imported yet", p)
		}
		pids = append(pids, id)
	}
	// A commit written by export-git maps back to its change, provided the
	// content and parents really match (the trailer alone is just text).
	if xid := sprigTrailer(msg); xid != "" {
		if x, err := im.r.GetChange(xid); err == nil && x.Tree == tree && slices.Equal(x.Parents, pids) {
			im.changes[sha] = xid
			return nil
		}
	}
	c := &Change{Parents: pids, Tree: tree, Author: author, Time: ts, TZ: tz,
		Message: strings.TrimRight(msg, "\n"), Meta: Meta{GitSHA: sha}}
	id, err := im.r.PutChange(c)
	if err != nil {
		return err
	}
	im.changes[sha] = id
	return nil
}

func (im *importer) importTree(sha string) (string, error) {
	if id, ok := im.trees[sha]; ok {
		return id, nil
	}
	typ, data, err := im.g.read(sha)
	if err != nil {
		return "", err
	}
	if typ != "tree" {
		return "", fmt.Errorf("%s is a %s, not a tree", sha, typ)
	}
	t := &Tree{}
	for len(data) > 0 {
		sp := bytes.IndexByte(data, ' ')
		nul := bytes.IndexByte(data, 0)
		if sp < 0 || nul < sp || len(data) < nul+1+im.rawLen {
			return "", errors.New("malformed tree")
		}
		mode, name := string(data[:sp]), string(data[sp+1:nul])
		child := hex.EncodeToString(data[nul+1 : nul+1+im.rawLen])
		data = data[nul+1+im.rawLen:]
		if !validName(name) {
			im.res.Warnings = append(im.res.Warnings, fmt.Sprintf("skipped entry with unsupported name %q", name))
			continue
		}
		switch mode {
		case "40000", "040000":
			id, err := im.importTree(child)
			if err != nil {
				return "", err
			}
			t.Entries = append(t.Entries, Entry{name, "dir", id})
		case "100644", "100664", "100755", "120000":
			id, err := im.importBlob(child)
			if err != nil {
				return "", err
			}
			m := "file"
			if mode == "100755" {
				m = "exec"
			} else if mode == "120000" {
				m = "link"
			}
			t.Entries = append(t.Entries, Entry{name, m, id})
		default: // 160000 gitlink (submodule): not representable
			im.res.Skipped++
		}
	}
	id, err := im.r.PutTree(t)
	if err != nil {
		return "", err
	}
	im.trees[sha] = id
	return id, nil
}

func (im *importer) importBlob(sha string) (string, error) {
	if id, ok := im.blobs[sha]; ok {
		return id, nil
	}
	typ, data, err := im.g.read(sha)
	if err != nil {
		return "", err
	}
	if typ != "blob" {
		return "", fmt.Errorf("%s is a %s, not a blob", sha, typ)
	}
	id, err := im.r.PutBlob(data)
	if err != nil {
		return "", err
	}
	im.blobs[sha] = id
	return id, nil
}
