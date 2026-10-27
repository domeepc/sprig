package main

import (
	"bytes"
	"fmt"
	"strings"
)

func splitLines(b []byte) []string {
	if len(b) == 0 {
		return nil
	}
	var out []string
	for len(b) > 0 {
		i := bytes.IndexByte(b, '\n')
		if i < 0 {
			out = append(out, string(b))
			break
		}
		out = append(out, string(b[:i+1]))
		b = b[i+1:]
	}
	return out
}

func isBinary(b []byte) bool {
	n := len(b)
	if n > 8000 {
		n = 8000
	}
	return bytes.IndexByte(b[:n], 0) >= 0
}

type match struct{ a, b, n int }

// maxLCS bounds the quadratic LCS table; larger middles are treated as a
// wholesale replacement (correct, just not minimal).
const maxLCS = 4_000_000

// matchingBlocks returns the maximal runs of identical lines shared by a
// and b, in order. It is the primitive both diff and merge are built on.
func matchingBlocks(a, b []string) []match {
	ids := map[string]int32{}
	intern := func(ss []string) []int32 {
		out := make([]int32, len(ss))
		for i, s := range ss {
			id, ok := ids[s]
			if !ok {
				id = int32(len(ids))
				ids[s] = id
			}
			out[i] = id
		}
		return out
	}
	ai, bi := intern(a), intern(b)

	pre := 0
	for pre < len(ai) && pre < len(bi) && ai[pre] == bi[pre] {
		pre++
	}
	suf := 0
	for suf < len(ai)-pre && suf < len(bi)-pre && ai[len(ai)-1-suf] == bi[len(bi)-1-suf] {
		suf++
	}
	var pairs [][2]int
	for i := 0; i < pre; i++ {
		pairs = append(pairs, [2]int{i, i})
	}
	ma, mb := ai[pre:len(ai)-suf], bi[pre:len(bi)-suf]
	if len(ma) > 0 && len(mb) > 0 && len(ma)*len(mb) <= maxLCS {
		w := len(mb) + 1
		L := make([]int32, (len(ma)+1)*w)
		for i := len(ma) - 1; i >= 0; i-- {
			for j := len(mb) - 1; j >= 0; j-- {
				if ma[i] == mb[j] {
					L[i*w+j] = L[(i+1)*w+j+1] + 1
				} else if L[(i+1)*w+j] >= L[i*w+j+1] {
					L[i*w+j] = L[(i+1)*w+j]
				} else {
					L[i*w+j] = L[i*w+j+1]
				}
			}
		}
		i, j := 0, 0
		for i < len(ma) && j < len(mb) {
			switch {
			case ma[i] == mb[j]:
				pairs = append(pairs, [2]int{pre + i, pre + j})
				i++
				j++
			case L[(i+1)*w+j] >= L[i*w+j+1]:
				i++
			default:
				j++
			}
		}
	}
	for k := suf; k > 0; k-- {
		pairs = append(pairs, [2]int{len(ai) - k, len(bi) - k})
	}
	var blocks []match
	for _, p := range pairs {
		if n := len(blocks); n > 0 && blocks[n-1].a+blocks[n-1].n == p[0] && blocks[n-1].b+blocks[n-1].n == p[1] {
			blocks[n-1].n++
		} else {
			blocks = append(blocks, match{p[0], p[1], 1})
		}
	}
	return blocks
}

type edit struct {
	op   byte // ' ', '-', '+'
	line string
}

func diffEdits(a, b []string) []edit {
	var out []edit
	ai, bi := 0, 0
	blocks := append(matchingBlocks(a, b), match{len(a), len(b), 0})
	for _, m := range blocks {
		for ; ai < m.a; ai++ {
			out = append(out, edit{'-', a[ai]})
		}
		for ; bi < m.b; bi++ {
			out = append(out, edit{'+', b[bi]})
		}
		for k := 0; k < m.n; k++ {
			out = append(out, edit{' ', a[ai]})
			ai++
			bi++
		}
	}
	return out
}

func writeDiffLine(sb *strings.Builder, op byte, line string) {
	sb.WriteByte(op)
	sb.WriteString(line)
	if !strings.HasSuffix(line, "\n") {
		sb.WriteString("\n\\ No newline at end of file\n")
	}
}

// UnifiedDiff renders a unified diff with `ctx` lines of context. It returns
// "" when there is no difference.
// hunks groups the non-equal edits into hunks with ctx lines of context,
// returning each hunk's [start, stop) range in edits.
func hunks(edits []edit, ctx int) [][2]int {
	var out [][2]int
	n := len(edits)
	i := 0
	for i < n {
		if edits[i].op == ' ' {
			i++
			continue
		}
		start := max(i-ctx, 0)
		end, j := i, i
		for j < n {
			if edits[j].op != ' ' {
				end = j
				j++
				continue
			}
			k := j
			for k < n && edits[k].op == ' ' {
				k++
			}
			if k == n || k-j > 2*ctx {
				break
			}
			j = k
		}
		stop := min(end+1+ctx, n)
		out = append(out, [2]int{start, stop})
		i = stop
	}
	return out
}

// UnifiedDiff renders a diff; hunks are numbered from 1 so that
// `commit --hunk path:N` can pick them.
func UnifiedDiff(name string, a, b []string, ctx int) string {
	edits := diffEdits(a, b)
	n := len(edits)
	aPos := make([]int, n+1)
	bPos := make([]int, n+1)
	for k, e := range edits {
		aPos[k+1], bPos[k+1] = aPos[k], bPos[k]
		if e.op != '+' {
			aPos[k+1]++
		}
		if e.op != '-' {
			bPos[k+1]++
		}
	}
	var sb strings.Builder
	for h, r := range hunks(edits, ctx) {
		start, stop := r[0], r[1]
		if sb.Len() == 0 {
			fmt.Fprintf(&sb, "--- a/%s\n+++ b/%s\n", name, name)
		}
		aCount, bCount := aPos[stop]-aPos[start], bPos[stop]-bPos[start]
		aStart, bStart := aPos[start]+1, bPos[start]+1
		if aCount == 0 {
			aStart--
		}
		if bCount == 0 {
			bStart--
		}
		fmt.Fprintf(&sb, "@@ -%d,%d +%d,%d @@ hunk %d\n", aStart, aCount, bStart, bCount, h+1)
		for _, e := range edits[start:stop] {
			writeDiffLine(&sb, e.op, e.line)
		}
	}
	return sb.String()
}

// applyHunks returns a with only the chosen hunks (numbered from 1, as
// UnifiedDiff prints them with 3 lines of context) of the change to b applied.
func applyHunks(a, b []string, keep map[int]bool) ([]string, error) {
	edits := diffEdits(a, b)
	hs := hunks(edits, 3)
	for n := range keep {
		if n < 1 || n > len(hs) {
			return nil, fmt.Errorf("no hunk %d (there are %d)", n, len(hs))
		}
	}
	in := make([]bool, len(edits))
	for h, r := range hs {
		if keep[h+1] {
			for k := r[0]; k < r[1]; k++ {
				in[k] = true
			}
		}
	}
	var out []string
	for k, e := range edits {
		switch {
		case e.op == ' ', e.op == '-' && !in[k], e.op == '+' && in[k]:
			out = append(out, e.line)
		}
	}
	return out, nil
}

// ---------------------------------------------------------------------------
// Three-way merge

type MergeResult struct {
	Lines     []string
	Conflicts int
}

func linesEq(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func ensureNL(lines []string) []string {
	if n := len(lines); n > 0 && !strings.HasSuffix(lines[n-1], "\n") {
		out := append([]string(nil), lines...)
		out[n-1] += "\n"
		return out
	}
	return lines
}

// Merge3 merges `ours` and `theirs`, both derived from `base`. Regions that
// only one side touched are taken from that side; regions both sides changed
// differently are emitted between conflict markers.
func Merge3(base, ours, theirs []string, oursLabel, theirsLabel string) MergeResult {
	ma := matchingBlocks(base, ours)
	mb := matchingBlocks(base, theirs)
	type region struct{ b, o, t, n int }
	var regions []region
	i, j := 0, 0
	for i < len(ma) && j < len(mb) {
		x, y := ma[i], mb[j]
		lo, hi := max(x.a, y.a), min(x.a+x.n, y.a+y.n)
		if lo < hi {
			regions = append(regions, region{lo, x.b + (lo - x.a), y.b + (lo - y.a), hi - lo})
		}
		if x.a+x.n < y.a+y.n {
			i++
		} else {
			j++
		}
	}
	regions = append(regions, region{len(base), len(ours), len(theirs), 0})

	var res MergeResult
	bp, op, tp := 0, 0, 0
	for _, r := range regions {
		bc, oc, tc := base[bp:r.b], ours[op:r.o], theirs[tp:r.t]
		switch {
		case len(bc) == 0 && len(oc) == 0 && len(tc) == 0:
		case linesEq(oc, bc):
			res.Lines = append(res.Lines, tc...)
		case linesEq(tc, bc), linesEq(oc, tc):
			res.Lines = append(res.Lines, oc...)
		default:
			res.Conflicts++
			res.Lines = append(res.Lines, "<<<<<<< "+oursLabel+"\n")
			res.Lines = append(res.Lines, ensureNL(oc)...)
			res.Lines = append(res.Lines, "=======\n")
			res.Lines = append(res.Lines, ensureNL(tc)...)
			res.Lines = append(res.Lines, ">>>>>>> "+theirsLabel+"\n")
		}
		res.Lines = append(res.Lines, ours[r.o:r.o+r.n]...)
		bp, op, tp = r.b+r.n, r.o+r.n, r.t+r.n
	}
	return res
}
