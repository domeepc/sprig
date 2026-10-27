package main

import (
	"container/heap"
	"errors"
	"fmt"
	"sort"
	"strings"
	"time"
)

// ---------------------------------------------------------------------------
// History walking

func (r *Repo) Ancestors(id string) (map[string]int, error) {
	dist := map[string]int{id: 0}
	queue := []string{id}
	for len(queue) > 0 {
		cur := queue[0]
		queue = queue[1:]
		c, err := r.GetChange(cur)
		if err != nil {
			return nil, err
		}
		for _, p := range c.Parents {
			if _, seen := dist[p]; !seen {
				dist[p] = dist[cur] + 1
				queue = append(queue, p)
			}
		}
	}
	return dist, nil
}

func (r *Repo) IsAncestor(anc, desc string) (bool, error) {
	if anc == desc {
		return true, nil
	}
	set, err := r.Ancestors(desc)
	if err != nil {
		return false, err
	}
	_, ok := set[anc]
	return ok, nil
}

// MergeBases returns every best common ancestor of a and b: common
// ancestors that are not themselves behind another one. Criss-cross
// histories have more than one.
func (r *Repo) MergeBases(a, b string) ([]string, error) {
	aset, err := r.Ancestors(a)
	if err != nil {
		return nil, err
	}
	seen := map[string]bool{b: true}
	queue := []string{b}
	var frontier []string
	for len(queue) > 0 {
		cur := queue[0]
		queue = queue[1:]
		if _, ok := aset[cur]; ok {
			frontier = append(frontier, cur)
			continue
		}
		c, err := r.GetChange(cur)
		if err != nil {
			return nil, err
		}
		for _, p := range c.Parents {
			if !seen[p] {
				seen[p] = true
				queue = append(queue, p)
			}
		}
	}
	var best []string
	for _, x := range frontier {
		behind := false
		for _, y := range frontier {
			if x == y {
				continue
			}
			if anc, err := r.IsAncestor(x, y); err != nil {
				return nil, err
			} else if anc {
				behind = true
				break
			}
		}
		if !behind {
			best = append(best, x)
		}
	}
	sort.Strings(best)
	return best, nil
}

// mergeBaseFlat is the tree to three-way merge a and b against. With several
// best common ancestors they are merged into a virtual base first, recursively,
// like git's recursive strategy. Conflicts inside the virtual base stay as
// markers, which then usually resolve against the two real sides.
func (r *Repo) mergeBaseFlat(a, b string, depth int) (Flat, error) {
	bases, err := r.MergeBases(a, b)
	if err != nil || len(bases) == 0 {
		return Flat{}, err
	}
	virtual, err := r.ChangeFlat(bases[0])
	if err != nil {
		return nil, err
	}
	for _, other := range bases[1:] {
		if depth >= 8 { // shortcut: very deep criss-cross nesting falls back to one base
			break
		}
		ob, err := r.ChangeFlat(other)
		if err != nil {
			return nil, err
		}
		under, err := r.mergeBaseFlat(bases[0], other, depth+1)
		if err != nil {
			return nil, err
		}
		if virtual, _, err = r.MergeFlat(under, virtual, ob, "base", "base"); err != nil {
			return nil, err
		}
	}
	return virtual, nil
}

type logItem struct {
	id string
	c  *Change
}
type logHeap []logItem

func (h logHeap) Len() int           { return len(h) }
func (h logHeap) Less(i, j int) bool { return h[i].c.Time > h[j].c.Time }
func (h logHeap) Swap(i, j int)      { h[i], h[j] = h[j], h[i] }
func (h *logHeap) Push(x any)        { *h = append(*h, x.(logItem)) }
func (h *logHeap) Pop() any {
	old := *h
	x := old[len(old)-1]
	*h = old[:len(old)-1]
	return x
}

// Walk visits every ancestor of start, newest first, until fn returns false.
func (r *Repo) Walk(start string, fn func(id string, c *Change) bool) error {
	c, err := r.GetChange(start)
	if err != nil {
		return err
	}
	h := &logHeap{{start, c}}
	seen := map[string]bool{start: true}
	for h.Len() > 0 {
		it := heap.Pop(h).(logItem)
		if !fn(it.id, it.c) {
			return nil
		}
		for _, p := range it.c.Parents {
			if seen[p] {
				continue
			}
			seen[p] = true
			pc, err := r.GetChange(p)
			if err != nil {
				return err
			}
			heap.Push(h, logItem{p, pc})
		}
	}
	return nil
}

// ---------------------------------------------------------------------------
// Merging trees

func mergeMode(base FileEnt, haveBase bool, o, t FileEnt) string {
	if !haveBase {
		return o.Mode
	}
	switch {
	case o.Mode == base.Mode:
		return t.Mode
	default:
		return o.Mode
	}
}

// MergeFlat three-way merges two flat trees against their base. Files that
// cannot be merged cleanly are written with conflict markers (text) or keep
// our side (binary/symlink/delete-vs-modify) and are reported as Conflicts.
func (r *Repo) MergeFlat(base, ours, theirs Flat, oursLabel, theirsLabel string) (Flat, []Conflict, error) {
	paths := map[string]bool{}
	for _, f := range []Flat{base, ours, theirs} {
		for p := range f {
			paths[p] = true
		}
	}
	sorted := make([]string, 0, len(paths))
	for p := range paths {
		sorted = append(sorted, p)
	}
	sort.Strings(sorted)

	out := Flat{}
	var conflicts []Conflict
	for _, p := range sorted {
		b, hasB := base[p]
		o, hasO := ours[p]
		t, hasT := theirs[p]
		switch {
		case hasO == hasT && o == t:
			if hasO {
				out[p] = o
			}
		case hasO == hasB && o == b:
			if hasT {
				out[p] = t
			}
		case hasT == hasB && t == b:
			if hasO {
				out[p] = o
			}
		case !hasO || !hasT:
			// one side deleted, the other changed it
			keep := o
			if hasT {
				keep = t
			}
			out[p] = keep
			cf := Conflict{Path: p, Result: keep.ID}
			if hasB {
				cf.Base = b.ID
			}
			if hasO {
				cf.Ours = o.ID
			}
			if hasT {
				cf.Theirs = t.ID
			}
			conflicts = append(conflicts, cf)
		default:
			// both sides changed
			mode := mergeMode(b, hasB, o, t)
			switch {
			case hasB && o.ID == b.ID:
				out[p] = FileEnt{mode, t.ID}
			case hasB && t.ID == b.ID:
				out[p] = FileEnt{mode, o.ID}
			case o.ID == t.ID:
				out[p] = FileEnt{mode, o.ID}
			default:
				fe, cf, err := r.mergeContent(p, b, hasB, o, t, mode, oursLabel, theirsLabel)
				if err != nil {
					return nil, nil, err
				}
				out[p] = fe
				if cf != nil {
					conflicts = append(conflicts, *cf)
				}
			}
		}
	}
	return out, conflicts, nil
}

func (r *Repo) mergeContent(p string, b FileEnt, hasB bool, o, t FileEnt, mode, oursLabel, theirsLabel string) (FileEnt, *Conflict, error) {
	od, err := r.GetBlob(o.ID)
	if err != nil {
		return FileEnt{}, nil, err
	}
	td, err := r.GetBlob(t.ID)
	if err != nil {
		return FileEnt{}, nil, err
	}
	var bd []byte
	if hasB {
		if bd, err = r.GetBlob(b.ID); err != nil {
			return FileEnt{}, nil, err
		}
	}
	cf := &Conflict{Path: p, Ours: o.ID, Theirs: t.ID}
	if hasB {
		cf.Base = b.ID
	}
	if o.Mode == "link" || t.Mode == "link" || isBinary(od) || isBinary(td) || isBinary(bd) {
		cf.Result = o.ID
		return FileEnt{o.Mode, o.ID}, cf, nil
	}
	res := Merge3(splitLines(bd), splitLines(od), splitLines(td), oursLabel, theirsLabel)
	id, err := r.PutBlob([]byte(strings.Join(res.Lines, "")))
	if err != nil {
		return FileEnt{}, nil, err
	}
	if res.Conflicts == 0 {
		return FileEnt{mode, id}, nil, nil
	}
	cf.Result = id
	return FileEnt{mode, id}, cf, nil
}

// ---------------------------------------------------------------------------
// Commands on history

type CommitOpts struct {
	Message    string
	Meta       Meta
	AllowEmpty bool
}

func (r *Repo) authorString() string {
	c := r.LoadConfig()
	return authorFrom(c)
}

func (r *Repo) newChange(parents []string, tree string, msg string, meta Meta, conflicts []Conflict) *Change {
	now := time.Now()
	return &Change{
		Parents:   parents,
		Tree:      tree,
		Author:    r.authorString(),
		Time:      now.Unix(),
		TZ:        now.Format("-0700"),
		Message:   msg,
		Meta:      meta,
		Conflicts: conflicts,
	}
}

var errNothingToCommit = errors.New("nothing to commit")

// Commit snapshots the working tree as a new change on the current branch.
func (r *Repo) Commit(o CommitOpts) (string, error) {
	_, head := r.Head()
	var parent *Change
	var parents []string
	if head != "" {
		var err error
		if parent, err = r.GetChange(head); err != nil {
			return "", err
		}
		parents = []string{head}
	}
	flat, err := r.Snapshot(true)
	if err != nil {
		return "", err
	}
	tree, err := r.BuildTree(flat)
	if err != nil {
		return "", err
	}
	if parent != nil && parent.Tree == tree && !o.AllowEmpty {
		return "", errNothingToCommit
	}
	// Carry forward conflicts that are still unresolved: the file is
	// untouched since the merge, or still contains conflict markers.
	var carried []Conflict
	if parent != nil {
		for _, cf := range parent.Conflicts {
			fe, ok := flat[cf.Path]
			if !ok {
				continue
			}
			if fe.ID == cf.Result {
				carried = append(carried, cf)
				continue
			}
			if data, err := r.GetBlob(fe.ID); err == nil && hasMarkers(data) {
				cf.Result = fe.ID
				carried = append(carried, cf)
			}
		}
	}
	id, err := r.PutChange(r.newChange(parents, tree, o.Message, o.Meta, carried))
	if err != nil {
		return "", err
	}
	return id, r.AdvanceHead(id)
}

type MergeOutcome struct {
	Kind      string // up-to-date | fast-forward | merged
	Change    string
	Conflicts []Conflict
}

// Merge integrates `target` into the current branch. A conflicted merge still
// completes: the conflicts are recorded in the new change and the working
// tree holds the marked-up files, so work can continue.
func (r *Repo) Merge(targetRev string, noFF bool, msg string, meta Meta) (*MergeOutcome, error) {
	ref, cur := r.Head()
	target, err := r.ResolveRev(targetRev)
	if err != nil {
		return nil, err
	}
	curFlat, err := r.ChangeFlat(cur) // empty for an unborn branch
	if err != nil {
		return nil, err
	}
	if ok, d, err := r.workingMatches(curFlat); err != nil {
		return nil, err
	} else if !ok {
		return nil, fmt.Errorf("uncommitted changes (%d paths); commit or discard them first", len(d))
	}
	if cur != "" {
		if anc, err := r.IsAncestor(target, cur); err != nil {
			return nil, err
		} else if anc {
			return &MergeOutcome{Kind: "up-to-date", Change: cur}, nil
		}
	}
	ff := cur == "" // an unborn branch simply adopts the target
	if !ff {
		var err error
		if ff, err = r.IsAncestor(cur, target); err != nil {
			return nil, err
		}
		ff = ff && !noFF
	}
	if ff {
		to, err := r.ChangeFlat(target)
		if err != nil {
			return nil, err
		}
		if err := r.Materialize(curFlat, to); err != nil {
			return nil, err
		}
		if err := r.AdvanceHead(target); err != nil {
			return nil, err
		}
		return &MergeOutcome{Kind: "fast-forward", Change: target}, nil
	}
	baseFlat, err := r.mergeBaseFlat(cur, target, 0)
	if err != nil {
		return nil, err
	}
	theirFlat, err := r.ChangeFlat(target)
	if err != nil {
		return nil, err
	}
	oursLabel := "ours"
	if ref != "" {
		oursLabel = strings.TrimPrefix(ref, "heads/")
	}
	merged, conflicts, err := r.MergeFlat(baseFlat, curFlat, theirFlat, oursLabel, targetRev)
	if err != nil {
		return nil, err
	}
	tree, err := r.BuildTree(merged)
	if err != nil {
		return nil, err
	}
	if msg == "" {
		msg = fmt.Sprintf("Merge %s into %s", targetRev, oursLabel)
	}
	id, err := r.PutChange(r.newChange([]string{cur, target}, tree, msg, meta, conflicts))
	if err != nil {
		return nil, err
	}
	if err := r.Materialize(curFlat, merged); err != nil {
		return nil, err
	}
	if err := r.AdvanceHead(id); err != nil {
		return nil, err
	}
	return &MergeOutcome{Kind: "merged", Change: id, Conflicts: conflicts}, nil
}

// Checkout moves HEAD to a branch (or a detached change) and updates the
// working tree. Without force it refuses to discard uncommitted work. With
// create, it makes branch `rev` at `start` (default: HEAD) and switches to it.
func (r *Repo) Checkout(rev string, force, create bool, start string) error {
	_, cur := r.Head()
	curFlat, err := r.ChangeFlat(cur)
	if err != nil {
		return err
	}
	var targetID, headRef string
	switch {
	case create:
		if !validRefName("heads/" + rev) {
			return fmt.Errorf("invalid branch name %q", rev)
		}
		if _, ok := r.ReadRef("heads/" + rev); ok {
			return fmt.Errorf("branch %q already exists", rev)
		}
		if start == "" && cur == "" {
			return errors.New("no changes yet; commit first or give a start point")
		}
		if targetID, err = r.ResolveRev(start); err != nil {
			return err
		}
		headRef = "heads/" + rev
	default:
		if id, ok := r.ReadRef("heads/" + rev); ok {
			headRef, targetID = "heads/"+rev, id
		} else if targetID, err = r.ResolveRev(rev); err != nil {
			return err
		}
	}
	targetFlat, err := r.ChangeFlat(targetID)
	if err != nil {
		return err
	}
	from := curFlat
	if ok, d, err := r.workingMatches(curFlat); err != nil {
		return err
	} else if !ok {
		if !force {
			return fmt.Errorf("uncommitted changes (%d paths); commit them or use -f to discard", len(d))
		}
		if from, err = r.Snapshot(false); err != nil {
			return err
		}
	}
	if err := r.Materialize(from, targetFlat); err != nil {
		return err
	}
	if create {
		if err := r.WriteRef(headRef, targetID); err != nil {
			return err
		}
	}
	if headRef != "" {
		return r.SetHeadRef(headRef)
	}
	return r.SetHeadDetached(targetID)
}

// Undo reverts the most recent recorded operation. Because undo is itself
// recorded, undoing it again redoes the original operation.
func (r *Repo) Undo(force bool) (*Op, error) {
	ops, err := r.Ops()
	if err != nil {
		return nil, err
	}
	if len(ops) == 0 {
		return nil, errors.New("nothing to undo")
	}
	last := ops[len(ops)-1]
	_, cur := r.Head()
	curFlat, err := r.ChangeFlat(cur)
	if err != nil {
		return nil, err
	}
	from := curFlat
	if ok, d, err := r.workingMatches(curFlat); err != nil {
		return nil, err
	} else if !ok {
		if !force {
			return nil, fmt.Errorf("uncommitted changes (%d paths); commit them or use -f to discard", len(d))
		}
		if from, err = r.Snapshot(false); err != nil {
			return nil, err
		}
	}
	// Work out the tree HEAD will have after restoring.
	var newHead string
	if strings.HasPrefix(last.Before.Head, "ref: ") {
		newHead = last.Before.Refs[strings.TrimPrefix(last.Before.Head, "ref: ")]
	} else {
		newHead = last.Before.Head
	}
	toFlat, err := r.ChangeFlat(newHead)
	if err != nil {
		return nil, err
	}
	err = r.Do("undo", func() error {
		if err := r.RestoreState(last.Before); err != nil {
			return err
		}
		return r.Materialize(from, toFlat)
	})
	return &last, err
}
