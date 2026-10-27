package main

import (
	"bufio"
	"bytes"
	"compress/zlib"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/klauspost/compress/zstd"
)

// Packfiles. `sprig gc` moves loose objects into a pack file, storing each
// file and directory version as a delta against its previous version when
// that is smaller. The .idx is a sorted list of 32-byte ids with 8-byte
// offsets. Every object read from a pack is still hash-verified, deltas
// included.
//
// A v2 pack starts with "SPRIGPK2". Each entry is a uvarint length, then a
// byte saying whether the rest is zstd-compressed, then (uncompressed) a type
// byte, an encoding byte, a base byte and the payload. The base byte is 0 for
// a whole object, 1 for a delta whose base is the entry at that uvarint
// offset in this pack (bases are written first), 2 for a delta whose base is
// a raw 32-byte id elsewhere. Encoding 1 is a tree in compact binary form
// (raw ids instead of hex JSON) that expands back to the exact original
// bytes. v1 packs (no magic) are zlib streams with a text header, and stay
// readable.

const (
	maxDeltaDepth = 20
	idxEntry      = 32 + 8
	packMagic     = "SPRIGPK2"
	repackAt      = 8    // more packs than this: gc merges them into one
	autoGCLoose   = 6400 // estimated loose objects that trigger a background gc
)

var (
	zstdEnc, _ = zstd.NewWriter(nil, zstd.WithEncoderLevel(zstd.SpeedBestCompression))
	zstdDec, _ = zstd.NewReader(nil)
)

type pack struct {
	path string
	idx  []byte
	v2   bool
}

type packSet struct {
	mu     sync.Mutex
	packs  []*pack
	loaded time.Time // packs dir mtime when last loaded
}

var packCache sync.Map // repo dir -> *packSet

func (r *Repo) packDir() string { return filepath.Join(r.Dir, "packs") }

// packList returns the repo's packs, re-reading the directory if it changed
// (another process may have run gc) or if reload is set.
func (r *Repo) packList(reload ...bool) []*pack {
	v, _ := packCache.LoadOrStore(r.Dir, &packSet{})
	ps := v.(*packSet)
	ps.mu.Lock()
	defer ps.mu.Unlock()
	st, err := os.Stat(r.packDir())
	if err != nil {
		return nil
	}
	if st.ModTime().Equal(ps.loaded) && len(reload) == 0 {
		return ps.packs
	}
	ents, _ := os.ReadDir(r.packDir())
	var packs []*pack
	for _, e := range ents {
		if !strings.HasSuffix(e.Name(), ".idx") {
			continue
		}
		if p, ok := loadPack(strings.TrimSuffix(filepath.Join(r.packDir(), e.Name()), ".idx")); ok {
			packs = append(packs, p)
		}
	}
	ps.packs, ps.loaded = packs, st.ModTime()
	return packs
}

func loadPack(name string) (*pack, bool) {
	idx, err := os.ReadFile(name + ".idx")
	if err != nil || len(idx)%idxEntry != 0 {
		return nil, false
	}
	f, err := os.Open(name + ".pack")
	if err != nil {
		return nil, false
	}
	defer f.Close()
	magic := make([]byte, len(packMagic))
	n, _ := io.ReadFull(f, magic)
	return &pack{name + ".pack", idx, n == len(packMagic) && string(magic) == packMagic}, true
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
	typ, data, err := r.readEntry(p, off, id, depth)
	if errors.Is(err, fs.ErrNotExist) { // a concurrent gc replaced that pack: look again
		r.packList(true)
		if p, off, ok = r.inPack(id); ok {
			return r.readEntry(p, off, id, depth)
		}
	}
	return typ, data, err
}

func (r *Repo) readEntry(p *pack, off int64, id string, depth int) (string, []byte, error) {
	if depth > maxDeltaDepth+1 {
		return "", nil, fmt.Errorf("object %s corrupt: delta chain too long", short(id))
	}
	f, err := os.Open(p.path)
	if err != nil {
		return "", nil, err
	}
	defer f.Close()
	corrupt := func(err error) error { return fmt.Errorf("object %s corrupt: %v", short(id), err) }
	var typ, enc string
	var data []byte
	var baseData func() ([]byte, error)
	if p.v2 {
		br := bufio.NewReader(io.NewSectionReader(f, off, 1<<62))
		n, err := binary.ReadUvarint(br)
		if err != nil || n < 1 || n > maxObjectSize*2 {
			return "", nil, corrupt(errors.New("bad entry length"))
		}
		body := make([]byte, n)
		if _, err := io.ReadFull(br, body); err != nil {
			return "", nil, corrupt(err)
		}
		if body[0] == 1 {
			if body, err = zstdDec.DecodeAll(body[1:], nil); err != nil {
				return "", nil, corrupt(err)
			}
		} else {
			body = body[1:]
		}
		if len(body) < 3 || int(body[0]) >= len(packTypes) || body[1] > 1 {
			return "", nil, corrupt(errors.New("bad entry header"))
		}
		typ, enc = packTypes[body[0]], [...]string{"raw", "ctree"}[body[1]]
		kind, rest := body[2], body[3:]
		switch kind {
		case 0:
			data = rest
		case 1:
			bo, k := binary.Uvarint(rest)
			if k <= 0 {
				return "", nil, corrupt(errors.New("bad base offset"))
			}
			data = rest[k:]
			baseData = func() ([]byte, error) {
				_, b, err := r.readEntry(p, int64(bo), "", depth+1)
				return b, err
			}
		case 2:
			if len(rest) < 32 {
				return "", nil, corrupt(errors.New("bad base id"))
			}
			base := hex.EncodeToString(rest[:32])
			data = rest[32:]
			baseData = func() ([]byte, error) {
				_, b, err := r.readLocalAt(base, depth+1)
				return b, err
			}
		default:
			return "", nil, corrupt(errors.New("bad base kind"))
		}
	} else {
		zr, err := zlib.NewReader(io.NewSectionReader(f, off, 1<<62))
		if err != nil {
			return "", nil, corrupt(err)
		}
		raw, err := io.ReadAll(io.LimitReader(zr, maxObjectSize*2))
		if err != nil {
			return "", nil, corrupt(err)
		}
		i := bytes.IndexByte(raw, 0)
		f3 := strings.Fields(string(raw[:max(i, 0)]))
		if i < 0 || len(f3) != 3 {
			return "", nil, corrupt(errors.New("bad header"))
		}
		if n, err := strconv.Atoi(f3[2]); err != nil || n != len(raw)-i-1 {
			return "", nil, corrupt(errors.New("bad length"))
		}
		typ, enc, data = f3[0], "raw", raw[i+1:]
		if base := f3[1]; base != "-" {
			baseData = func() ([]byte, error) {
				_, b, err := r.readLocalAt(base, depth+1)
				return b, err
			}
		}
	}
	if baseData != nil {
		bdata, err := baseData()
		if err != nil {
			return "", nil, err
		}
		if enc == "ctree" {
			var ok bool
			if bdata, ok = compactTree(bdata); !ok {
				return "", nil, corrupt(errors.New("delta base is not a compactable tree"))
			}
		}
		if data, err = applyDelta(bdata, data); err != nil {
			return "", nil, corrupt(err)
		}
	}
	if enc == "ctree" {
		if data, err = expandTree(data); err != nil {
			return "", nil, corrupt(err)
		}
	}
	if id == "" { // a base read by offset: the final object's hash check covers it
		return typ, data, nil
	}
	if HashObject(typ, data) != id {
		return "", nil, corrupt(errors.New("hash mismatch"))
	}
	return typ, data, nil
}

var packTypes = []string{TypeBlob, TypeTree, TypeChange}

// Compact trees: count, then per entry a mode byte, the name and the raw id.
var treeModes = []string{"file", "exec", "link", "dir"}

// compactTree encodes a tree, but only if expanding it gives back exactly
// data; anything unusual is simply stored as it is.
func compactTree(data []byte) ([]byte, bool) {
	t, err := parseTree(data)
	if err != nil {
		return nil, false
	}
	out := binary.AppendUvarint(nil, uint64(len(t.Entries)))
	for _, e := range t.Entries {
		m := slicesIndex(treeModes, e.Mode)
		raw, err := hex.DecodeString(e.ID)
		if m < 0 || err != nil {
			return nil, false
		}
		out = append(out, byte(m))
		out = binary.AppendUvarint(out, uint64(len(e.Name)))
		out = append(out, e.Name...)
		out = append(out, raw...)
	}
	if back, err := expandTree(out); err != nil || !bytes.Equal(back, data) {
		return nil, false
	}
	return out, true
}

func expandTree(b []byte) ([]byte, error) {
	n, k := binary.Uvarint(b)
	if k <= 0 || n > uint64(len(b)) {
		return nil, errors.New("bad compact tree")
	}
	b = b[k:]
	t := &Tree{Entries: make([]Entry, 0, n)}
	for range n {
		if len(b) < 1 || int(b[0]) >= len(treeModes) {
			return nil, errors.New("bad compact tree entry")
		}
		mode := treeModes[b[0]]
		l, k := binary.Uvarint(b[1:])
		if k <= 0 || uint64(len(b)) < 1+uint64(k)+l+32 {
			return nil, errors.New("bad compact tree entry")
		}
		name := string(b[1+k : 1+k+int(l)])
		id := hex.EncodeToString(b[1+k+int(l) : 1+k+int(l)+32])
		t.Entries = append(t.Entries, Entry{name, mode, id})
		b = b[1+k+int(l)+32:]
	}
	if len(b) != 0 {
		return nil, errors.New("trailing bytes in compact tree")
	}
	return json.Marshal(t) // as PutTree does: entries already in stored order
}

func slicesIndex(list []string, s string) int {
	for i, v := range list {
		if v == s {
			return i
		}
	}
	return -1
}

// Delta format: 'c' offset length (copy from base) or 'i' length bytes
// (insert), numbers as uvarints.
func appendCopy(out []byte, off, n int) []byte {
	out = append(out, 'c')
	out = binary.AppendUvarint(out, uint64(off))
	return binary.AppendUvarint(out, uint64(n))
}

func appendInsert(out, data []byte) []byte {
	if len(data) == 0 {
		return out
	}
	out = append(out, 'i')
	out = binary.AppendUvarint(out, uint64(len(data)))
	return append(out, data...)
}

// bestDelta tries both encoders and keeps the smaller result.
func bestDelta(base, target []byte) []byte {
	a, b := makeDelta(base, target), blockDelta(base, target)
	if len(b) < len(a) {
		return b
	}
	return a
}

// makeDelta matches whole tokens: lines, also cut after '}'. Good for source.
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
		out = appendInsert(out, ins)
		ins = ins[:0]
		out = appendCopy(out, off, n)
	}
	return appendInsert(out, ins)
}

// blockDelta finds matches at any byte position: the base is indexed by
// 16-byte blocks and the target scanned with a rolling hash, each hit then
// extended both ways. Good for binary data and compact trees.
const deltaBlock = 16

func blockDelta(base, target []byte) []byte {
	if len(base) < deltaBlock || len(target) < deltaBlock {
		return appendInsert(nil, target)
	}
	const prime = 16777619
	pow := uint32(1)
	for range deltaBlock - 1 {
		pow *= prime
	}
	hash := func(b []byte) uint32 {
		var h uint32
		for _, c := range b[:deltaBlock] {
			h = h*prime + uint32(c)
		}
		return h
	}
	at := map[uint32]int{}
	for i := 0; i+deltaBlock <= len(base); i += deltaBlock {
		if h := hash(base[i:]); at[h] == 0 {
			at[h] = i + 1 // +1 so that 0 means absent
		}
	}
	var out []byte
	pending, i := 0, 0
	h := hash(target)
	for i+deltaBlock <= len(target) {
		if j := at[h] - 1; j >= 0 && bytes.Equal(base[j:j+deltaBlock], target[i:i+deltaBlock]) {
			for j > 0 && i > pending && base[j-1] == target[i-1] {
				i, j = i-1, j-1
			}
			n := deltaBlock
			for j+n < len(base) && i+n < len(target) && base[j+n] == target[i+n] {
				n++
			}
			out = appendInsert(out, target[pending:i])
			out = appendCopy(out, j, n)
			i += n
			pending = i
			if i+deltaBlock <= len(target) {
				h = hash(target[i:])
			}
			continue
		}
		if i+deltaBlock < len(target) {
			h = (h-uint32(target[i])*pow)*prime + uint32(target[i+deltaBlock])
		}
		i++
	}
	return appendInsert(out, target[pending:])
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
	Repacked        int // old packs merged into the new one
	Before, After   int64
	DeletedLooseErr int
}

// lockGC makes sure only one gc runs at a time; a lock older than an hour is
// taken to be left over from a crash.
func (r *Repo) lockGC() (func(), error) {
	p := filepath.Join(r.Dir, "gc.lock")
	for try := 0; try < 2; try++ {
		f, err := os.OpenFile(p, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o644)
		if err == nil {
			fmt.Fprintf(f, "%d\n", os.Getpid())
			f.Close()
			return func() { os.Remove(p) }, nil
		}
		if st, serr := os.Stat(p); serr == nil && time.Since(st.ModTime()) > time.Hour {
			os.Remove(p)
			continue
		}
		return nil, errors.New("another gc is running (remove .sprig/gc.lock if not)")
	}
	return nil, errors.New("cannot take gc.lock")
}

func (r *Repo) storageSize() int64 {
	var n int64
	filepath.WalkDir(filepath.Join(r.Dir, "objects"), func(p string, d fs.DirEntry, err error) error {
		if err == nil && !d.IsDir() {
			if st, err := d.Info(); err == nil {
				n += st.Size()
			}
		}
		return nil
	})
	for _, p := range r.packList() {
		if st, err := os.Stat(p.path); err == nil {
			n += st.Size() + int64(len(p.idx))
		}
	}
	return n
}

// GC packs every loose object, and with repack (or once there are many
// packs) everything in the old packs too. Nothing is deleted until the new
// pack has been written, synced and read back object by object.
func (r *Repo) GC(repack bool) (*GCResult, error) {
	unlock, err := r.lockGC()
	if err != nil {
		return nil, err
	}
	defer unlock()
	res := &GCResult{Before: r.storageSize()}
	old := r.packList(true)
	repack = repack || len(old) > repackAt
	var ids []string
	filepath.WalkDir(filepath.Join(r.Dir, "objects"), func(p string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() || strings.HasPrefix(d.Name(), ".tmp-") {
			return nil
		}
		if id := filepath.Base(filepath.Dir(p)) + d.Name(); isID(id) {
			ids = append(ids, id)
		}
		return nil
	})
	loose := len(ids)
	if repack {
		seen := map[string]bool{}
		for _, id := range ids {
			seen[id] = true
		}
		for _, p := range old {
			for i := 0; i+idxEntry <= len(p.idx); i += idxEntry {
				if id := hex.EncodeToString(p.idx[i : i+32]); !seen[id] {
					seen[id] = true
					ids = append(ids, id)
				}
			}
		}
	}
	if len(ids) == 0 || (!repack && loose == 0) {
		res.After = res.Before
		return res, nil
	}
	sort.Strings(ids)

	// Delta bases: each file and directory version's previous version.
	prev := map[string]string{}
	seen := map[string]bool{}
	for _, tip := range r.ListRefs() {
		r.Walk(tip, func(id string, c *Change) bool {
			if seen[id] {
				return true
			}
			seen[id] = true
			if len(c.Parents) > 0 {
				if pc, err := r.GetChange(c.Parents[0]); err == nil {
					r.pairVersions(pc.Tree, c.Tree, prev)
				}
			}
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
	bw.WriteString(packMagic)
	off := int64(len(packMagic))
	type ent struct {
		raw []byte
		off int64
	}
	var ents []ent
	// Write bases before the deltas built on them, so a delta can name its
	// base by offset.
	for _, id := range ids {
		depthOf(id)
	}
	sort.SliceStable(ids, func(i, j int) bool { return depth[ids[i]] < depth[ids[j]] })
	written := map[string]int64{}
	for _, id := range ids {
		typ, data, err := r.getLocal(id)
		if err != nil {
			tmp.Close()
			return nil, err
		}
		encByte, body := byte(0), data
		if typ == TypeTree {
			if c, ok := compactTree(data); ok {
				encByte, body = 1, c
			}
		}
		head := []byte{byte(slicesIndex(packTypes, typ)), encByte, 0}
		payload := body
		if b := prev[id]; b != "" && typ != TypeChange && depth[id] > 0 {
			if btyp, bdata, err := r.getLocal(b); err == nil && btyp == typ {
				ok := true
				if encByte == 1 {
					bdata, ok = compactTree(bdata)
				}
				if d := bestDelta(bdata, body); ok && len(d) < len(body)*3/4 {
					if bo, here := written[b]; here {
						head = binary.AppendUvarint(append(head[:2], 1), uint64(bo))
					} else {
						braw, _ := hex.DecodeString(b)
						head = append(append(head[:2], 2), braw...)
					}
					payload = d
					res.Deltas++
				}
			}
		}
		plain := append(head, payload...)
		entry := append([]byte{0}, plain...)
		if z := zstdEnc.EncodeAll(plain, nil); len(z) < len(plain) {
			entry = append([]byte{1}, z...)
		}
		raw, _ := hex.DecodeString(id)
		ents = append(ents, ent{raw, off})
		written[id] = off
		n := binary.AppendUvarint(nil, uint64(len(entry)))
		bw.Write(n)
		bw.Write(entry)
		off += int64(len(n) + len(entry))
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
	// Read every object back from the new pack itself before letting go of
	// any other copy.
	np, ok := loadPack(name)
	if !ok {
		return nil, errors.New("new pack unreadable, nothing removed")
	}
	for _, e := range ents {
		id := hex.EncodeToString(e.raw)
		if _, _, err := r.readEntry(np, e.off, id, 0); err != nil {
			return nil, fmt.Errorf("new pack failed verification, nothing removed: %w", err)
		}
	}
	for _, id := range ids {
		if err := os.Remove(r.objPath(id)); err != nil && !errors.Is(err, fs.ErrNotExist) {
			res.DeletedLooseErr++
		}
	}
	if repack {
		for _, p := range old {
			if p.path == name+".pack" {
				continue
			}
			os.Remove(strings.TrimSuffix(p.path, ".pack") + ".idx") // idx first: the pack stops being visible
			os.Remove(p.path)
			res.Repacked++
		}
	}
	res.Packed = len(ids)
	filepath.WalkDir(filepath.Join(r.Dir, "objects"), func(p string, d fs.DirEntry, err error) error {
		if err == nil && d.IsDir() && p != filepath.Join(r.Dir, "objects") {
			os.Remove(p) // only succeeds when empty
		}
		return nil
	})
	r.packList(true)
	res.After = r.storageSize()
	return res, nil
}

// startGC runs `sprig gc --auto` in the background; tests replace it.
var startGC = func(r *Repo) {
	exe, err := os.Executable()
	if err != nil {
		return
	}
	cmd := exec.Command(exe, "gc", "--auto")
	cmd.Dir = r.Dir
	if !r.Bare {
		cmd.Dir = r.Root
	}
	if cmd.Start() == nil {
		go cmd.Wait() // reap it if we are still around (servers); a CLI just exits
	}
}

// needsGC estimates loose objects from one fan-out directory (git's trick)
// and counts packs.
func (r *Repo) needsGC() bool {
	ents, _ := os.ReadDir(filepath.Join(r.Dir, "objects", "17"))
	return len(ents)*256 >= autoGCLoose || len(r.packList()) > repackAt
}

// maybeAutoGC starts a background gc when objects have piled up.
func (r *Repo) maybeAutoGC() {
	if os.Getenv("SPRIG_NO_AUTOGC") == "" && r.needsGC() {
		startGC(r)
	}
}
