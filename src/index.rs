//! The index: what is in the working tree, in time proportional to what
//! changed.
//!
//! For every path in HEAD's tree or the working tree it records HEAD's
//! entry and, once seen, the working file's mode, content id, size and
//! mtime. A working file whose size, mtime and mode still match is known to
//! have that content without reading it, so status only compares the two
//! halves of each record; nothing is flattened and nothing is hashed. The
//! file is written whole (records merged in one sorted pass) and read with
//! mmap: fixed-width records sorted by path, then the paths, searched by
//! binary search with nothing decoded up front.
//!
//! The HEAD halves are a cache of one tree (`tree`); when HEAD moved without
//! updating the index they are rebuilt from that tree, keeping the working
//! halves, which do not depend on HEAD. Entries written less than a second
//! before the scan that saved the index are not trusted ("racily clean", as
//! git calls it), unless a `sprig watch` vouches for that time.

use crate::store::{hash_object, Config, Repo, BLOB};
use crate::tree::{ignored, link_bytes, Delta, FileEnt, Mode};
use crate::util::{hex, now_ns, raw_id, Result};
use std::collections::BTreeMap;
use std::fs;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex};

const MAGIC: &[u8] = b"SPRIGSC2";
const HEADER: usize = 8 + 8 + 1 + 32 + 4;
const REC: usize = 4 + 4 + 1 + 1 + 2 + 8 + 8 + 32 + 32;
const RACY_NS: i64 = 1_000_000_000;

/// A working file as last seen.
#[derive(Clone, Debug, PartialEq)]
pub struct Work {
    pub mode: Mode,
    pub id: String,
    pub size: u64,
    pub mtime: i64,
}

impl Work {
    fn ent(&self) -> FileEnt {
        FileEnt::new(self.mode, self.id.clone())
    }
}

struct Rec {
    path: String,
    head: Option<FileEnt>,
    work: Option<Work>,
}

enum Buf {
    Map(memmap2::Mmap),
    Owned(Vec<u8>),
}

impl std::ops::Deref for Buf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Buf::Map(m) => m,
            Buf::Owned(v) => v,
        }
    }
}

pub struct Index {
    buf: Buf,
    n: usize,
    /// when the scan that saved it started (unix ns)
    pub written: i64,
    /// the tree the HEAD halves mirror: None = unknown, Some("") = no HEAD yet
    pub tree: Option<String>,
}

fn mode_code(m: Option<Mode>) -> u8 {
    match m {
        None => 0,
        Some(Mode::File) => 1,
        Some(Mode::Exec) => 2,
        Some(Mode::Link) => 3,
        Some(Mode::Dir) => 4, // never stored
    }
}

fn code_mode(c: u8) -> Option<Mode> {
    [None, Some(Mode::File), Some(Mode::Exec), Some(Mode::Link)].get(c as usize).copied().flatten()
}

impl Index {
    fn empty() -> Index {
        Index { buf: Buf::Owned(vec![]), n: 0, written: 0, tree: None }
    }

    fn parse(buf: Buf) -> Index {
        let b = &buf[..];
        if b.len() < HEADER || &b[..8] != MAGIC {
            return Index::empty(); // missing, the old format, or torn: every file is read once
        }
        let written = i64::from_le_bytes(b[8..16].try_into().unwrap());
        let tree = match b[16] {
            1 => Some(hex(&b[17..49])),
            2 => Some(String::new()),
            _ => None,
        };
        let n = u32::from_le_bytes(b[49..53].try_into().unwrap()) as usize;
        let table = HEADER + n * REC;
        if b.len() < table {
            return Index::empty();
        }
        let tlen = (b.len() - table) as u64;
        for i in 0..n {
            let r = &b[HEADER + i * REC..];
            let (off, len) = (u32::from_le_bytes(r[..4].try_into().unwrap()), u32::from_le_bytes(r[4..8].try_into().unwrap()));
            if off as u64 + len as u64 > tlen || r[8] > 3 || r[9] > 3 {
                return Index::empty();
            }
        }
        Index { buf, n, written, tree }
    }

    fn load(repo: &Repo) -> Index {
        let Ok(f) = fs::File::open(index_path(repo)) else { return Index::empty() };
        // SAFETY: the index is only ever replaced by rename, never written in place.
        match unsafe { memmap2::Mmap::map(&f) } {
            Ok(m) => Index::parse(Buf::Map(m)),
            Err(_) => Index::empty(),
        }
    }

    fn rec(&self, i: usize) -> &[u8] {
        &self.buf[HEADER + i * REC..HEADER + (i + 1) * REC]
    }

    fn path(&self, i: usize) -> &[u8] {
        let r = self.rec(i);
        let off = u32::from_le_bytes(r[..4].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(r[4..8].try_into().unwrap()) as usize;
        let t = HEADER + self.n * REC + off;
        &self.buf[t..t + len]
    }

    fn path_str(&self, i: usize) -> String {
        String::from_utf8_lossy(self.path(i)).into_owned()
    }

    fn head_raw(&self, i: usize) -> Option<(Mode, &[u8])> {
        let r = self.rec(i);
        code_mode(r[8]).map(|m| (m, &r[28..60]))
    }

    fn work_raw(&self, i: usize) -> Option<(Mode, &[u8])> {
        let r = self.rec(i);
        code_mode(r[9]).map(|m| (m, &r[60..92]))
    }

    pub fn head(&self, i: usize) -> Option<FileEnt> {
        self.head_raw(i).map(|(m, id)| FileEnt::new(m, hex(id)))
    }

    fn work(&self, i: usize) -> Option<Work> {
        let r = self.rec(i);
        let (mode, id) = self.work_raw(i)?;
        Some(Work {
            mode,
            id: hex(id),
            size: u64::from_le_bytes(r[12..20].try_into().unwrap()),
            mtime: i64::from_le_bytes(r[20..28].try_into().unwrap()),
        })
    }

    fn rec_owned(&self, i: usize) -> Rec {
        Rec { path: self.path_str(i), head: self.head(i), work: self.work(i) }
    }

    /// The first record whose path is >= p.
    fn lower_bound(&self, p: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.path(mid) < p {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn find(&self, p: &[u8]) -> Option<usize> {
        let i = self.lower_bound(p);
        (i < self.n && self.path(i) == p).then_some(i)
    }

    /// The records at p and below it ("" covers everything).
    fn covered(&self, p: &str) -> (Option<usize>, Range<usize>) {
        if p.is_empty() {
            return (None, 0..self.n);
        }
        let lo = self.lower_bound(format!("{p}/").as_bytes());
        let hi = self.lower_bound(format!("{p}0").as_bytes()); // '0' follows '/'
        (self.find(p.as_bytes()), lo..hi)
    }

    /// Writes recs (sorted by path) as a new index.
    fn encode(recs: impl Iterator<Item = Rec>, tree: Option<&str>, written: i64) -> Vec<u8> {
        let (mut body, mut table) = (vec![], vec![]);
        let mut n = 0u32;
        for r in recs {
            if r.head.is_none() && r.work.is_none() {
                continue;
            }
            n += 1;
            body.extend_from_slice(&(table.len() as u32).to_le_bytes());
            body.extend_from_slice(&(r.path.len() as u32).to_le_bytes());
            table.extend_from_slice(r.path.as_bytes());
            body.push(mode_code(r.head.as_ref().map(|h| h.mode)));
            body.push(mode_code(r.work.as_ref().map(|w| w.mode)));
            body.extend_from_slice(&[0, 0]);
            let w = r.work.as_ref();
            body.extend_from_slice(&w.map_or(0, |w| w.size).to_le_bytes());
            body.extend_from_slice(&w.map_or(0, |w| w.mtime).to_le_bytes());
            for id in [r.head.as_ref().map(|h| h.id.as_str()), w.map(|w| w.id.as_str())] {
                body.extend_from_slice(&id.and_then(raw_id).unwrap_or([0; 32]));
            }
        }
        let mut out = Vec::with_capacity(HEADER + body.len() + table.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&written.to_le_bytes());
        match tree.map(|t| (t, raw_id(t))) {
            Some((_, Some(raw))) => {
                out.push(1);
                out.extend_from_slice(&raw);
            }
            Some(("", _)) => {
                out.push(2);
                out.extend_from_slice(&[0; 32]);
            }
            _ => {
                out.push(0);
                out.extend_from_slice(&[0; 32]);
            }
        }
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&body);
        out.extend_from_slice(&table);
        out
    }

    /// This index's records with the given halves replaced, merged in one
    /// sorted pass.
    fn patched<'a>(
        &'a self,
        head: &'a BTreeMap<String, Option<FileEnt>>,
        work: &'a BTreeMap<String, Option<Work>>,
    ) -> impl Iterator<Item = Rec> + 'a {
        let mut keys: Vec<&String> = head.keys().chain(work.keys()).collect();
        keys.sort();
        keys.dedup();
        let (mut i, mut k) = (0, 0);
        std::iter::from_fn(move || {
            let take_rec = i < self.n && (k == keys.len() || self.path(i) <= keys[k].as_bytes());
            let mut r = if take_rec {
                let same = k < keys.len() && self.path(i) == keys[k].as_bytes();
                i += 1;
                k += same as usize;
                self.rec_owned(i - 1)
            } else if k < keys.len() {
                k += 1;
                Rec { path: keys[k - 1].clone(), head: None, work: None }
            } else {
                return None;
            };
            if let Some(h) = head.get(&r.path) {
                r.head = h.clone();
            }
            if let Some(w) = work.get(&r.path) {
                r.work = w.clone();
            }
            Some(r)
        })
    }
}

fn index_path(repo: &Repo) -> PathBuf {
    repo.dir.join("index")
}

/// The working tree against HEAD, and what was learned looking at it.
pub struct WorkStatus {
    /// HEAD's tree ("" for none)
    pub tree: String,
    /// working tree vs HEAD, sorted by path; new sides not yet in the store
    /// unless the scan stored them
    pub deltas: Vec<Delta>,
    idx: Index,
    written: i64,
    work: BTreeMap<String, Option<Work>>,
    /// the index on disk is out of date
    dirty: bool,
}

impl WorkStatus {
    /// Saves what the scan learned, if anything.
    pub fn save(&self, repo: &Repo) {
        if self.dirty {
            self.save_with(repo, &self.tree, &BTreeMap::new(), &BTreeMap::new());
        }
    }

    /// Saves the index for HEAD at tree, with these HEAD halves and working
    /// halves changed on top of what the scan learned. Best effort: losing
    /// the index only costs reading files again.
    pub fn save_with(
        &self,
        repo: &Repo,
        tree: &str,
        head: &BTreeMap<String, Option<FileEnt>>,
        work: &BTreeMap<String, Option<Work>>,
    ) {
        let mut all = self.work.clone();
        all.extend(work.iter().map(|(k, v)| (k.clone(), v.clone())));
        let b = Index::encode(self.idx.patched(head, &all), Some(tree), self.written);
        let _ = crate::store::write_file_atomic(&index_path(repo), &b, false);
    }
}

/// Stat data of a working file, from lstat.
pub fn work_of(md: &fs::Metadata, id: String) -> Option<Work> {
    let mode = if md.file_type().is_symlink() {
        Mode::Link
    } else if !md.is_file() {
        return None; // sockets, devices, ...
    } else if is_exec(md) {
        Mode::Exec
    } else {
        Mode::File
    };
    Some(Work { mode, id, size: md.len(), mtime: mtime_ns(md) })
}

#[cfg(unix)]
fn is_exec(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn mtime_ns(md: &fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    md.mtime() * 1_000_000_000 + md.mtime_nsec()
}

#[cfg(not(unix))]
fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

/// What a walk saw: records confirmed by stat, and files read afresh.
#[derive(Default)]
struct Seen {
    matched: Vec<u32>,
    fresh: Vec<(Option<u32>, String, Work)>,
    err: Option<crate::util::Error>,
}

struct Walk<'a> {
    repo: &'a Repo,
    idx: &'a Index,
    pats: Vec<String>,
    cfg: &'a Config,
    store: bool,
    /// entries with an mtime from here on are not trusted (unless a watcher vouches)
    racy: i64,
    queue: Mutex<(Vec<(PathBuf, String)>, usize)>,
    cv: Condvar,
}

impl Walk<'_> {
    fn skip_dir(&self, name: &str, rel: &str) -> bool {
        name == ".sprig" || name == ".git" || ignored(&self.pats, rel) || !self.cfg.dir_in_view(rel)
    }

    fn skip_file(&self, rel: &str) -> bool {
        ignored(&self.pats, rel) || !self.cfg.in_view(rel)
    }

    /// Records one working file, reusing the index's id when its size,
    /// mtime and mode are unchanged.
    fn file(&self, abs: &std::path::Path, rel: String, md: &fs::Metadata, seen: &mut Seen) -> Result<()> {
        let Some(st) = work_of(md, String::new()) else { return Ok(()) };
        let i = self.idx.find(rel.as_bytes());
        if let Some(i) = i {
            if let Some((mode, id)) = self.idx.work_raw(i) {
                let r = self.idx.rec(i);
                let (size, mtime) =
                    (u64::from_le_bytes(r[12..20].try_into().unwrap()), i64::from_le_bytes(r[20..28].try_into().unwrap()));
                if mode == st.mode && size == st.size && mtime == st.mtime && mtime < self.racy {
                    // committing needs the content stored; HEAD's already is
                    let stored = !self.store
                        || self.idx.head_raw(i).is_some_and(|(_, h)| h == id)
                        || self.repo.has_object(&hex(id));
                    if stored {
                        seen.matched.push(i as u32);
                        return Ok(());
                    }
                }
            }
        }
        let data = if st.mode == Mode::Link {
            link_bytes(fs::read_link(abs)?)
        } else {
            match fs::read(abs) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // deleted while we looked
                r => r.map_err(|e| crate::err!("read {}: {e}", abs.display()))?,
            }
        };
        let id = if self.store { self.repo.put_blob(&data)? } else { hash_object(BLOB, &data) };
        seen.fresh.push((i.map(|i| i as u32), rel, Work { id, ..st }));
        Ok(())
    }

    fn dir(&self, abs: PathBuf, rel: String, seen: &mut Seen) -> Result<()> {
        let ents = match fs::read_dir(&abs) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            r => r.map_err(|e| crate::err!("open {}: {e}", abs.display()))?,
        };
        for e in ents {
            let e = e?;
            let name = e.file_name();
            let name = name.to_string_lossy();
            let crel = if rel.is_empty() { name.to_string() } else { format!("{rel}/{name}") };
            let ft = e.file_type()?;
            if ft.is_dir() {
                if !self.skip_dir(&name, &crel) {
                    self.push(e.path(), crel);
                }
                continue;
            }
            if self.skip_file(&crel) {
                continue;
            }
            let md = match e.metadata() {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue, // deleted while we looked
                r => r?,
            };
            if md.is_dir() {
                self.push(e.path(), crel);
                continue;
            }
            self.file(&e.path(), crel, &md, seen)?;
        }
        Ok(())
    }

    fn push(&self, abs: PathBuf, rel: String) {
        self.queue.lock().unwrap().0.push((abs, rel));
        self.cv.notify_one();
    }

    /// Works through the queue until it is empty and nobody can add to it.
    fn work(&self) -> Seen {
        let mut seen = Seen::default();
        loop {
            let job = {
                let mut q = self.queue.lock().unwrap();
                loop {
                    if let Some(j) = q.0.pop() {
                        q.1 += 1;
                        break Some(j);
                    }
                    if q.1 == 0 {
                        break None;
                    }
                    q = self.cv.wait(q).unwrap();
                }
            };
            let Some((abs, rel)) = job else {
                self.cv.notify_all();
                return seen;
            };
            if let Err(e) = self.dir(abs, rel, &mut seen) {
                seen.err.get_or_insert(e);
            }
            let mut q = self.queue.lock().unwrap();
            q.1 -= 1;
            if q.1 == 0 && q.0.is_empty() {
                self.cv.notify_all();
            }
        }
    }
}

/// The paths not below another one in the list ("" is the root).
fn outermost(mut paths: Vec<String>) -> Vec<String> {
    if paths.iter().any(String::is_empty) {
        return vec![String::new()];
    }
    paths.sort();
    paths.dedup();
    let set: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
    let covered = |p: &str| p.match_indices('/').any(|(i, _)| set.contains(&p[..i]));
    paths.iter().filter(|p| !covered(p)).cloned().collect()
}

/// Threads for walking: more than 4 made it slower on a 12-core machine.
fn walkers() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 4)
}

impl Repo {
    /// The index for HEAD at tree, with its HEAD halves rebuilt if they
    /// mirror another tree. The second value says it needs saving.
    fn index_for(&self, tree: &str) -> Result<(Index, bool)> {
        let idx = Index::load(self);
        if idx.tree.as_deref() == Some(tree) {
            return Ok((idx, false));
        }
        let flat = self.flatten(tree)?;
        let mut head: BTreeMap<String, Option<FileEnt>> = flat.into_iter().map(|(p, fe)| (p, Some(fe))).collect();
        for i in 0..idx.n {
            head.entry(idx.path_str(i)).or_insert(None); // in no tree now
        }
        let b = Index::encode(idx.patched(&head, &BTreeMap::new()), Some(tree), idx.written);
        Ok((Index::parse(Buf::Owned(b)), true))
    }

    /// The working tree against HEAD. With store, the content of every
    /// changed file is put in the object store.
    pub fn work_status(&self, store: bool) -> Result<WorkStatus> {
        if self.bare {
            crate::bail!("bare repository has no working tree");
        }
        let start = now_ns();
        let (_, head) = self.head();
        let tree = self.tree_of(&head)?;
        let (idx, mut dirty) = self.index_for(&tree)?;
        let cfg = self.load_config();
        let (dirty_paths, watched, running) = if idx.n > 0 {
            crate::watch::ask_watcher_since(self, idx.written)
        } else {
            (vec![], false, false)
        };
        let roots = if watched { outermost(dirty_paths) } else { vec![String::new()] };

        let walk = Walk {
            repo: self,
            idx: &idx,
            pats: self.load_ignore(),
            cfg: &cfg,
            store,
            racy: if watched { i64::MAX } else { idx.written - RACY_NS },
            queue: Mutex::new((vec![], 0)),
            cv: Condvar::new(),
        };
        let mut seen = Seen::default();
        for r in &roots {
            if r.is_empty() {
                walk.push(self.root.clone(), String::new());
                continue;
            }
            let abs = self.abs(r);
            match fs::symlink_metadata(&abs) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
                Ok(md) if md.is_dir() => {
                    if !walk.skip_dir(crate::util::base_name(r), r) {
                        walk.push(abs, r.clone());
                    }
                }
                Ok(md) => {
                    if !walk.skip_file(r) {
                        walk.file(&abs, r.clone(), &md, &mut seen)?;
                    }
                }
            }
        }
        let results: Vec<Seen> = if walk.queue.lock().unwrap().0.is_empty() {
            vec![]
        } else {
            std::thread::scope(|s| {
                let hs: Vec<_> = (0..walkers()).map(|_| s.spawn(|| walk.work())).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            })
        };
        for r in results {
            if let Some(e) = r.err {
                return Err(e);
            }
            seen.matched.extend(r.matched);
            seen.fresh.extend(r.fresh);
        }

        // Every record under a root was either seen again or is gone. A
        // record's state is KEEP, CONFIRMED, GONE or its index in fresh.
        const KEEP: u32 = u32::MAX;
        const CONFIRMED: u32 = u32::MAX - 1;
        const GONE: u32 = u32::MAX - 2;
        let mut state = vec![KEEP; idx.n];
        for r in &roots {
            let (at, range) = idx.covered(r);
            for i in at.into_iter().chain(range) {
                state[i] = GONE;
            }
        }
        for &i in &seen.matched {
            state[i as usize] = CONFIRMED;
        }
        let mut work = BTreeMap::new();
        let mut deltas = vec![];
        for (k, (i, path, w)) in seen.fresh.iter().enumerate() {
            match i {
                Some(i) => {
                    state[*i as usize] = k as u32;
                    if idx.work(*i as usize).as_ref() != Some(w) {
                        work.insert(path.clone(), Some(w.clone()));
                    }
                }
                None => {
                    work.insert(path.clone(), Some(w.clone()));
                    deltas.push(Delta::of(path, None, Some(w.ent())));
                }
            }
        }
        for (i, &s) in state.iter().enumerate() {
            let head = idx.head_raw(i);
            let w = match s {
                KEEP | CONFIRMED => idx.work_raw(i).map(|(m, id)| (m, std::borrow::Cow::Borrowed(id))),
                GONE => {
                    if idx.work_raw(i).is_some() {
                        work.insert(idx.path_str(i), None);
                    }
                    None
                }
                k => {
                    let w = &seen.fresh[k as usize].2;
                    Some((w.mode, std::borrow::Cow::Owned(raw_id(&w.id).unwrap().to_vec())))
                }
            };
            let same = match (&head, &w) {
                (Some((hm, hid)), Some((wm, wid))) => hm == wm && hid == &&wid[..],
                (None, None) => true,
                _ => false,
            };
            if same {
                continue;
            }
            let path = idx.path_str(i);
            if !cfg.in_view(&path) {
                continue; // outside a sparse view files are whatever HEAD says
            }
            let new = w.map(|(m, id)| FileEnt::new(m, hex(&id)));
            deltas.push(Delta::of(&path, idx.head(i), new));
        }
        deltas.sort_by(|a, b| a.path.cmp(&b.path));
        // A running watcher that could not vouch this time can next time,
        // from a fresh save.
        dirty |= !work.is_empty() || (running && !watched);
        Ok(WorkStatus { tree, deltas, idx, written: start, work, dirty })
    }

    /// The working tree against HEAD, saving what was learned.
    pub fn working_deltas(&self) -> Result<Vec<Delta>> {
        let st = self.work_status(false)?;
        st.save(self);
        Ok(st.deltas)
    }
}
