package main

import (
	"bufio"
	"bytes"
	"compress/zlib"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

// Packfiles. `sprig gc` moves loose objects into one pack file, storing each
// file version as a delta against the previous version of the same path when
// that is smaller. A pack is a sequence of zlib streams, each holding
// "<type> <base id or -> <len>\x00<payload>"; its .idx is a sorted list of
// 32-byte ids with 8-byte offsets. Every object read from a pack is still
// hash-verified, deltas included.

const (
	maxDeltaDepth = 20
	idxEntry      = 32 + 8
)

type pack struct {
	path string
	idx  []byte
}

type packSet struct {
	mu     sync.Mutex
	packs  []*pack
	loaded time.Time // packs dir mtime when last loaded
}

var packCache sync.Map // repo dir -> *packSet

func (r *Repo) packDir() string { return filepath.Join(r.Dir, "packs") }

// packList returns the repo's packs, re-reading the directory if it changed
// (another process may have run gc).
func (r *Repo) packList() []*pack {
	v, _ := packCache.LoadOrStore(r.Dir, &packSet{})
	ps := v.(*packSet)
	ps.mu.Lock()
	defer ps.mu.Unlock()
	st, err := os.Stat(r.packDir())
	if err != nil {
		return nil
	}
	if st.ModTime().Equal(ps.loaded) {
		return ps.packs
	}
	ents, _ := os.ReadDir(r.packDir())
	var packs []*pack
	for _, e := range ents {
		if !strings.HasSuffix(e.Name(), ".idx") {
			continue
		}
		idx, err := os.ReadFile(filepath.Join(r.packDir(), e.Name()))
		if err != nil || len(idx)%idxEntry != 0 {
			continue
		}
		packs = append(packs, &pack{strings.TrimSuffix(filepath.Join(r.packDir(), e.Name()), ".idx") + ".pack", idx})
	}
	ps.packs, ps.loaded = packs, st.ModTime()
	return packs
}

func (p *pack) find(raw []byte) (int64, bool) {
	n := len(p.idx) / idxEntry
	i := sort.Search(n, func(i int) bool { return bytes.Compare(p.idx[i*idxEntry:i*idxEntry+32], raw) >= 0 })
	if i < n && bytes.Equal(p.idx[i*idxEntry:i*idxEntry+32], raw) {
		return int64(binary.BigEndian.Uint64(p.idx[i*idxEntry+32:])), true
	}
	return 0, false
}

func (r *Repo) inPack(id string) (*pack, int64, bool) {
	raw, err := hex.DecodeString(id)
	if err != nil {
		return nil, 0, false
	}
	for _, p := range r.packList() {
		if off, ok := p.find(raw); ok {
			return p, off, true
		}
	}
	return nil, 0, false
}

// packedPrefix lists packed ids starting with a hex prefix.
func (r *Repo) packedPrefix(prefix string) []string {
	var out []string
	for _, p := range r.packList() {
		for i := 0; i+idxEntry <= len(p.idx); i += idxEntry {
			if id := hex.EncodeToString(p.idx[i : i+32]); strings.HasPrefix(id, prefix) {
				out = append(out, id)
			}
		}
	}
	return out
}

func (r *Repo) readPacked(id string, depth int) (string, []byte, error) {
	p, off, ok := r.inPack(id)
	if !ok {
		return "", nil, fmt.Errorf("object %s %w", short(id), errObjectNotFound)
	}
	if depth > maxDeltaDepth+1 {
		return "", nil, fmt.Errorf("object %s corrupt: delta chain too long", short(id))
	}
	f, err := os.Open(p.path)
	if err != nil {
		return "", nil, err
	}
	defer f.Close()
	zr, err := zlib.NewReader(io.NewSectionReader(f, off, 1<<62))
	if err != nil {
		return "", nil, fmt.Errorf("object %s corrupt: %w", short(id), err)
	}
	br := bufio.NewReader(zr)
	head, err := br.ReadString(0)
	if err != nil {
		return "", nil, fmt.Errorf("object %s corrupt: %w", short(id), err)
	}
	var typ, base string
	var n int
	if _, err := fmt.Sscanf(strings.TrimSuffix(head, "\x00"), "%s %s %d", &typ, &base, &n); err != nil || n < 0 || n > maxObjectSize {
		return "", nil, fmt.Errorf("object %s corrupt: bad pack header", short(id))
	}
	payload := make([]byte, n)
	if _, err := io.ReadFull(br, payload); err != nil {
		return "", nil, fmt.Errorf("object %s corrupt: %w", short(id), err)
	}
	data := payload
	if base != "-" {
		var bdata []byte
		if _, bdata, err = r.readLocalAt(base, depth+1); err != nil {
			return "", nil, err
		}
		if data, err = applyDelta(bdata, payload); err != nil {
			return "", nil, fmt.Errorf("object %s corrupt: %w", short(id), err)
		}
	}
	if HashObject(typ, data) != id {
		return "", nil, fmt.Errorf("object %s corrupt: hash mismatch", short(id))
	}
	return typ, data, nil
}

// Delta format: 'c' offset length (copy from base) or 'i' length bytes
// (insert), numbers as uvarints. Matching is by whole tokens: lines, also cut
// after '}' so that one-line JSON trees match entry by entry.
func splitTokens(b []byte) []string {
	var out []string
	for len(b) > 0 {
		i := bytes.IndexAny(b, "\n}")
		if i < 0 {
			out = append(out, string(b))
			break
		}
		out = append(out, string(b[:i+1]))
		b = b[i+1:]
	}
	return out
}

func makeDelta(base, target []byte) []byte {
	at := map[string]int{}
	for off, l := 0, splitTokens(base); len(l) > 0; l = l[1:] {
		if _, seen := at[l[0]]; !seen {
			at[l[0]] = off
		}
		off += len(l[0])
	}
	var out, ins []byte
	flushIns := func() {
		if len(ins) > 0 {
			out = append(out, 'i')
			out = binary.AppendUvarint(out, uint64(len(ins)))
			out = append(out, ins...)
			ins = ins[:0]
		}
	}
	lines := splitTokens(target)
	for i := 0; i < len(lines); {
		off, ok := at[lines[i]]
		if !ok || len(lines[i]) < 2 { // copying a lone newline costs more than inserting it
			ins = append(ins, lines[i]...)
			i++
			continue
		}
		n := 0
		for i < len(lines) && bytes.HasPrefix(base[off+n:], []byte(lines[i])) {
			n += len(lines[i])
			i++
		}
		flushIns()
		out = append(out, 'c')
		out = binary.AppendUvarint(out, uint64(off))
		out = binary.AppendUvarint(out, uint64(n))
	}
	flushIns()
	return out
}

func applyDelta(base, delta []byte) ([]byte, error) {
	var out []byte
	for len(delta) > 0 {
		op := delta[0]
		delta = delta[1:]
		a, k := binary.Uvarint(delta)
		if k <= 0 {
			return nil, errors.New("bad delta")
		}
		delta = delta[k:]
		switch op {
		case 'c':
			n, k := binary.Uvarint(delta)
			if k <= 0 || a+n > uint64(len(base)) {
				return nil, errors.New("bad delta copy")
			}
			delta = delta[k:]
			out = append(out, base[a:a+n]...)
		case 'i':
			if a > uint64(len(delta)) {
				return nil, errors.New("bad delta insert")
			}
			out = append(out, delta[:a]...)
			delta = delta[a:]
		default:
			return nil, errors.New("bad delta op")
		}
	}
	return out, nil
}

// pairVersions records, for every file and directory that differs between
// two trees, the old version as the delta base of the new one.
func (r *Repo) pairVersions(old, new string, prev map[string]string) {
	if old == new {
		return
	}
	if prev[new] == "" {
		prev[new] = old
	}
	ot, err1 := r.GetTree(old)
	nt, err2 := r.GetTree(new)
	if err1 != nil || err2 != nil {
		return
	}
	was := map[string]Entry{}
	for _, e := range ot.Entries {
		was[e.Name] = e
	}
	for _, e := range nt.Entries {
		o, ok := was[e.Name]
		switch {
		case !ok || o.ID == e.ID:
		case e.Mode == "dir" && o.Mode == "dir":
			r.pairVersions(o.ID, e.ID, prev)
		case e.Mode != "dir" && o.Mode != "dir" && prev[e.ID] == "":
			prev[e.ID] = o.ID
		}
	}
}

type GCResult struct {
	Packed, Deltas  int
	Before, After   int64 // bytes of object storage
	DeletedLooseErr int
}

// GC packs every loose object. Loose files are deleted only after the new
// pack has been written, synced and read back object by object.
func (r *Repo) GC() (*GCResult, error) {
	res := &GCResult{}
	var loose []string
	filepath.WalkDir(filepath.Join(r.Dir, "objects"), func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() || strings.HasPrefix(d.Name(), ".tmp-") {
			return nil
		}
		id := filepath.Base(filepath.Dir(p)) + d.Name()
		if isID(id) {
			loose = append(loose, id)
			if st, err := d.Info(); err == nil {
				res.Before += st.Size()
			}
		}
		return nil
	})
	for _, p := range r.packList() {
		if st, err := os.Stat(p.path); err == nil {
			res.Before += st.Size() + int64(len(p.idx))
		}
	}
	if len(loose) == 0 {
		res.After = res.Before
		return res, nil
	}
	sort.Strings(loose)

	// Delta bases: each file version's previous version along history.
	prev := map[string]string{}
	seen := map[string]bool{}
	for _, tip := range r.ListRefs() {
		r.Walk(tip, func(id string, c *Change) bool {
			if seen[id] || len(c.Parents) == 0 {
				seen[id] = true
				return true
			}
			seen[id] = true
			pc, err := r.GetChange(c.Parents[0])
			if err != nil {
				return true
			}
			r.pairVersions(pc.Tree, c.Tree, prev)
			return true
		})
	}
	// Chains are capped, and a revert (A -> B -> A) must not make two
	// versions each other's base, so cycles are cut.
	depth := map[string]int{}
	state := map[string]int{} // 1 = visiting, 2 = done
	var depthOf func(id string) int
	depthOf = func(id string) int {
		switch state[id] {
		case 1:
			return -1
		case 2:
			return depth[id]
		}
		state[id] = 1
		d := 0
		if b := prev[id]; b != "" {
			if bd := depthOf(b); bd >= 0 && bd < maxDeltaDepth {
				d = bd + 1
			} else {
				delete(prev, id) // store this one whole
			}
		}
		depth[id], state[id] = d, 2
		return d
	}

	if err := os.MkdirAll(r.packDir(), 0o755); err != nil {
		return nil, err
	}
	tmp, err := os.CreateTemp(r.packDir(), ".tmp-pack-")
	if err != nil {
		return nil, err
	}
	defer os.Remove(tmp.Name())
	bw := bufio.NewWriter(tmp)
	var off int64
	type ent struct {
		raw []byte
		off int64
	}
	var ents []ent
	for _, id := range loose {
		typ, data, err := r.getLocal(id)
		if err != nil {
			tmp.Close()
			return nil, err
		}
		base, payload := "-", data
		if b := prev[id]; b != "" && typ != TypeChange && depthOf(id) > 0 {
			if _, bdata, err := r.getLocal(b); err == nil {
				if d := makeDelta(bdata, data); len(d) < len(data)*3/4 {
					base, payload = b, d
					res.Deltas++
				}
			}
		}
		var buf bytes.Buffer
		zw, _ := zlib.NewWriterLevel(&buf, zlib.BestCompression)
		fmt.Fprintf(zw, "%s %s %d\x00", typ, base, len(payload))
		zw.Write(payload)
		zw.Close()
		raw, _ := hex.DecodeString(id)
		ents = append(ents, ent{raw, off})
		bw.Write(buf.Bytes())
		off += int64(buf.Len())
	}
	if err := bw.Flush(); err != nil {
		tmp.Close()
		return nil, err
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return nil, err
	}
	tmp.Close()
	sort.Slice(ents, func(i, j int) bool { return bytes.Compare(ents[i].raw, ents[j].raw) < 0 })
	idx := make([]byte, 0, len(ents)*idxEntry)
	for _, e := range ents {
		idx = append(idx, e.raw...)
		idx = binary.BigEndian.AppendUint64(idx, uint64(e.off))
	}
	name := filepath.Join(r.packDir(), "pack-"+HashObject("pack", idx)[:16])
	if err := os.Rename(tmp.Name(), name+".pack"); err != nil {
		return nil, err
	}
	if err := atomicWrite(name+".idx", idx); err != nil { // the .idx makes the pack visible
		return nil, err
	}
	// Read every object back from the pack before letting go of the loose copy.
	for _, id := range loose {
		if _, _, err := r.readPacked(id, 0); err != nil {
			return nil, fmt.Errorf("new pack failed verification, loose objects kept: %w", err)
		}
	}
	for _, id := range loose {
		if err := os.Remove(r.objPath(id)); err != nil {
			res.DeletedLooseErr++
		}
	}
	res.Packed = len(loose)
	filepath.WalkDir(filepath.Join(r.Dir, "objects"), func(p string, d fs.DirEntry, err error) error {
		if err == nil && d.IsDir() && p != filepath.Join(r.Dir, "objects") {
			os.Remove(p) // only succeeds when empty
		}
		return nil
	})
	for _, p := range r.packList() {
		if st, err := os.Stat(p.path); err == nil {
			res.After += st.Size() + int64(len(p.idx))
		}
	}
	return res, nil
}
