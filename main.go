package main

import (
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
)

var stdout io.Writer = os.Stdout

type exitError struct {
	code int
	msg  string
}

func (e *exitError) Error() string { return e.msg }

func main() {
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}
	err := run(os.Args[1], os.Args[2:])
	if err != nil {
		var ee *exitError
		if errors.As(err, &ee) {
			if ee.msg != "" {
				fmt.Fprintln(os.Stderr, "sprig:", ee.msg)
			}
			os.Exit(ee.code)
		}
		fmt.Fprintln(os.Stderr, "sprig:", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprint(os.Stderr, `sprig - a small version control system for humans and agents

usage: sprig <command> [flags] [args]     (flags go before positional args)

  init [--bare] [-b branch] [dir]     create a repository
  clone [-b branch] [--path dir]... <url> <dir>   lazy (and optionally sparse) working copy
  commit -m msg [--intent s] [--agent s] [--model s] [--check k=v]... [path...]
  status [--json]                     working tree vs HEAD, pending conflicts
  log [-n N] [--agent A] [--json] [rev]
  show [rev]                          a change and its diff
  diff [revA [revB]]                  working tree vs HEAD, or between two revs
  branch [-d] [name [start]]          list, create or delete branches
  checkout [-f] <branch|rev> | checkout -b <name> [start]
  merge [--no-ff] [-m msg] <rev>      conflicts are recorded in the change
  conflicts                           list conflicts recorded in HEAD
  resolve (--ours|--theirs|--base) <path>...
  ops                                 operation log
  undo [-f]                           undo the last operation (undo again = redo)
  audit [--require check] [--signed] [--json] [rev]   agent changes lacking a check or trusted signature
  keygen                              create a signing key; commits, merges and landings get signed
  trust [<key> <name>]                list or add keys that audit --signed accepts
  remote add <name> <url> | remote list
  push [-f] [remote] [branch]    fetch [remote] [branch]    pull [remote] [branch]
  land -m msg [--intent s] [--check k=v]... [remote] [branch]
                                      send working-tree edits; the server rebases and lands them
  serve [--addr host:port] [--token t] [--tokens file] [--upstream url] [dir]
  import-git <path-to-git-repo>       convert a git repository's branches
  export-git [--force] <git-repo>     write branches into a git repo (then git push)
  cat <id>                            print a raw object
  sparse [path...]                    show or change which paths are checked out (none = all)

environment: SPRIG_AUTHOR, SPRIG_AGENT, SPRIG_MODEL, SPRIG_TOKEN, SPRIG_KEY (signing key file)
`)
}

func authorFrom(c Config) string {
	if a := os.Getenv("SPRIG_AUTHOR"); a != "" {
		return a
	}
	if c.Author != "" {
		return c.Author
	}
	u := os.Getenv("USER")
	if u == "" {
		u = "unknown"
	}
	return u + " <" + u + "@localhost>"
}

type multiFlag []string

func (m *multiFlag) String() string     { return strings.Join(*m, ",") }
func (m *multiFlag) Set(v string) error { *m = append(*m, v); return nil }

func newFlags(name string) *flag.FlagSet {
	fs := flag.NewFlagSet(name, flag.ContinueOnError)
	fs.SetOutput(os.Stderr)
	return fs
}

func printJSON(v any) error {
	b, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	fmt.Fprintln(stdout, string(b))
	return nil
}

func run(cmd string, args []string) error {
	switch cmd {
	case "help", "-h", "--help":
		usage()
		return nil
	case "version":
		fmt.Fprintln(stdout, "sprig 0.1")
		return nil
	case "init":
		return cmdInit(args)
	case "serve":
		return cmdServe(args)
	case "keygen":
		path, pub, err := generateKey()
		if err != nil {
			return err
		}
		fmt.Fprintf(stdout, "wrote %s\npublic key: %s\nyour changes are now signed; others can trust you with `sprig trust %s <name>`\n", path, pub, pub)
		return nil
	case "clone":
		return cmdClone(args)
	}
	r, err := FindRepo()
	if err != nil {
		return err
	}
	switch cmd {
	case "commit":
		return cmdCommit(r, args)
	case "status":
		return cmdStatus(r, args)
	case "log":
		return cmdLog(r, args)
	case "show":
		return cmdShow(r, args)
	case "diff":
		return cmdDiff(r, args)
	case "branch":
		return cmdBranch(r, args)
	case "checkout":
		return cmdCheckout(r, args)
	case "merge":
		return cmdMerge(r, args)
	case "conflicts":
		return cmdConflicts(r, args)
	case "resolve":
		return cmdResolve(r, args)
	case "ops":
		return cmdOps(r, args)
	case "undo":
		return cmdUndo(r, args)
	case "audit":
		return cmdAudit(r, args)
	case "remote":
		return cmdRemote(r, args)
	case "push":
		return cmdPush(r, args)
	case "land":
		return cmdLand(r, args)
	case "fetch":
		return cmdFetch(r, args, false)
	case "pull":
		return cmdFetch(r, args, true)
	case "import-git":
		return cmdImportGit(r, args)
	case "export-git":
		return cmdExportGit(r, args)
	case "cat":
		return cmdCat(r, args)
	case "sparse":
		return cmdSparse(r, args)
	case "trust":
		return cmdTrust(r, args)
	}
	usage()
	return &exitError{2, fmt.Sprintf("unknown command %q", cmd)}
}

// ---------------------------------------------------------------------------

func cmdInit(args []string) error {
	fs := newFlags("init")
	bare := fs.Bool("bare", false, "create a bare (server-side) repository")
	branch := fs.String("b", "main", "initial branch name")
	if err := fs.Parse(args); err != nil {
		return err
	}
	dir := "."
	if fs.NArg() > 0 {
		dir = fs.Arg(0)
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	abs, _ := filepath.Abs(dir)
	r, err := InitRepo(abs, *bare, *branch)
	if err != nil {
		return err
	}
	fmt.Fprintf(stdout, "initialized empty sprig repository in %s\n", r.Dir)
	return nil
}

// cmdClone makes a lazy working copy: history and directory trees are
// downloaded, file contents only when first read, and --path limits which
// files are written to disk at all.
func cmdClone(args []string) error {
	fs := newFlags("clone")
	branch := fs.String("b", "main", "branch to check out")
	var paths multiFlag
	fs.Var(&paths, "path", "only check out this directory or file (repeatable)")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() != 2 {
		return errors.New("usage: clone [-b branch] [--path dir]... <url> <dir>")
	}
	var sparse []string
	for _, p := range paths {
		p = strings.Trim(p, "/")
		if !validPath(p) {
			return fmt.Errorf("bad path %q", p)
		}
		sparse = append(sparse, p)
	}
	if err := os.MkdirAll(fs.Arg(1), 0o755); err != nil {
		return err
	}
	abs, _ := filepath.Abs(fs.Arg(1))
	r, err := InitRepo(abs, false, *branch)
	if err != nil {
		return err
	}
	cfg := r.LoadConfig()
	cfg.Remotes["origin"] = strings.TrimRight(fs.Arg(0), "/")
	cfg.Lazy, cfg.Sparse = "origin", sparse
	if err := r.SaveConfig(cfg); err != nil {
		return err
	}
	rm, err := r.Remote("origin")
	if err != nil {
		return err
	}
	tip, n, err := r.Fetch(rm, *branch)
	if err != nil {
		return err
	}
	flat, err := r.ChangeFlat(tip)
	if err != nil {
		return err
	}
	if err := r.Materialize(Flat{}, flat); err != nil {
		return err
	}
	if err := r.WriteRef("heads/"+*branch, tip); err != nil {
		return err
	}
	fmt.Fprintf(stdout, "cloned %s into %s (%d objects; file contents load on demand)\n", *branch, abs, n)
	return nil
}

func parseChecks(list []string) (map[string]string, error) {
	if len(list) == 0 {
		return nil, nil
	}
	out := map[string]string{}
	for _, kv := range list {
		k, v, ok := strings.Cut(kv, "=")
		if !ok || k == "" {
			return nil, fmt.Errorf("bad --check %q (want name=value)", kv)
		}
		out[k] = v
	}
	return out, nil
}

func metaFlags(fs *flag.FlagSet) func() (Meta, error) {
	intent := fs.String("intent", "", "what this change was asked to do / why")
	agent := fs.String("agent", os.Getenv("SPRIG_AGENT"), "agent or tool that produced it")
	model := fs.String("model", os.Getenv("SPRIG_MODEL"), "model identifier")
	var checks multiFlag
	fs.Var(&checks, "check", "verification result name=value (repeatable)")
	return func() (Meta, error) {
		c, err := parseChecks(checks)
		return Meta{Intent: *intent, Agent: *agent, Model: *model, Checks: c}, err
	}
}

func cmdCommit(r *Repo, args []string) error {
	fs := newFlags("commit")
	msg := fs.String("m", "", "message")
	empty := fs.Bool("allow-empty", false, "allow a change with no file differences")
	getMeta := metaFlags(fs)
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *msg == "" {
		return errors.New("a message is required (-m)")
	}
	meta, err := getMeta()
	if err != nil {
		return err
	}
	var id string
	err = r.Do("commit", func() error {
		var err error
		var paths []string
		for _, p := range fs.Args() {
			if p = strings.Trim(filepath.ToSlash(p), "/"); !validPath(p) {
				return fmt.Errorf("bad path %q", p)
			}
			paths = append(paths, p)
		}
		id, err = r.Commit(CommitOpts{Message: *msg, Meta: meta, AllowEmpty: *empty, Paths: paths})
		return err
	})
	if errors.Is(err, errNothingToCommit) {
		return &exitError{1, "nothing to commit"}
	}
	if err != nil {
		return err
	}
	ref, _ := r.Head()
	fmt.Fprintf(stdout, "[%s %s] %s\n", strings.TrimPrefix(ref, "heads/"), short(id), firstLine(*msg))
	if c, _ := r.GetChange(id); c != nil && len(c.Conflicts) > 0 {
		fmt.Fprintf(stdout, "warning: %d unresolved conflict(s) carried forward (see `sprig conflicts`)\n", len(c.Conflicts))
	}
	return nil
}

func firstLine(s string) string {
	l, _, _ := strings.Cut(s, "\n")
	return l
}

func cmdStatus(r *Repo, args []string) error {
	fs := newFlags("status")
	asJSON := fs.Bool("json", false, "machine-readable output")
	if err := fs.Parse(args); err != nil {
		return err
	}
	ref, head := r.Head()
	headFlat, err := r.ChangeFlat(head)
	if err != nil {
		return err
	}
	_, deltas, err := r.workingMatches(headFlat)
	if err != nil {
		return err
	}
	deltas = withRenames(deltas)
	var conflicts []Conflict
	if head != "" {
		c, err := r.GetChange(head)
		if err != nil {
			return err
		}
		conflicts = c.Conflicts
	}
	if *asJSON {
		type ch struct {
			Path string `json:"path"`
			Kind string `json:"kind"`
			From string `json:"from,omitempty"`
		}
		changes := []ch{}
		for _, d := range deltas {
			changes = append(changes, ch{d.Path, d.Kind, d.From})
		}
		paths := []string{}
		for _, c := range conflicts {
			paths = append(paths, c.Path)
		}
		return printJSON(map[string]any{"branch": strings.TrimPrefix(ref, "heads/"), "head": head,
			"changes": changes, "conflicts": paths})
	}
	if ref != "" {
		fmt.Fprintf(stdout, "On branch %s", strings.TrimPrefix(ref, "heads/"))
	} else {
		fmt.Fprint(stdout, "HEAD detached")
	}
	if head != "" {
		fmt.Fprintf(stdout, " (%s)", short(head))
	} else {
		fmt.Fprint(stdout, " (no changes yet)")
	}
	fmt.Fprintln(stdout)
	if len(conflicts) > 0 {
		fmt.Fprintf(stdout, "\nunresolved conflicts (%d):\n", len(conflicts))
		for _, c := range conflicts {
			fmt.Fprintf(stdout, "  C %s\n", c.Path)
		}
	}
	if len(deltas) == 0 {
		fmt.Fprintln(stdout, "\nworking tree clean")
		return nil
	}
	fmt.Fprintln(stdout, "\nchanges since last change:")
	for _, d := range deltas {
		if d.Kind == "R" {
			fmt.Fprintf(stdout, "  R %s -> %s\n", d.From, d.Path)
			continue
		}
		fmt.Fprintf(stdout, "  %s %s\n", d.Kind, d.Path)
	}
	return nil
}

func fmtTime(c *Change) string {
	return time.Unix(c.Time, 0).Format("Mon Jan 2 15:04:05 2006 ") + c.TZ
}

func printChangeHeader(r *Repo, id string, c *Change) {
	tag := ""
	if len(c.Parents) > 1 {
		tag = " (merge)"
	}
	fmt.Fprintf(stdout, "change %s%s\n", id, tag)
	fmt.Fprintf(stdout, "Author: %s\n", c.Author)
	fmt.Fprintf(stdout, "Date:   %s\n", fmtTime(c))
	if c.Meta.Agent != "" || c.Meta.Model != "" {
		fmt.Fprintf(stdout, "Agent:  %s %s\n", c.Meta.Agent, c.Meta.Model)
	}
	if c.Meta.Intent != "" {
		fmt.Fprintf(stdout, "Intent: %s\n", c.Meta.Intent)
	}
	if len(c.Meta.Checks) > 0 {
		keys := make([]string, 0, len(c.Meta.Checks))
		for k := range c.Meta.Checks {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		var parts []string
		for _, k := range keys {
			parts = append(parts, k+"="+c.Meta.Checks[k])
		}
		fmt.Fprintf(stdout, "Checks: %s\n", strings.Join(parts, " "))
	}
	if len(c.Conflicts) > 0 {
		fmt.Fprintf(stdout, "Conflicts: %d unresolved\n", len(c.Conflicts))
	}
	if c.Sig != nil {
		key, err := r.VerifyChange(c)
		who := key
		if name, ok := r.LoadConfig().Trusted[key]; ok {
			who = name + " (trusted)"
		}
		if err != nil {
			fmt.Fprintf(stdout, "Signed: INVALID (%v) %s\n", err, who)
		} else {
			fmt.Fprintf(stdout, "Signed: %s\n", who)
		}
	}
	for _, l := range strings.Split(c.Message, "\n") {
		fmt.Fprintf(stdout, "\n    %s", l)
	}
	fmt.Fprint(stdout, "\n\n")
}

func cmdLog(r *Repo, args []string) error {
	fs := newFlags("log")
	n := fs.Int("n", 20, "max changes")
	agent := fs.String("agent", "", "only changes by this agent")
	asJSON := fs.Bool("json", false, "machine-readable output")
	if err := fs.Parse(args); err != nil {
		return err
	}
	rev := "HEAD"
	if fs.NArg() > 0 {
		rev = fs.Arg(0)
	}
	start, err := r.ResolveRev(rev)
	if err != nil {
		return err
	}
	type item struct {
		ID     string  `json:"id"`
		Change *Change `json:"change"`
	}
	var items []item
	err = r.Walk(start, func(id string, c *Change) bool {
		if *agent == "" || c.Meta.Agent == *agent {
			items = append(items, item{id, c})
		}
		return len(items) < *n
	})
	if err != nil {
		return err
	}
	if *asJSON {
		if items == nil {
			items = []item{}
		}
		return printJSON(items)
	}
	for _, it := range items {
		printChangeHeader(r, it.ID, it.Change)
	}
	return nil
}

func (r *Repo) diffBetween(a, b Flat) (string, error) {
	var sb strings.Builder
	read := func(fe FileEnt) ([]byte, error) { return r.GetBlob(fe.ID) }
	for _, d := range withRenames(DiffFlat(a, b)) {
		if d.Kind == "R" {
			fmt.Fprintf(&sb, "renamed: %s -> %s\n", d.From, d.Path)
			if d.Old.Mode != d.New.Mode {
				fmt.Fprintf(&sb, "mode change: %s %s -> %s\n", d.Path, d.Old.Mode, d.New.Mode)
			}
			continue
		}
		var od, nd []byte
		var err error
		if d.Kind != "A" {
			if od, err = read(d.Old); err != nil {
				return "", err
			}
		}
		if d.Kind != "D" {
			if nd, err = read(d.New); err != nil {
				return "", err
			}
		}
		switch d.Kind {
		case "A":
			fmt.Fprintf(&sb, "added: %s (%s)\n", d.Path, d.New.Mode)
		case "D":
			fmt.Fprintf(&sb, "deleted: %s\n", d.Path)
		default:
			if d.Old.Mode != d.New.Mode {
				fmt.Fprintf(&sb, "mode change: %s %s -> %s\n", d.Path, d.Old.Mode, d.New.Mode)
			}
		}
		if isBinary(od) || isBinary(nd) {
			if d.Kind == "M" && d.Old.ID == d.New.ID {
				continue
			}
			fmt.Fprintf(&sb, "Binary files a/%s and b/%s differ\n", d.Path, d.Path)
			continue
		}
		sb.WriteString(UnifiedDiff(d.Path, splitLines(od), splitLines(nd), 3))
	}
	return sb.String(), nil
}

func cmdShow(r *Repo, args []string) error {
	rev := "HEAD"
	if len(args) > 0 {
		rev = args[0]
	}
	id, err := r.ResolveRev(rev)
	if err != nil {
		return err
	}
	c, err := r.GetChange(id)
	if err != nil {
		return err
	}
	printChangeHeader(r, id, c)
	var parent string
	if len(c.Parents) > 0 {
		parent = c.Parents[0]
	}
	a, err := r.ChangeFlat(parent)
	if err != nil {
		return err
	}
	b, err := r.Flatten(c.Tree)
	if err != nil {
		return err
	}
	out, err := r.diffBetween(a, b)
	fmt.Fprint(stdout, out)
	return err
}

func cmdDiff(r *Repo, args []string) error {
	var a, b Flat
	var err error
	switch len(args) {
	case 0:
		_, head := r.Head()
		if a, err = r.ChangeFlat(head); err != nil {
			return err
		}
		if b, err = r.Snapshot(true); err != nil {
			return err
		}
	case 1, 2:
		ida, err := r.ResolveRev(args[0])
		if err != nil {
			return err
		}
		if a, err = r.ChangeFlat(ida); err != nil {
			return err
		}
		if len(args) == 2 {
			idb, err := r.ResolveRev(args[1])
			if err != nil {
				return err
			}
			if b, err = r.ChangeFlat(idb); err != nil {
				return err
			}
		} else if b, err = r.Snapshot(true); err != nil {
			return err
		}
	default:
		return errors.New("usage: diff [revA [revB]]")
	}
	out, err := r.diffBetween(a, b)
	fmt.Fprint(stdout, out)
	return err
}

func cmdBranch(r *Repo, args []string) error {
	fs := newFlags("branch")
	del := fs.Bool("d", false, "delete the branch")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() == 0 {
		cur, _ := r.Head()
		var names []string
		for ref := range r.ListRefs() {
			if strings.HasPrefix(ref, "heads/") {
				names = append(names, strings.TrimPrefix(ref, "heads/"))
			}
		}
		if cur != "" {
			found := false
			for _, n := range names {
				found = found || "heads/"+n == cur
			}
			if !found {
				names = append(names, strings.TrimPrefix(cur, "heads/"))
			}
		}
		sort.Strings(names)
		for _, n := range names {
			mark := "  "
			if "heads/"+n == cur {
				mark = "* "
			}
			id, _ := r.ReadRef("heads/" + n)
			fmt.Fprintf(stdout, "%s%-20s %s\n", mark, n, short(id))
		}
		return nil
	}
	name := fs.Arg(0)
	if !validRefName("heads/" + name) {
		return fmt.Errorf("invalid branch name %q", name)
	}
	if *del {
		if cur, _ := r.Head(); cur == "heads/"+name {
			return errors.New("cannot delete the current branch")
		}
		return r.Do("branch -d", func() error { return r.DeleteRef("heads/" + name) })
	}
	if _, ok := r.ReadRef("heads/" + name); ok {
		return fmt.Errorf("branch %q already exists", name)
	}
	start := "HEAD"
	if fs.NArg() > 1 {
		start = fs.Arg(1)
	}
	id, err := r.ResolveRev(start)
	if err != nil {
		return err
	}
	return r.Do("branch "+name, func() error { return r.WriteRef("heads/"+name, id) })
}

func cmdCheckout(r *Repo, args []string) error {
	fs := newFlags("checkout")
	create := fs.Bool("b", false, "create the branch (at HEAD, or at the given start point) and switch to it")
	force := fs.Bool("f", false, "discard uncommitted changes")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() < 1 || fs.NArg() > 2 || (fs.NArg() == 2 && !*create) {
		return errors.New("usage: checkout [-f] <branch|rev>   or   checkout -b <name> [start]")
	}
	name, start := fs.Arg(0), fs.Arg(1)
	return r.Do("checkout "+name, func() error {
		if err := r.Checkout(name, *force, *create, start); err != nil {
			return err
		}
		fmt.Fprintf(stdout, "switched to %s\n", name)
		return nil
	})
}

func cmdMerge(r *Repo, args []string) error {
	fs := newFlags("merge")
	noff := fs.Bool("no-ff", false, "always create a merge change")
	msg := fs.String("m", "", "merge message")
	getMeta := metaFlags(fs)
	asJSON := fs.Bool("json", false, "machine-readable output")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() != 1 {
		return errors.New("usage: merge [--no-ff] [-m msg] <rev>")
	}
	meta, err := getMeta()
	if err != nil {
		return err
	}
	var out *MergeOutcome
	err = r.Do("merge "+fs.Arg(0), func() error {
		var err error
		out, err = r.Merge(fs.Arg(0), *noff, *msg, meta)
		return err
	})
	if err != nil {
		return err
	}
	if *asJSON {
		paths := []string{}
		for _, c := range out.Conflicts {
			paths = append(paths, c.Path)
		}
		if err := printJSON(map[string]any{"result": out.Kind, "change": out.Change, "conflicts": paths}); err != nil {
			return err
		}
	} else {
		switch out.Kind {
		case "up-to-date":
			fmt.Fprintln(stdout, "already up to date")
		case "fast-forward":
			fmt.Fprintf(stdout, "fast-forward to %s\n", short(out.Change))
		default:
			fmt.Fprintf(stdout, "merged as %s\n", short(out.Change))
		}
		for _, c := range out.Conflicts {
			fmt.Fprintf(stdout, "CONFLICT %s\n", c.Path)
		}
		if len(out.Conflicts) > 0 {
			fmt.Fprintln(stdout, "conflicts are recorded in the change; edit the files (or `sprig resolve --ours/--theirs`) and commit")
		}
	}
	if len(out.Conflicts) > 0 {
		return &exitError{3, ""}
	}
	return nil
}

func cmdConflicts(r *Repo, args []string) error {
	_, head := r.Head()
	if head == "" {
		return nil
	}
	c, err := r.GetChange(head)
	if err != nil {
		return err
	}
	for _, cf := range c.Conflicts {
		fmt.Fprintf(stdout, "%s\tbase=%s ours=%s theirs=%s\n", cf.Path, short(cf.Base), short(cf.Ours), short(cf.Theirs))
	}
	return nil
}

func cmdResolve(r *Repo, args []string) error {
	fs := newFlags("resolve")
	ours := fs.Bool("ours", false, "take our side")
	theirs := fs.Bool("theirs", false, "take their side")
	base := fs.Bool("base", false, "take the common ancestor's version")
	if err := fs.Parse(args); err != nil {
		return err
	}
	n := 0
	for _, b := range []bool{*ours, *theirs, *base} {
		if b {
			n++
		}
	}
	if n != 1 || fs.NArg() == 0 {
		return errors.New("usage: resolve (--ours|--theirs|--base) <path>...")
	}
	_, head := r.Head()
	if head == "" {
		return errors.New("no changes yet")
	}
	c, err := r.GetChange(head)
	if err != nil {
		return err
	}
	flat, err := r.Flatten(c.Tree)
	if err != nil {
		return err
	}
	for _, p := range fs.Args() {
		p = filepath.ToSlash(filepath.Clean(p))
		var cf *Conflict
		for i := range c.Conflicts {
			if c.Conflicts[i].Path == p {
				cf = &c.Conflicts[i]
			}
		}
		if cf == nil {
			return fmt.Errorf("%s has no recorded conflict", p)
		}
		pick := cf.Ours
		switch {
		case *theirs:
			pick = cf.Theirs
		case *base:
			pick = cf.Base
		}
		if pick == "" {
			// that side has no file: resolving to it means deleting
			if err := removeFile(r, p); err != nil {
				return err
			}
			fmt.Fprintf(stdout, "resolved %s (removed)\n", p)
			continue
		}
		mode := "file"
		if fe, ok := flat[p]; ok {
			mode = fe.Mode
		}
		if err := r.writeFile(p, FileEnt{mode, pick}); err != nil {
			return err
		}
		fmt.Fprintf(stdout, "resolved %s\n", p)
	}
	fmt.Fprintln(stdout, "commit to record the resolution")
	return nil
}

func removeFile(r *Repo, p string) error {
	if err := os.Remove(r.abs(p)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	r.pruneDirs(filepath.Dir(r.abs(p)))
	return nil
}

func cmdOps(r *Repo, args []string) error {
	ops, err := r.Ops()
	if err != nil {
		return err
	}
	for i := len(ops) - 1; i >= 0; i-- {
		o := ops[i]
		fmt.Fprintf(stdout, "%4d  %s  %s\n", o.N, time.Unix(o.Time, 0).Format("2006-01-02 15:04:05"), o.Cmd)
	}
	return nil
}

func cmdUndo(r *Repo, args []string) error {
	fs := newFlags("undo")
	force := fs.Bool("f", false, "discard uncommitted changes")
	if err := fs.Parse(args); err != nil {
		return err
	}
	op, err := r.Undo(*force)
	if err != nil {
		return err
	}
	fmt.Fprintf(stdout, "undid operation %d: %s\n", op.N, op.Cmd)
	return nil
}

var passValues = map[string]bool{"pass": true, "passed": true, "ok": true, "approved": true, "true": true}

func cmdAudit(r *Repo, args []string) error {
	fs := newFlags("audit")
	require := fs.String("require", "review", "check that must be present and passing")
	signed := fs.Bool("signed", false, "also require a valid signature from a trusted key (see `sprig trust`)")
	asJSON := fs.Bool("json", false, "machine-readable output")
	if err := fs.Parse(args); err != nil {
		return err
	}
	rev := "HEAD"
	if fs.NArg() > 0 {
		rev = fs.Arg(0)
	}
	start, err := r.ResolveRev(rev)
	if err != nil {
		return err
	}
	trusted := r.LoadConfig().Trusted
	if *signed && len(trusted) == 0 {
		return errors.New("no trusted keys; add one with `sprig trust <key> <name>`")
	}
	type finding struct {
		ID      string `json:"id"`
		Agent   string `json:"agent"`
		Model   string `json:"model,omitempty"`
		Message string `json:"message"`
		Problem string `json:"problem"`
	}
	found := []finding{}
	total := 0
	err = r.Walk(start, func(id string, c *Change) bool {
		if c.Meta.Agent == "" {
			return true
		}
		total++
		problem := ""
		if !passValues[strings.ToLower(c.Meta.Checks[*require])] {
			problem = "no passing " + *require + " check"
		}
		if *signed && problem == "" {
			if key, err := r.VerifyChange(c); err != nil {
				problem = "signature: " + err.Error()
			} else if _, ok := trusted[key]; !ok {
				problem = "signed by untrusted key " + key
			}
		}
		if problem != "" {
			found = append(found, finding{id, c.Meta.Agent, c.Meta.Model, firstLine(c.Message), problem})
		}
		return true
	})
	if err != nil {
		return err
	}
	if *asJSON {
		if err := printJSON(found); err != nil {
			return err
		}
	} else {
		what := fmt.Sprintf("a passing %q check", *require)
		if *signed {
			what += " or a trusted signature"
		}
		fmt.Fprintf(stdout, "%d of %d agent-authored changes lack %s\n", len(found), total, what)
		for _, f := range found {
			fmt.Fprintf(stdout, "  %s  %s  %s  (%s)\n", short(f.ID), f.Agent, f.Message, f.Problem)
		}
	}
	if len(found) > 0 {
		return &exitError{1, ""}
	}
	return nil
}

func cmdRemote(r *Repo, args []string) error {
	cfg := r.LoadConfig()
	if len(args) == 0 || args[0] == "list" {
		names := make([]string, 0, len(cfg.Remotes))
		for n := range cfg.Remotes {
			names = append(names, n)
		}
		sort.Strings(names)
		for _, n := range names {
			fmt.Fprintf(stdout, "%s\t%s\n", n, cfg.Remotes[n])
		}
		return nil
	}
	if args[0] == "add" && len(args) == 3 {
		if !regexp.MustCompile(`^[A-Za-z0-9_-]+$`).MatchString(args[1]) {
			return errors.New("invalid remote name")
		}
		cfg.Remotes[args[1]] = args[2]
		return r.SaveConfig(cfg)
	}
	return errors.New("usage: remote add <name> <url> | remote list")
}

func remoteArgs(r *Repo, fs *flag.FlagSet) (*Remote, string, error) {
	name, branch := "origin", ""
	if fs.NArg() > 0 {
		name = fs.Arg(0)
	}
	if fs.NArg() > 1 {
		branch = fs.Arg(1)
	}
	if branch == "" {
		ref, _ := r.Head()
		if ref == "" {
			return nil, "", errors.New("HEAD is detached; name a branch")
		}
		branch = strings.TrimPrefix(ref, "heads/")
	}
	rm, err := r.Remote(name)
	return rm, branch, err
}

func cmdPush(r *Repo, args []string) error {
	fs := newFlags("push")
	force := fs.Bool("f", false, "allow non-fast-forward updates and conflicted tips")
	if err := fs.Parse(args); err != nil {
		return err
	}
	rm, branch, err := remoteArgs(r, fs)
	if err != nil {
		return err
	}
	msg, err := r.Push(rm, branch, *force)
	if err != nil {
		return err
	}
	fmt.Fprintln(stdout, msg)
	return nil
}

func cmdLand(r *Repo, args []string) error {
	fs := newFlags("land")
	msg := fs.String("m", "", "message")
	getMeta := metaFlags(fs)
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *msg == "" {
		return errors.New("a message is required (-m)")
	}
	meta, err := getMeta()
	if err != nil {
		return err
	}
	rm, branch, err := remoteArgs(r, fs)
	if err != nil {
		return err
	}
	var res *LandResult
	err = r.Do("land "+rm.Name, func() error {
		var err error
		res, err = r.Land(rm, branch, *msg, meta)
		return err
	})
	if errors.Is(err, errNothingToCommit) {
		return &exitError{1, "nothing to land"}
	}
	if err != nil {
		return err
	}
	fmt.Fprintf(stdout, "[%s %s] %s (landed on %s)\n", branch, short(res.ID), firstLine(*msg), short(res.Parent))
	return nil
}

func cmdFetch(r *Repo, args []string, integrate bool) error {
	fs := newFlags("fetch")
	if err := fs.Parse(args); err != nil {
		return err
	}
	rm, branch, err := remoteArgs(r, fs)
	if err != nil {
		return err
	}
	var tip string
	var n int
	err = r.Do("fetch "+rm.Name, func() error {
		var err error
		tip, n, err = r.Fetch(rm, branch)
		return err
	})
	if err != nil {
		return err
	}
	fmt.Fprintf(stdout, "fetched %s/%s -> %s (%d objects)\n", rm.Name, branch, short(tip), n)
	if !integrate {
		return nil
	}
	ref, _ := r.Head()
	if ref != "heads/"+branch {
		fmt.Fprintf(stdout, "not on %s; run `sprig checkout %s` then `sprig merge %s/%s`\n", branch, branch, rm.Name, branch)
		return nil
	}
	return cmdMerge(r, []string{rm.Name + "/" + branch}) // fast-forwards when possible
}

func cmdImportGit(r *Repo, args []string) error {
	fs := newFlags("import-git")
	force := fs.Bool("force", false, "overwrite existing branches of the same name")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() != 1 {
		return errors.New("usage: import-git [--force] <path-to-git-repo>")
	}
	if r.Bare {
		return errors.New("run import-git in a repository with a working tree")
	}
	res, err := r.ImportGit(fs.Arg(0), *force)
	if err != nil {
		return err
	}
	names := make([]string, 0, len(res.Branches))
	for n := range res.Branches {
		names = append(names, n)
	}
	sort.Strings(names)
	fmt.Fprintf(stdout, "imported %d changes, %d file versions across %d branch(es)\n", res.Changes, res.Blobs, len(names))
	for _, n := range names {
		fmt.Fprintf(stdout, "  %-20s %s\n", n, short(res.Branches[n]))
	}
	if res.Skipped > 0 {
		fmt.Fprintf(stdout, "note: skipped %d submodule entries\n", res.Skipped)
	}
	for _, w := range res.Warnings {
		fmt.Fprintf(stdout, "warning: %s\n", w)
	}
	return nil
}

func cmdExportGit(r *Repo, args []string) error {
	fs := newFlags("export-git")
	force := fs.Bool("force", false, "allow non-fast-forward updates and conflicted tips")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if fs.NArg() != 1 {
		return errors.New("usage: export-git [--force] <path-to-git-repo>")
	}
	res, err := r.ExportGit(fs.Arg(0), *force)
	if err != nil {
		return err
	}
	names := make([]string, 0, len(res.Branches))
	for n := range res.Branches {
		names = append(names, n)
	}
	sort.Strings(names)
	fmt.Fprintf(stdout, "wrote %d commit(s) across %d branch(es)\n", res.Commits, len(names))
	for _, n := range names {
		fmt.Fprintf(stdout, "  %-20s %s\n", n, short(res.Branches[n]))
	}
	for _, w := range res.Warnings {
		fmt.Fprintf(stdout, "warning: %s\n", w)
	}
	return nil
}

// cmdSparse changes a working tree's view: files entering it are written
// (loading content on demand in a lazy clone), files leaving it are removed.
func cmdSparse(r *Repo, args []string) error {
	cfg := r.LoadConfig()
	if len(args) == 0 {
		if len(cfg.Sparse) == 0 {
			fmt.Fprintln(stdout, "(everything is checked out)")
		}
		for _, p := range cfg.Sparse {
			fmt.Fprintln(stdout, p)
		}
		return nil
	}
	if r.Bare {
		return errors.New("bare repository has no working tree")
	}
	next := cfg
	next.Sparse = nil
	for _, p := range args {
		p = strings.Trim(p, "/")
		if p == "." || p == "" {
			next.Sparse = nil // "." means everything
			break
		}
		if !validPath(p) {
			return fmt.Errorf("bad path %q", p)
		}
		next.Sparse = append(next.Sparse, p)
	}
	_, head := r.Head()
	hf, err := r.ChangeFlat(head)
	if err != nil {
		return err
	}
	if ok, d, err := r.workingMatches(hf); err != nil {
		return err
	} else if !ok {
		return fmt.Errorf("uncommitted changes (%d paths); commit them first", len(d))
	}
	added, removed := 0, 0
	for p, fe := range hf {
		switch was, now := cfg.inView(p), next.inView(p); {
		case now && !was:
			if err := r.writeFile(p, fe); err != nil {
				return err
			}
			added++
		case was && !now:
			a := r.abs(p)
			if err := os.Remove(a); err != nil && !errors.Is(err, os.ErrNotExist) {
				return err
			}
			r.pruneDirs(filepath.Dir(a))
			removed++
		}
	}
	if err := r.SaveConfig(next); err != nil {
		return err
	}
	fmt.Fprintf(stdout, "checked out %d more file(s), removed %d\n", added, removed)
	return nil
}

func cmdTrust(r *Repo, args []string) error {
	cfg := r.LoadConfig()
	switch {
	case len(args) == 0:
		for k, name := range cfg.Trusted {
			fmt.Fprintf(stdout, "%s\t%s\n", name, k)
		}
		return nil
	case len(args) == 2:
		if err := checkSig(&Signature{Key: args[0]}, nil); err != nil && err.Error() == "malformed key" {
			return fmt.Errorf("%q is not a sprig public key (expected ed25519:...)", args[0])
		}
		if cfg.Trusted == nil {
			cfg.Trusted = map[string]string{}
		}
		cfg.Trusted[args[0]] = args[1]
		return r.SaveConfig(cfg)
	}
	return errors.New("usage: trust [<key> <name>]")
}

func cmdCat(r *Repo, args []string) error {
	if len(args) != 1 {
		return errors.New("usage: cat <id>")
	}
	id := args[0]
	if !idRe.MatchString(id) {
		var err error
		if id, err = r.ResolveRev(id); err != nil {
			return err
		}
	}
	typ, data, err := r.GetObject(id)
	if err != nil {
		return err
	}
	fmt.Fprintf(os.Stderr, "(%s, %d bytes)\n", typ, len(data))
	_, err = stdout.Write(data)
	return err
}

func cmdServe(args []string) error {
	fs := newFlags("serve")
	addr := fs.String("addr", "127.0.0.1:7700", "listen address")
	token := fs.String("token", os.Getenv("SPRIG_TOKEN"), "require this bearer token")
	tokens := fs.String("tokens", "", `JSON file of scoped tokens: {"<token>": ["dir", ...]}`)
	upstream := fs.String("upstream", "", "run as a cache in front of this sprig server")
	if err := fs.Parse(args); err != nil {
		return err
	}
	dir := "."
	if fs.NArg() > 0 {
		dir = fs.Arg(0)
	}
	r, err := OpenRepo(dir)
	if err != nil {
		return err
	}
	s := NewServer(r, *token)
	if *tokens != "" {
		b, err := os.ReadFile(*tokens)
		if err != nil {
			return err
		}
		if err := json.Unmarshal(b, &s.scopes); err != nil {
			return fmt.Errorf("%s: %w", *tokens, err)
		}
		for tok, scope := range s.scopes {
			if len(tok) < 16 {
				return fmt.Errorf("%s: tokens must be at least 16 characters", *tokens)
			}
			for i, pre := range scope {
				pre = strings.Trim(pre, "/")
				if !validPath(pre) {
					return fmt.Errorf("%s: bad path %q", *tokens, scope[i])
				}
				scope[i] = pre
			}
		}
	}
	if *upstream != "" {
		if !r.Bare {
			return errors.New("a cache needs its own bare repository (sprig init --bare cache.sprig)")
		}
		s.upstream = newRemote("upstream", *upstream)
	}
	return serve(s, *addr)
}
