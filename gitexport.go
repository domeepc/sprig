package main

import (
	"bufio"
	"bytes"
	"fmt"
	"os/exec"
	"sort"
	"strings"
)

// exportRef is the scratch ref fast-import builds commits on; branches are
// pointed at the results at the end and the scratch ref is deleted.
const exportRef = "refs/sprig-export"

type ExportResult struct {
	Commits  int               // new git commits written
	Branches map[string]string // branch -> git commit sha
	Warnings []string
}

type exporter struct {
	r       *Repo
	dir     string
	w       *bufio.Writer
	next    int
	blobs   map[string]int    // sprig blob id -> fast-import mark
	commits map[string]string // sprig change id -> git sha or ":mark"
	res     *ExportResult
}

// ExportGit writes every sprig branch into a git repository via
// `git fast-import`, so it can be pushed with git. Changes that came from git
// reuse their original commit; the rest become commits with a Sprig-Change
// trailer, which import-git maps back. Output is deterministic, so exporting
// twice yields the same commits. Branch updates must fast-forward unless force.
func (r *Repo) ExportGit(gitDir string, force bool) (*ExportResult, error) {
	res := &ExportResult{Branches: map[string]string{}}
	if _, err := gitOutput(gitDir, "rev-parse", "--git-dir"); err != nil {
		return nil, err
	}
	var names []string
	refs := r.ListRefs()
	for name := range refs {
		if strings.HasPrefix(name, "heads/") {
			names = append(names, strings.TrimPrefix(name, "heads/"))
		}
	}
	if len(names) == 0 {
		return nil, fmt.Errorf("nothing to export: no branches")
	}
	sort.Strings(names)

	gitOutput(gitDir, "update-ref", "-d", exportRef) // leftover from a failed run
	args := []string{"-C", gitDir, "fast-import", "--quiet"}
	if force {
		args = append(args, "--force")
	}
	cmd := exec.Command("git", args...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	in, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("cannot run git: %w", err)
	}
	ex := &exporter{r: r, dir: gitDir, w: bufio.NewWriterSize(in, 1<<20), res: res,
		blobs: map[string]int{}, commits: map[string]string{}}
	var exported []string
	streamErr := func() error {
		for _, b := range names {
			id := refs["heads/"+b]
			c, err := r.GetChange(id)
			if err != nil {
				return err
			}
			if len(c.Conflicts) > 0 && !force {
				res.Warnings = append(res.Warnings, fmt.Sprintf("branch %q has %d unresolved conflict(s); skipped (use --force to export anyway)", b, len(c.Conflicts)))
				continue
			}
			ref, err := ex.export(id)
			if err != nil {
				return fmt.Errorf("change %s: %w", short(id), err)
			}
			fmt.Fprintf(ex.w, "reset refs/heads/%s\nfrom %s\n\n", b, ref)
			exported = append(exported, b)
		}
		return ex.w.Flush()
	}()
	in.Close()
	waitErr := cmd.Wait()
	gitOutput(gitDir, "update-ref", "-d", exportRef)
	if waitErr != nil {
		return nil, fmt.Errorf("git fast-import: %v: %s", waitErr, strings.TrimSpace(stderr.String()))
	}
	if streamErr != nil {
		return nil, streamErr
	}
	for _, b := range exported {
		sha, err := gitOutput(gitDir, "rev-parse", "refs/heads/"+b)
		if err != nil {
			return nil, err
		}
		res.Branches[b] = sha
	}
	return res, nil
}

func (ex *exporter) mark() int {
	ex.next++
	return ex.next
}

// export writes change id (and any ancestors git lacks) to the stream and
// returns how to refer to it in a later `from`/`merge`.
func (ex *exporter) export(id string) (string, error) {
	if ref, ok := ex.commits[id]; ok {
		return ref, nil
	}
	c, err := ex.r.GetChange(id)
	if err != nil {
		return "", err
	}
	// shortcut: sprig-made changes are re-streamed on every export (same output, so harmless); record change->sha if exports get slow.
	if c.Meta.GitSHA != "" && exec.Command("git", "-C", ex.dir, "cat-file", "-e", c.Meta.GitSHA+"^{commit}").Run() == nil {
		ex.commits[id] = c.Meta.GitSHA
		return c.Meta.GitSHA, nil
	}
	var parents []string
	for _, p := range c.Parents {
		ref, err := ex.export(p)
		if err != nil {
			return "", err
		}
		parents = append(parents, ref)
	}
	flat, err := ex.r.Flatten(c.Tree)
	if err != nil {
		return "", err
	}
	paths := make([]string, 0, len(flat))
	for p := range flat {
		paths = append(paths, p)
	}
	sort.Strings(paths)
	for _, p := range paths {
		bid := flat[p].ID
		if _, ok := ex.blobs[bid]; ok {
			continue
		}
		data, err := ex.r.GetBlob(bid)
		if err != nil {
			return "", err
		}
		m := ex.mark()
		ex.blobs[bid] = m
		fmt.Fprintf(ex.w, "blob\nmark :%d\ndata %d\n", m, len(data))
		ex.w.Write(data)
		ex.w.WriteString("\n")
	}

	m := ex.mark()
	if len(parents) == 0 {
		fmt.Fprintf(ex.w, "reset %s\n", exportRef) // otherwise fast-import continues from the ref's tip
	}
	tz := c.TZ
	if tz == "" {
		tz = "+0000"
	}
	sig := fmt.Sprintf("%s %d %s", gitIdent(c.Author), c.Time, tz)
	msg := exportMessage(id, c)
	fmt.Fprintf(ex.w, "commit %s\nmark :%d\nauthor %s\ncommitter %s\ndata %d\n%s\n", exportRef, m, sig, sig, len(msg), msg)
	for i, p := range parents {
		if i == 0 {
			fmt.Fprintf(ex.w, "from %s\n", p)
		} else {
			fmt.Fprintf(ex.w, "merge %s\n", p)
		}
	}
	ex.w.WriteString("deleteall\n")
	for _, p := range paths {
		fe := flat[p]
		mode := "100644"
		switch fe.Mode {
		case "exec":
			mode = "100755"
		case "link":
			mode = "120000"
		}
		fmt.Fprintf(ex.w, "M %s :%d %s\n", mode, ex.blobs[fe.ID], fastImportPath(p))
	}
	ex.w.WriteString("\n")
	ref := fmt.Sprintf(":%d", m)
	ex.commits[id] = ref
	ex.res.Commits++
	return ref, nil
}

// gitIdent makes "Name <email>" out of a sprig author string.
func gitIdent(a string) string {
	if strings.Contains(a, "<") && strings.HasSuffix(a, ">") {
		return a
	}
	return strings.NewReplacer("<", "", ">", "").Replace(a) + " <>"
}

// exportMessage is the change message plus its provenance as git trailers.
func exportMessage(id string, c *Change) string {
	var b strings.Builder
	b.WriteString(strings.TrimRight(c.Message, "\n"))
	b.WriteString("\n\n")
	oneLine := strings.NewReplacer("\n", " ").Replace
	if c.Meta.Intent != "" {
		fmt.Fprintf(&b, "Sprig-Intent: %s\n", oneLine(c.Meta.Intent))
	}
	if c.Meta.Agent != "" {
		fmt.Fprintf(&b, "Sprig-Agent: %s\n", oneLine(c.Meta.Agent))
	}
	if c.Meta.Model != "" {
		fmt.Fprintf(&b, "Sprig-Model: %s\n", oneLine(c.Meta.Model))
	}
	keys := make([]string, 0, len(c.Meta.Checks))
	for k := range c.Meta.Checks {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		fmt.Fprintf(&b, "Sprig-Check: %s=%s\n", oneLine(k), oneLine(c.Meta.Checks[k]))
	}
	fmt.Fprintf(&b, "Sprig-Change: %s\n", id)
	return b.String()
}

// sprigTrailer returns the Sprig-Change id from an exported commit message.
func sprigTrailer(msg string) string {
	lines := strings.Split(strings.TrimRight(msg, "\n"), "\n")
	if id, ok := strings.CutPrefix(lines[len(lines)-1], "Sprig-Change: "); ok && idRe.MatchString(id) {
		return id
	}
	return ""
}

func fastImportPath(p string) string {
	if strings.HasPrefix(p, `"`) || strings.Contains(p, "\n") {
		return `"` + strings.NewReplacer(`\`, `\\`, `"`, `\"`, "\n", `\n`).Replace(p) + `"`
	}
	return p
}
