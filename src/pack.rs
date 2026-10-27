//! Packfiles. `sprig gc` moves loose objects into a pack file, storing each
//! file and directory version as a delta against its previous version when
//! that is smaller. The .idx is a sorted list of 32-byte ids with 8-byte
//! offsets. Every object read from a pack is still hash-verified, deltas
//! included.
//!
//! A v2 pack starts with "SPRIGPK2". Each entry is a uvarint length, then a
//! byte saying whether the rest is zstd-compressed, then (uncompressed) a
//! type byte, an encoding byte, a base byte and the payload. The base byte is
//! 0 for a whole object, 1 for a delta whose base is the entry at that
//! uvarint offset in this pack (bases are written first), 2 for a delta whose
//! base is a raw 32-byte id elsewhere. Encoding 1 is a tree in compact binary
//! form (raw ids instead of hex JSON) that expands back to the exact original
//! bytes. v1 packs (no magic) are zlib streams with a text header, and stay
//! readable.

use crate::store::{hash_object, is_id, obj_type, short, write_file_atomic, Repo, BLOB, CHANGE, TREE};
use crate::tree::{parse_tree, Entry, Mode, Tree};
use crate::util::{hex, put_uvarint, raw_id, uvarint, Error, Result};
use crate::{bail, err};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

const MAX_DELTA_DEPTH: usize = 20;
const IDX_ENTRY: usize = 32 + 8;
const PACK_MAGIC: &[u8] = b"SPRIGPK2";
/// more packs than this: gc merges them into one
pub const REPACK_AT: usize = 8;
/// estimated loose objects that trigger a background gc
pub const AUTO_GC_LOOSE: usize = 6400;
pub const MAX_OBJECT_SIZE: usize = 256 << 20;
const PACK_TYPES: [&str; 3] = [BLOB, TREE, CHANGE];
const TREE_MODES: [Mode; 4] = [Mode::File, Mode::Exec, Mode::Link, Mode::Dir];

// ---------------------------------------------------------------------------
// A per-repository cache of decoded objects. Objects are immutable and
// addressed by content, so a cached copy can never be stale. It matters most
// for packs, where reading one object can mean decoding a chain of deltas:
// with the cache, each base in a chain is decoded once per process.

#[derive(Clone)]
pub(crate) struct Obj {
    pub typ: &'static str,
    pub data: Arc<[u8]>,
    /// the compact form of a tree read from a pack, which deltas apply to
    pub body: Option<Arc<[u8]>>,
}

/// A pack entry in the form deltas against it apply to: whole objects, and
/// trees in compact form. Only the object finally asked for is expanded and
/// hash-checked; the bases in its delta chain stay as they are.
#[derive(Clone)]
struct PackEntry {
    typ: &'static str,
    ctree: bool,
    body: Arc<[u8]>,
}

#[derive(Clone)]
enum Cached {
    Obj(Obj),
    Entry(PackEntry),
}

#[derive(Default)]
pub(crate) struct ObjCache {
    map: HashMap<String, Cached>,
    size: usize,
}

const CACHE_MAX: usize = 64 << 20;

impl ObjCache {
    fn obj(&self, id: &str) -> Option<Obj> {
        match self.map.get(id) {
            Some(Cached::Obj(o)) => Some(o.clone()),
            _ => None,
        }
    }

    fn entry(&self, key: &str) -> Option<PackEntry> {
        match self.map.get(key) {
            Some(Cached::Entry(e)) => Some(e.clone()),
            _ => None,
        }
    }

    fn put(&mut self, key: String, c: Cached) {
        let size = 64 + match &c {
            Cached::Obj(o) => o.data.len() + o.body.as_ref().map_or(0, |b| b.len()),
            Cached::Entry(e) => e.body.len(),
        };
        if size > CACHE_MAX / 8 {
            return; // one huge object should not flush everything else
        }
        if self.size + size > CACHE_MAX {
            self.map.clear(); // shortcut: dropped wholesale when full; an LRU would keep the hot part
            self.size = 0;
        }
        self.size += size;
        self.map.insert(key, c);
    }
}

pub(crate) struct Pack {
    path: PathBuf,
    idx: Vec<u8>,
    v2: bool,
    /// opened on first read; mapped into memory where the platform allows
    file: OnceLock<Result<(fs::File, Option<memmap2::Mmap>), String>>,
}

#[derive(Default)]
pub(crate) struct PackSet {
    packs: Vec<Arc<Pack>>,
    /// packs dir mtime when last loaded
    loaded: Option<SystemTime>,
    once: bool,
}

fn load_pack(name: &str) -> Option<Pack> {
    let idx = fs::read(format!("{name}.idx")).ok()?;
    if idx.len() % IDX_ENTRY != 0 {
        return None;
    }
    let mut magic = [0u8; 8];
    let mut f = fs::File::open(format!("{name}.pack")).ok()?;
    let v2 = f.read_exact(&mut magic).is_ok() && magic == PACK_MAGIC;
    Some(Pack { path: format!("{name}.pack").into(), idx, v2, file: OnceLock::new() })
}

impl Pack {
    fn find(&self, raw: &[u8; 32]) -> Option<u64> {
        let n = self.idx.len() / IDX_ENTRY;
        let key = |i: usize| &self.idx[i * IDX_ENTRY..i * IDX_ENTRY + 32];
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if key(mid) < &raw[..] {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo < n && key(lo) == raw).then(|| {
            let o = lo * IDX_ENTRY + 32;
            u64::from_be_bytes(self.idx[o..o + 8].try_into().unwrap())
        })
    }

    fn ids(&self) -> impl Iterator<Item = (&[u8], u64)> {
        self.idx.chunks(IDX_ENTRY).map(|e| (&e[..32], u64::from_be_bytes(e[32..].try_into().unwrap())))
    }

    /// The open file and its mapping. shortcut: packs a gc replaced stay open until exit.
    fn open(&self) -> Result<&(fs::File, Option<memmap2::Mmap>)> {
        self.file
            .get_or_init(|| {
                let f = fs::File::open(&self.path).map_err(|e| e.to_string())?;
                // SAFETY: packs are never modified in place; gc writes new ones.
                let m = unsafe { memmap2::Mmap::map(&f) }.ok().filter(|m| !m.is_empty());
                Ok((f, m))
            })
            .as_ref()
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, e.clone())))
    }

    /// The bytes at off, at most n of them (the whole mapping when mapped).
    fn bytes_at(&self, off: u64, n: usize) -> Result<std::borrow::Cow<'_, [u8]>> {
        let (f, m) = self.open()?;
        if let Some(m) = m {
            let off = off.min(m.len() as u64) as usize;
            return Ok(std::borrow::Cow::Borrowed(&m[off..(off + n).min(m.len())]));
        }
        let mut buf = vec![0; n];
        let got = read_at(f, &mut buf, off)?;
        buf.truncate(got);
        Ok(std::borrow::Cow::Owned(buf))
    }
}

#[cfg(unix)]
fn read_at(f: &fs::File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(&mut buf[got..], off + got as u64)? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

#[cfg(not(unix))]
fn read_at(f: &fs::File, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
    use std::io::Seek;
    let mut f = f.try_clone()?;
    f.seek(std::io::SeekFrom::Start(off))?;
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..])? {
            0 => break,
            n => got += n,
        }
    }
    Ok(got)
}

thread_local! {
    /// One decompression context per thread: making one per entry costs
    /// more than decoding a small entry.
    static UNZSTD: std::cell::RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { std::cell::RefCell::new(None) };
}

fn unzstd(data: &[u8]) -> std::io::Result<Vec<u8>> {
    UNZSTD.with_borrow_mut(|d| {
        if d.is_none() {
            *d = Some(zstd::bulk::Decompressor::new()?);
        }
        d.as_mut().unwrap().decompress(data, MAX_OBJECT_SIZE * 2)
    })
}

fn corrupt(id: &str, why: impl std::fmt::Display) -> Error {
    err!("object {} corrupt: {why}", short(id))
}

impl Repo {
    pub fn pack_dir(&self) -> PathBuf {
        self.dir.join("packs")
    }

    /// The repo's packs. They are read once; with reload set, the directory
    /// is checked again (another process may have run gc), which callers do
    /// when an object they expect is missing.
    pub(crate) fn pack_list(&self, reload: bool) -> Vec<Arc<Pack>> {
        let mut ps = self.packs.lock().unwrap();
        if ps.once && !reload {
            return ps.packs.clone();
        }
        ps.once = true;
        let Ok(st) = fs::metadata(self.pack_dir()) else { return vec![] };
        let mtime = st.modified().ok();
        if mtime.is_some() && mtime == ps.loaded && !ps.packs.is_empty() {
            return ps.packs.clone();
        }
        let mut names: Vec<String> = fs::read_dir(self.pack_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".idx").map(String::from))
            .collect();
        names.sort();
        ps.packs = names
            .iter()
            .filter_map(|n| load_pack(&self.pack_dir().join(n).to_string_lossy()))
            .map(Arc::new)
            .collect();
        ps.loaded = mtime;
        ps.packs.clone()
    }

    pub fn pack_count(&self) -> usize {
        self.pack_list(true).len()
    }

    pub(crate) fn in_pack(&self, id: &str) -> Option<(Arc<Pack>, u64)> {
        let raw = raw_id(id)?;
        self.pack_list(false).into_iter().find_map(|p| p.find(&raw).map(|off| (p, off)))
    }

    /// in_pack, looking at the packs directory again on a miss.
    pub(crate) fn in_pack_fresh(&self, id: &str) -> Option<(Arc<Pack>, u64)> {
        self.in_pack(id).or_else(|| {
            self.pack_list(true);
            self.in_pack(id)
        })
    }

    /// Packed ids starting with a hex prefix.
    pub(crate) fn packed_prefix(&self, prefix: &str) -> Vec<String> {
        self.pack_list(false)
            .iter()
            .flat_map(|p| p.ids().map(|(raw, _)| hex(raw)).filter(|id| id.starts_with(prefix)).collect::<Vec<_>>())
            .collect()
    }

    /// A loose or packed object; depth counts delta hops.
    pub(crate) fn read_local_at(&self, id: &str, depth: usize) -> Result<Obj> {
        if let Some(o) = self.objects.lock().unwrap().obj(id) {
            return Ok(o);
        }
        let o = if let Some((p, off)) = self.in_pack(id) {
            self.object_at(&p, off, id, depth)? // known packed: skip looking for a loose file
        } else {
            match self.read_loose(id) {
                Ok((typ, data)) => Obj { typ, data: data.into(), body: None },
                Err(e) if e.is_not_found() => self.read_packed(id, depth)?, // maybe a gc just packed it
                Err(e) => return Err(e),
            }
        };
        self.objects.lock().unwrap().put(id.to_string(), Cached::Obj(o.clone()));
        Ok(o)
    }

    fn read_packed(&self, id: &str, depth: usize) -> Result<Obj> {
        let Some((p, off)) = self.in_pack_fresh(id) else {
            return Err(Error::NotFound(format!("object {} not found", short(id))));
        };
        match self.object_at(&p, off, id, depth) {
            Err(e) if e.is_not_found() => {
                // a concurrent gc replaced that pack: look again
                self.pack_list(true);
                match self.in_pack(id) {
                    Some((p, off)) => self.object_at(&p, off, id, depth),
                    None => Err(e),
                }
            }
            r => r,
        }
    }

    /// The object id at off in p, expanded and hash-checked.
    fn object_at(&self, p: &Pack, off: u64, id: &str, depth: usize) -> Result<Obj> {
        let e = self.read_entry(p, off, id, depth)?;
        let data: Arc<[u8]> = if e.ctree { expand_tree(&e.body).map_err(|err| corrupt(id, err))?.into() } else { e.body.clone() };
        if hash_object(e.typ, &data) != id {
            return Err(corrupt(id, "hash mismatch"));
        }
        Ok(Obj { typ: e.typ, data, body: e.ctree.then_some(e.body) })
    }

    /// Reads an entry, caching it by position, so a chain of deltas is
    /// decoded once. id is only for messages.
    fn read_entry(&self, p: &Pack, off: u64, id: &str, depth: usize) -> Result<PackEntry> {
        let key = format!("{}@{off}", p.path.display());
        if let Some(e) = self.objects.lock().unwrap().entry(&key) {
            return Ok(e);
        }
        let e = self.decode_entry(p, off, id, depth)?;
        self.objects.lock().unwrap().put(key, Cached::Entry(e.clone()));
        Ok(e)
    }

    fn decode_entry(&self, p: &Pack, off: u64, id: &str, depth: usize) -> Result<PackEntry> {
        if depth > MAX_DELTA_DEPTH + 1 {
            return Err(corrupt(id, "delta chain too long"));
        }
        // the entry's type, form, payload, and the base form a delta payload applies to
        let (typ, ctree, data, base): (&'static str, bool, Vec<u8>, Option<Arc<[u8]>>) = if p.v2 {
            let head = p.bytes_at(off, 10)?;
            let (n, k) = uvarint(&head).ok_or_else(|| corrupt(id, "bad entry length"))?;
            if n < 1 || n as usize > MAX_OBJECT_SIZE * 2 {
                return Err(corrupt(id, "bad entry length"));
            }
            let entry = p.bytes_at(off + k as u64, n as usize)?;
            if entry.len() != n as usize {
                return Err(corrupt(id, "bad entry length"));
            }
            let body = if entry[0] == 1 {
                unzstd(&entry[1..]).map_err(|e| corrupt(id, e))?
            } else {
                entry[1..].to_vec()
            };
            if body.len() < 3 || body[0] as usize >= PACK_TYPES.len() || body[1] > 1 {
                return Err(corrupt(id, "bad entry header"));
            }
            let (typ, ctree, rest) = (PACK_TYPES[body[0] as usize], body[1] == 1, &body[3..]);
            match body[2] {
                0 => (typ, ctree, rest.to_vec(), None),
                1 => {
                    let (bo, k) = uvarint(rest).ok_or_else(|| corrupt(id, "bad base offset"))?;
                    let b = self.read_entry(p, bo, id, depth + 1)?; // already in the form the delta applies to
                    (typ, ctree, rest[k..].to_vec(), Some(b.body))
                }
                2 => {
                    if rest.len() < 32 {
                        return Err(corrupt(id, "bad base id"));
                    }
                    let b = self.read_local_at(&hex(&rest[..32]), depth + 1)?;
                    let form = match (ctree, b.body) {
                        (false, _) => b.data,
                        (true, Some(c)) => c,
                        (true, None) => {
                            compact_tree(&b.data).ok_or_else(|| corrupt(id, "delta base is not a compactable tree"))?.into()
                        }
                    };
                    (typ, ctree, rest[32..].to_vec(), Some(form))
                }
                _ => return Err(corrupt(id, "bad base kind")),
            }
        } else {
            let (f, m) = p.open()?;
            let mut raw = Vec::new();
            let limit = (MAX_OBJECT_SIZE * 2) as u64;
            match m {
                Some(m) => flate2::read::ZlibDecoder::new(&m[(off as usize).min(m.len())..]).take(limit).read_to_end(&mut raw),
                None => {
                    use std::io::Seek;
                    let mut f = f.try_clone()?;
                    f.seek(std::io::SeekFrom::Start(off))?;
                    flate2::read::ZlibDecoder::new(f).take(limit).read_to_end(&mut raw)
                }
            }
            .map_err(|e| corrupt(id, e))?;
            let i = raw.iter().position(|&c| c == 0).ok_or_else(|| corrupt(id, "bad header"))?;
            let head = String::from_utf8_lossy(&raw[..i]).into_owned();
            let f3: Vec<&str> = head.split_whitespace().collect();
            let typ = match f3.as_slice() {
                [t, _, n] if n.parse::<usize>().ok() == Some(raw.len() - i - 1) => {
                    obj_type(t).ok_or_else(|| corrupt(id, "bad header"))?
                }
                [_, _, _] => return Err(corrupt(id, "bad length")),
                _ => return Err(corrupt(id, "bad header")),
            };
            let base = if f3[1] != "-" { Some(self.read_local_at(f3[1], depth + 1)?.data) } else { None };
            (typ, false, raw.split_off(i + 1), base)
        };
        let body = match base {
            Some(b) => apply_delta(&b, &data).map_err(|e| corrupt(id, e))?,
            None => data,
        };
        Ok(PackEntry { typ, ctree, body: body.into() })
    }

    // -----------------------------------------------------------------------
    // gc

    /// Records, for every file and directory that differs between two trees,
    /// the new version as the delta base of the old one. So the newest
    /// version of everything is stored whole and reads of current files
    /// decode no chain; history further back costs more, as in git.
    fn pair_versions(&self, old: &str, new: &str, prev: &mut HashMap<String, String>) {
        if old == new {
            return;
        }
        prev.entry(old.to_string()).or_insert_with(|| new.to_string());
        let (Ok(ot), Ok(nt)) = (self.get_tree(old), self.get_tree(new)) else { return };
        let was: HashMap<&str, &Entry> = ot.entries.iter().map(|e| (e.name.as_str(), e)).collect();
        for e in &nt.entries {
            match was.get(e.name.as_str()) {
                Some(o) if o.id != e.id => {
                    if e.mode == Mode::Dir && o.mode == Mode::Dir {
                        self.pair_versions(&o.id, &e.id, prev);
                    } else if e.mode != Mode::Dir && o.mode != Mode::Dir {
                        prev.entry(o.id.clone()).or_insert_with(|| e.id.clone());
                    }
                }
                _ => {}
            }
        }
    }

    /// Only one gc runs at a time; a lock older than an hour is taken to be
    /// left over from a crash.
    fn lock_gc(&self) -> Result<GcLock> {
        let p = self.dir.join("gc.lock");
        for _ in 0..2 {
            match fs::OpenOptions::new().write(true).create_new(true).open(&p) {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(GcLock(p));
                }
                Err(_) => {
                    let stale = fs::metadata(&p)
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t.elapsed().is_ok_and(|d| d.as_secs() > 3600));
                    if !stale {
                        bail!("another gc is running (remove .sprig/gc.lock if not)");
                    }
                    let _ = fs::remove_file(&p);
                }
            }
        }
        bail!("cannot take gc.lock")
    }

    fn storage_size(&self) -> u64 {
        let mut n = 0;
        walk_files(&self.dir.join("objects"), &mut |_, m| n += m.len());
        for p in self.pack_list(false) {
            n += fs::metadata(&p.path).map(|m| m.len()).unwrap_or(0) + p.idx.len() as u64;
        }
        n
    }

    fn loose_ids(&self) -> Vec<String> {
        let mut ids = vec![];
        walk_files(&self.dir.join("objects"), &mut |p, _| {
            let id = format!(
                "{}{}",
                p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()).unwrap_or(""),
                p.file_name().and_then(|n| n.to_str()).unwrap_or("")
            );
            if is_id(&id) {
                ids.push(id);
            }
        });
        ids
    }

    /// Packs every loose object, and with repack (or once there are many
    /// packs) everything in the old packs too. Nothing is deleted until the
    /// new pack has been written, synced and read back object by object.
    pub fn gc(&self, repack: bool) -> Result<GcResult> {
        let _lock = self.lock_gc()?;
        let mut res = GcResult { before: self.storage_size(), ..Default::default() };
        let old = self.pack_list(true);
        let repack = repack || old.len() > REPACK_AT;
        let mut ids = self.loose_ids();
        let loose = ids.len();
        if repack {
            let mut seen: std::collections::HashSet<String> = ids.iter().cloned().collect();
            for p in &old {
                for (raw, _) in p.ids() {
                    let id = hex(raw);
                    if seen.insert(id.clone()) {
                        ids.push(id);
                    }
                }
            }
        }
        if ids.is_empty() || (!repack && loose == 0) {
            res.after = res.before;
            return Ok(res);
        }
        ids.sort();

        // Delta bases: each file and directory version's previous version.
        let mut prev = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for tip in self.list_refs().values() {
            let _ = self.walk(tip, |id, c| {
                if seen.insert(id.to_string()) {
                    if let Some(pc) = c.parents.first().and_then(|p| self.get_change(p).ok()) {
                        self.pair_versions(&pc.tree, &c.tree, &mut prev);
                    }
                }
                true
            });
        }
        // Chains are capped, and a revert (A -> B -> A) must not make two
        // versions each other's base, so cycles are cut.
        let mut depth: HashMap<String, usize> = HashMap::new();
        let mut visiting = std::collections::HashSet::new();
        fn depth_of(
            id: &str,
            prev: &mut HashMap<String, String>,
            depth: &mut HashMap<String, usize>,
            visiting: &mut std::collections::HashSet<String>,
        ) -> Option<usize> {
            if let Some(&d) = depth.get(id) {
                return Some(d);
            }
            if !visiting.insert(id.to_string()) {
                return None; // a cycle
            }
            let mut d = 0;
            if let Some(b) = prev.get(id).cloned() {
                match depth_of(&b, prev, depth, visiting) {
                    Some(bd) if bd < MAX_DELTA_DEPTH => d = bd + 1,
                    _ => {
                        prev.remove(id); // store this one whole
                    }
                }
            }
            depth.insert(id.to_string(), d);
            Some(d)
        }
        for id in &ids {
            depth_of(id, &mut prev, &mut depth, &mut visiting);
        }
        // Write bases before the deltas built on them, so a delta can name
        // its base by offset.
        ids.sort_by_key(|id| depth[id]);

        fs::create_dir_all(self.pack_dir())?;
        let tmp = self.pack_dir().join(format!(".tmp-pack-{}", std::process::id()));
        let _cleanup = Cleanup(tmp.clone());
        let mut w = std::io::BufWriter::new(fs::File::create(&tmp)?);
        w.write_all(PACK_MAGIC)?;
        let mut off = PACK_MAGIC.len() as u64;
        let mut ents: Vec<([u8; 32], u64)> = vec![];
        let mut written: HashMap<String, u64> = HashMap::new();
        let mut zstd = zstd::bulk::Compressor::new(19)?;
        for id in &ids {
            let o = self.read_local_at(id, 0)?;
            let (enc, body): (u8, Vec<u8>) = match (o.typ, &o.body) {
                (TREE, Some(b)) => (1, b.to_vec()),
                (TREE, None) => compact_tree(&o.data).map_or((0, o.data.to_vec()), |c| (1, c)),
                _ => (0, o.data.to_vec()),
            };
            let tbyte = PACK_TYPES.iter().position(|&t| t == o.typ).unwrap() as u8;
            let mut head = vec![tbyte, enc, 0];
            let mut payload = &body[..];
            let delta;
            if let (Some(b), true) = (prev.get(id), o.typ != CHANGE && depth[id] > 0) {
                if let Ok(bo) = self.read_local_at(b, 0).filter_typ(o.typ) {
                    let bform = if enc == 1 {
                        bo.body.as_deref().map(<[u8]>::to_vec).or_else(|| compact_tree(&bo.data))
                    } else {
                        Some(bo.data.to_vec())
                    };
                    if let Some(bdata) = bform {
                        let d = best_delta(&bdata, &body);
                        if d.len() < body.len() * 3 / 4 {
                            head.truncate(2);
                            match written.get(b) {
                                Some(&boff) => {
                                    head.push(1);
                                    put_uvarint(&mut head, boff);
                                }
                                None => {
                                    head.push(2);
                                    head.extend_from_slice(&raw_id(b).unwrap());
                                }
                            }
                            delta = d;
                            payload = &delta;
                            res.deltas += 1;
                        }
                    }
                }
            }
            let mut plain = head;
            plain.extend_from_slice(payload);
            let z = zstd.compress(&plain)?;
            let entry: Vec<u8> = if z.len() < plain.len() { [&[1u8][..], &z].concat() } else { [&[0u8][..], &plain].concat() };
            ents.push((raw_id(id).unwrap(), off));
            written.insert(id.clone(), off);
            let mut n = vec![];
            put_uvarint(&mut n, entry.len() as u64);
            w.write_all(&n)?;
            w.write_all(&entry)?;
            off += (n.len() + entry.len()) as u64;
        }
        let f = w.into_inner().map_err(|e| err!("{e}"))?;
        f.sync_all()?;
        drop(f);
        ents.sort();
        let mut idx = Vec::with_capacity(ents.len() * IDX_ENTRY);
        for (raw, o) in &ents {
            idx.extend_from_slice(raw);
            idx.extend_from_slice(&o.to_be_bytes());
        }
        let name = self.pack_dir().join(format!("pack-{}", &hash_object("pack", &idx)[..16]));
        let name = name.to_string_lossy().into_owned();
        fs::rename(&tmp, format!("{name}.pack"))?;
        write_file_atomic(format!("{name}.idx").as_ref(), &idx, true)?; // the .idx makes the pack visible
        // Read every object back from the new pack itself before letting go
        // of any other copy.
        let np = load_pack(&name).ok_or_else(|| err!("new pack unreadable, nothing removed"))?;
        for (raw, o) in &ents {
            self.object_at(&np, *o, &hex(raw), 0)
                .map_err(|e| err!("new pack failed verification, nothing removed: {e}"))?;
        }
        for id in &ids {
            match fs::remove_file(self.obj_path(id)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => res.deleted_loose_err += 1,
                _ => {}
            }
        }
        if repack {
            for p in &old {
                if p.path.to_string_lossy() == format!("{name}.pack") {
                    continue;
                }
                let _ = fs::remove_file(p.path.with_extension("idx")); // idx first: the pack stops being visible
                let _ = fs::remove_file(&p.path);
                res.repacked += 1;
            }
        }
        res.packed = ids.len();
        if let Ok(ents) = fs::read_dir(self.dir.join("objects")) {
            for e in ents.flatten() {
                let _ = fs::remove_dir(e.path()); // only succeeds when empty
            }
        }
        self.pack_list(true);
        res.after = self.storage_size();
        Ok(res)
    }

    /// Estimates loose objects from one fan-out directory (git's trick) and
    /// counts packs.
    pub fn needs_gc(&self) -> bool {
        let n = fs::read_dir(self.dir.join("objects").join("17")).map(|d| d.count()).unwrap_or(0);
        n * 256 >= AUTO_GC_LOOSE || self.pack_list(false).len() > REPACK_AT
    }

    /// Starts a background `sprig gc --auto` when objects have piled up.
    pub fn maybe_auto_gc(&self) {
        if std::env::var_os("SPRIG_NO_AUTOGC").is_none_or(|v| v.is_empty()) && self.needs_gc() {
            let Ok(exe) = std::env::current_exe() else { return };
            let dir = if self.bare { &self.dir } else { &self.root };
            let child = std::process::Command::new(exe)
                .args(["gc", "--auto"])
                .current_dir(dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Ok(mut c) = child {
                std::thread::spawn(move || c.wait()); // reaped if we are still around (servers); a CLI just exits
            }
        }
    }
}

trait FilterTyp {
    fn filter_typ(self, typ: &str) -> Result<Obj>;
}

impl FilterTyp for Result<Obj> {
    fn filter_typ(self, typ: &str) -> Result<Obj> {
        self.and_then(|o| if o.typ == typ { Ok(o) } else { bail!("type differs") })
    }
}

struct GcLock(PathBuf);

impl Drop for GcLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn walk_files(dir: &std::path::Path, f: &mut dyn FnMut(&std::path::Path, &fs::Metadata)) {
    let Ok(ents) = fs::read_dir(dir) else { return };
    for e in ents.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if m.is_dir() {
            walk_files(&e.path(), f);
        } else if !e.file_name().to_string_lossy().starts_with(".tmp-") {
            f(&e.path(), &m);
        }
    }
}

#[derive(Default, Debug)]
pub struct GcResult {
    pub packed: usize,
    pub deltas: usize,
    /// old packs merged into the new one
    pub repacked: usize,
    pub before: u64,
    pub after: u64,
    pub deleted_loose_err: usize,
}

// ---------------------------------------------------------------------------
// Compact trees: count, then per entry a mode byte, the name and the raw id.

/// Encodes a tree, but only if expanding it gives back exactly data;
/// anything unusual is simply stored as it is.
pub fn compact_tree(data: &[u8]) -> Option<Vec<u8>> {
    let t = parse_tree(data).ok()?;
    let mut out = vec![];
    put_uvarint(&mut out, t.entries.len() as u64);
    for e in &t.entries {
        out.push(TREE_MODES.iter().position(|&m| m == e.mode)? as u8);
        put_uvarint(&mut out, e.name.len() as u64);
        out.extend_from_slice(e.name.as_bytes());
        out.extend_from_slice(&raw_id(&e.id)?);
    }
    (expand_tree(&out).ok()? == data).then_some(out)
}

pub fn decode_compact(mut b: &[u8]) -> Result<Tree> {
    let (n, k) = uvarint(b).filter(|&(n, _)| n <= b.len() as u64).ok_or_else(|| err!("bad compact tree"))?;
    b = &b[k..];
    let mut t = Tree { entries: Vec::with_capacity(n as usize) };
    for _ in 0..n {
        let mode = *b.first().and_then(|&m| TREE_MODES.get(m as usize)).ok_or_else(|| err!("bad compact tree entry"))?;
        let (l, k) = uvarint(&b[1..]).ok_or_else(|| err!("bad compact tree entry"))?;
        let (l, start) = (l as usize, 1 + k);
        if b.len() < start + l + 32 {
            bail!("bad compact tree entry");
        }
        let name = String::from_utf8(b[start..start + l].to_vec()).map_err(|_| err!("bad compact tree entry"))?;
        t.entries.push(Entry { name, mode, id: hex(&b[start + l..start + l + 32]) });
        b = &b[start + l + 32..];
    }
    if !b.is_empty() {
        bail!("trailing bytes in compact tree");
    }
    Ok(t)
}

pub fn expand_tree(b: &[u8]) -> Result<Vec<u8>> {
    Ok(crate::tree::tree_json(&decode_compact(b)?)) // as put_tree does: entries already in stored order
}

// ---------------------------------------------------------------------------
// Delta format: 'c' offset length (copy from base) or 'i' length bytes
// (insert), numbers as uvarints.

fn append_copy(out: &mut Vec<u8>, off: usize, n: usize) {
    out.push(b'c');
    put_uvarint(out, off as u64);
    put_uvarint(out, n as u64);
}

fn append_insert(out: &mut Vec<u8>, data: &[u8]) {
    if !data.is_empty() {
        out.push(b'i');
        put_uvarint(out, data.len() as u64);
        out.extend_from_slice(data);
    }
}

/// Tries both encoders and keeps the smaller result.
pub fn best_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
    let (a, b) = (make_delta(base, target), block_delta(base, target));
    if b.len() < a.len() { b } else { a }
}

/// Whole tokens: lines, also cut after '}'.
fn split_tokens(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    b.split_inclusive(|&c| c == b'\n' || c == b'}')
}

/// Matches whole tokens. Good for source.
pub fn make_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
    let mut at: HashMap<&[u8], usize> = HashMap::new();
    let mut off = 0;
    for t in split_tokens(base) {
        at.entry(t).or_insert(off);
        off += t.len();
    }
    let (mut out, mut ins) = (vec![], vec![]);
    let toks: Vec<&[u8]> = split_tokens(target).collect();
    let mut i = 0;
    while i < toks.len() {
        let Some(&off) = at.get(toks[i]).filter(|_| toks[i].len() >= 2) else {
            ins.extend_from_slice(toks[i]); // copying a lone newline costs more than inserting it
            i += 1;
            continue;
        };
        let mut n = 0;
        while i < toks.len() && base[off + n..].starts_with(toks[i]) {
            n += toks[i].len();
            i += 1;
        }
        append_insert(&mut out, &ins);
        ins.clear();
        append_copy(&mut out, off, n);
    }
    append_insert(&mut out, &ins);
    out
}

const DELTA_BLOCK: usize = 16;

/// Finds matches at any byte position: the base is indexed by 16-byte
/// blocks and the target scanned with a rolling hash, each hit then extended
/// both ways. Good for binary data and compact trees.
pub fn block_delta(base: &[u8], target: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    if base.len() < DELTA_BLOCK || target.len() < DELTA_BLOCK {
        append_insert(&mut out, target);
        return out;
    }
    const PRIME: u32 = 16777619;
    let pow = (0..DELTA_BLOCK - 1).fold(1u32, |p, _| p.wrapping_mul(PRIME));
    let hash = |b: &[u8]| b[..DELTA_BLOCK].iter().fold(0u32, |h, &c| h.wrapping_mul(PRIME).wrapping_add(c as u32));
    let mut at: HashMap<u32, usize> = HashMap::new();
    for i in (0..=base.len() - DELTA_BLOCK).step_by(DELTA_BLOCK) {
        at.entry(hash(&base[i..])).or_insert(i);
    }
    let (mut pending, mut i) = (0, 0);
    let mut h = hash(target);
    while i + DELTA_BLOCK <= target.len() {
        if let Some(&j0) = at.get(&h) {
            if base[j0..j0 + DELTA_BLOCK] == target[i..i + DELTA_BLOCK] {
                let mut j = j0;
                while j > 0 && i > pending && base[j - 1] == target[i - 1] {
                    i -= 1;
                    j -= 1;
                }
                let mut n = DELTA_BLOCK;
                while j + n < base.len() && i + n < target.len() && base[j + n] == target[i + n] {
                    n += 1;
                }
                append_insert(&mut out, &target[pending..i]);
                append_copy(&mut out, j, n);
                i += n;
                pending = i;
                if i + DELTA_BLOCK <= target.len() {
                    h = hash(&target[i..]);
                }
                continue;
            }
        }
        if i + DELTA_BLOCK < target.len() {
            h = h.wrapping_sub((target[i] as u32).wrapping_mul(pow)).wrapping_mul(PRIME).wrapping_add(target[i + DELTA_BLOCK] as u32);
        }
        i += 1;
    }
    append_insert(&mut out, &target[pending..]);
    out
}

pub fn apply_delta(base: &[u8], mut delta: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![];
    while let Some((&op, rest)) = delta.split_first() {
        let (a, k) = uvarint(rest).ok_or_else(|| err!("bad delta"))?;
        delta = &rest[k..];
        match op {
            b'c' => {
                let (n, k) = uvarint(delta).ok_or_else(|| err!("bad delta copy"))?;
                if a.checked_add(n).is_none_or(|e| e > base.len() as u64) {
                    bail!("bad delta copy");
                }
                delta = &delta[k..];
                out.extend_from_slice(&base[a as usize..(a + n) as usize]);
            }
            b'i' => {
                if a > delta.len() as u64 {
                    bail!("bad delta insert");
                }
                out.extend_from_slice(&delta[..a as usize]);
                delta = &delta[a as usize..];
            }
            _ => bail!("bad delta op"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_round_trip() {
        let d = make_delta(b"a\nbb\ncc\n", b"a\nbb\nXX\ncc\n");
        assert_eq!(apply_delta(b"a\nbb\ncc\n", &d).unwrap(), b"a\nbb\nXX\ncc\n");
        let base = "0123456789abcdef".repeat(40).into_bytes();
        let target = [&base[..300], b"INSERTED", &base[300..]].concat();
        for d in [make_delta(&base, &target), block_delta(&base, &target), best_delta(&base, &target)] {
            assert_eq!(apply_delta(&base, &d).unwrap(), target);
        }
        assert!(block_delta(&base, &target).len() <= 40);
    }

    #[test]
    fn compact_trees_expand_exactly() {
        let t = Tree {
            entries: vec![
                Entry { name: "a<b>.go".into(), mode: Mode::File, id: hash_object("blob", b"") },
                Entry { name: "d".into(), mode: Mode::Dir, id: hash_object("tree", b"") },
            ],
        };
        let tr = crate::util::to_json(&t);
        let c = compact_tree(&tr).unwrap();
        assert!(c.len() < tr.len() / 2);
        assert_eq!(expand_tree(&c).unwrap(), tr);
    }

    #[test]
    fn tree_json_matches_serde() {
        let names = ["plain.txt", "a<b>&c", "quote\"back\\slash", "tab\tnl\n", "ünï", "line\u{2028}sep", "\u{7f}del", "\u{1}ctl"];
        let t = Tree {
            entries: names.iter().map(|n| Entry { name: n.to_string(), mode: Mode::Exec, id: hash_object("blob", n.as_bytes()) }).collect(),
        };
        assert_eq!(crate::tree::tree_json(&t), crate::util::to_json(&t));
    }
}
