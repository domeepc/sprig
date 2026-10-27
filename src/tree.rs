//! Trees, their flat (path -> file) form, tree diffs and rename detection,
//! and writing files into the working tree.

use crate::store::{valid_name, valid_path, Repo, TREE};
use crate::util::{self, Result};
use crate::{bail, err};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    File,
    Exec,
    Link,
    Dir,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::File => "file",
            Mode::Exec => "exec",
            Mode::Link => "link",
            Mode::Dir => "dir",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        [Mode::File, Mode::Exec, Mode::Link, Mode::Dir].into_iter().find(|m| m.as_str() == s)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub mode: Mode,
    pub id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Tree {
    #[serde(default, deserialize_with = "util::nullable")]
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileEnt {
    pub mode: Mode,
    pub id: String,
}

impl FileEnt {
    pub fn new(mode: Mode, id: impl Into<String>) -> FileEnt {
        FileEnt { mode, id: id.into() }
    }
}

/// A whole tree as path -> file entry. Most operations (diff, merge,
/// checkout) are simplest over the flat form.
pub type Flat = BTreeMap<String, FileEnt>;

/// A tree's JSON, byte for byte what util::to_json makes of it, written
/// directly: trees are hashed on every read from a pack.
pub fn tree_json(t: &Tree) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + t.entries.len() * 112);
    out.extend_from_slice(b"{\"entries\":[");
    for (i, e) in t.entries.iter().enumerate() {
        out.extend_from_slice(if i == 0 { b"{\"name\":" } else { b",{\"name\":" });
        if e.name.bytes().all(|c| (0x20..0x7f).contains(&c) && !b"\"\\<>&".contains(&c)) {
            out.push(b'"');
            out.extend_from_slice(e.name.as_bytes());
            out.push(b'"');
        } else {
            util::go_json_str(&mut out, e.name.as_bytes());
        }
        out.extend_from_slice(b",\"mode\":\"");
        out.extend_from_slice(e.mode.as_str().as_bytes());
        out.extend_from_slice(b"\",\"id\":\"");
        out.extend_from_slice(e.id.as_bytes());
        out.extend_from_slice(b"\"}");
    }
    out.extend_from_slice(b"]}");
    out
}

pub fn parse_tree(data: &[u8]) -> Result<Tree> {
    let t: Tree = serde_json::from_slice(data).map_err(|e| err!("bad tree: {e}"))?;
    let mut seen = HashSet::new();
    for e in &t.entries {
        if !valid_name(&e.name) || !crate::store::is_id(&e.id) {
            bail!("bad tree entry {e:?}");
        }
        if !seen.insert(e.name.as_str()) {
            bail!("duplicate tree entry {:?}", e.name);
        }
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// Delta between two trees

#[derive(Clone, Debug, PartialEq)]
pub struct Delta {
    pub path: String,
    /// A, M, D, or R (renamed, for display only)
    pub kind: char,
    pub old: Option<FileEnt>,
    pub new: Option<FileEnt>,
    /// old path of a rename
    pub from: String,
}

impl Delta {
    pub fn of(path: &str, old: Option<FileEnt>, new: Option<FileEnt>) -> Delta {
        let kind = match (&old, &new) {
            (None, _) => 'A',
            (_, None) => 'D',
            _ => 'M',
        };
        Delta { path: path.to_string(), kind, old, new, from: String::new() }
    }
}

pub fn diff_flat(a: &Flat, b: &Flat) -> Vec<Delta> {
    let mut out = vec![];
    for (p, ae) in a {
        match b.get(p) {
            None => out.push(Delta::of(p, Some(ae.clone()), None)),
            Some(be) if be != ae => out.push(Delta::of(p, Some(ae.clone()), Some(be.clone()))),
            _ => {}
        }
    }
    for (p, be) in b {
        if !a.contains_key(p) {
            out.push(Delta::of(p, None, Some(be.clone())));
        }
    }
    out.sort_by(|x, y| x.path.cmp(&y.path));
    out
}

/// Applies deltas (as from diff_flat(f, g)) to f, giving g.
pub fn apply_deltas(mut f: Flat, ds: &[Delta]) -> Flat {
    for d in ds {
        match &d.new {
            Some(fe) => f.insert(d.path.clone(), fe.clone()),
            None => f.remove(&d.path),
        };
    }
    f
}

/// The default similarity (percent) for a rename + edit.
pub const RENAME_THRESHOLD: usize = 50;

impl Repo {
    pub fn put_tree(&self, t: &mut Tree) -> Result<String> {
        t.entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.put_object(TREE, &tree_json(t))
    }

    /// A parsed tree. Trees read from a pack are decoded straight from their
    /// compact form, without parsing JSON.
    pub fn get_tree(&self, id: &str) -> Result<Arc<Tree>> {
        if let Some(t) = self.trees.lock().unwrap().get(id) {
            return Ok(t.clone());
        }
        let t = match self.read_local_at(id, 0) {
            Ok(o) if o.typ == TREE => match &o.body {
                Some(c) => crate::pack::decode_compact(c)?,
                None => parse_tree(&o.data)?,
            },
            Ok(o) => bail!("object {} is a {}, not a tree", crate::store::short(id), o.typ),
            Err(e) if e.is_not_found() => {
                let (typ, data) = self.get_object(id)?; // lazy clones and caches fetch it
                if typ != TREE {
                    bail!("object {} is a {typ}, not a tree", crate::store::short(id));
                }
                parse_tree(&data)?
            }
            Err(e) => return Err(e),
        };
        let t = Arc::new(t);
        let mut cache = self.trees.lock().unwrap();
        if cache.len() >= 50000 {
            cache.clear(); // shortcut: dropped wholesale when full; an LRU would keep the hot part
        }
        cache.insert(id.to_string(), t.clone());
        Ok(t)
    }

    pub fn flatten(&self, tree: &str) -> Result<Flat> {
        let mut out = Flat::new();
        if !tree.is_empty() {
            self.flatten_into(tree, "", &mut out)?;
        }
        Ok(out)
    }

    fn flatten_into(&self, id: &str, prefix: &str, out: &mut Flat) -> Result<()> {
        for e in &self.get_tree(id)?.entries {
            let p = format!("{prefix}{}", e.name);
            if e.mode == Mode::Dir {
                self.flatten_into(&e.id, &format!("{p}/"), out)?;
            } else {
                out.insert(p, FileEnt::new(e.mode, e.id.clone()));
            }
        }
        Ok(())
    }

    pub fn change_flat(&self, change: &str) -> Result<Flat> {
        self.flatten(&self.tree_of(change)?)
    }

    pub fn build_tree(&self, flat: &Flat) -> Result<String> {
        let edits = flat.iter().map(|(p, fe)| (p.clone(), Some(fe.clone()))).collect();
        self.update_tree("", &edits)
    }

    /// Finds path p in a tree without flattening it ("" = empty tree). A
    /// directory is returned with mode Dir.
    pub fn lookup(&self, tree: &str, p: &str) -> Result<Option<FileEnt>> {
        let (mut tree, mut p) = (tree.to_string(), p);
        while !tree.is_empty() {
            let t = self.get_tree(&tree)?;
            let (name, rest) = match p.split_once('/') {
                Some((n, r)) => (n, Some(r)),
                None => (p, None),
            };
            tree = String::new();
            if let Ok(i) = t.entries.binary_search_by(|e| e.name.as_str().cmp(name)) {
                let e = &t.entries[i];
                match rest {
                    None => return Ok(Some(FileEnt::new(e.mode, e.id.clone()))),
                    Some(r) if e.mode == Mode::Dir => (tree, p) = (e.id.clone(), r),
                    _ => {}
                }
            }
        }
        Ok(None)
    }

    /// Returns tree with the given paths set (None removes one), rewriting
    /// only the directories along those paths. Directories left empty are
    /// dropped. A file may replace a directory and the other way round, as
    /// long as everything under the old one is removed in the same edit.
    pub fn update_tree(&self, tree: &str, edits: &BTreeMap<String, Option<FileEnt>>) -> Result<String> {
        Ok(self.update_node(tree, edits)?.0)
    }

    fn update_node(&self, tree: &str, edits: &BTreeMap<String, Option<FileEnt>>) -> Result<(String, usize)> {
        let mut by_name: BTreeMap<String, Entry> = BTreeMap::new();
        if !tree.is_empty() {
            for e in &self.get_tree(tree)?.entries {
                by_name.insert(e.name.clone(), e.clone());
            }
        }
        let mut direct: BTreeMap<&str, &Option<FileEnt>> = BTreeMap::new();
        let mut sub: BTreeMap<&str, BTreeMap<String, Option<FileEnt>>> = BTreeMap::new();
        for (p, fe) in edits {
            match p.split_once('/') {
                Some((name, rest)) => {
                    sub.entry(name).or_default().insert(rest.to_string(), fe.clone());
                }
                None => {
                    direct.insert(p, fe);
                }
            }
        }
        for (name, fe) in &direct {
            match fe {
                Some(fe) => {
                    if sub.get(name).is_some_and(|s| s.values().any(Option::is_some)) {
                        bail!("path {name:?} is both a file and a directory");
                    }
                    sub.remove(name); // only removals of what this file replaces
                    by_name.insert(name.to_string(), Entry { name: name.to_string(), mode: fe.mode, id: fe.id.clone() });
                }
                None => {
                    by_name.remove(*name);
                }
            }
        }
        for (name, ed) in &sub {
            let base = match by_name.get(*name) {
                Some(e) if e.mode != Mode::Dir => bail!("path {name:?} is both a file and a directory"),
                Some(e) => e.id.clone(),
                None => String::new(),
            };
            let (id, n) = self.update_node(&base, ed)?;
            if n == 0 {
                by_name.remove(*name);
            } else {
                by_name.insert(name.to_string(), Entry { name: name.to_string(), mode: Mode::Dir, id });
            }
        }
        let mut t = Tree { entries: by_name.into_values().collect() };
        let n = t.entries.len();
        Ok((self.put_tree(&mut t)?, n))
    }

    /// Compares two trees ("" = empty) like diff_flat on their flattened
    /// forms, but skips every directory that is identical on both sides, so
    /// it only reads what changed.
    pub fn diff_trees(&self, a: &str, b: &str) -> Result<Vec<Delta>> {
        let mut out = vec![];
        self.diff_trees_into(a, b, "", &mut out)?;
        out.sort_by(|x, y| x.path.cmp(&y.path));
        Ok(out)
    }

    fn diff_trees_into(&self, a: &str, b: &str, prefix: &str, out: &mut Vec<Delta>) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let entries = |id: &str| -> Result<Vec<Entry>> {
            Ok(if id.is_empty() { vec![] } else { self.get_tree(id)?.entries.clone() })
        };
        let (ea, eb) = (entries(a)?, entries(b)?);
        // a whole file or directory added or removed
        let side = |id: &str, p: &str, added: bool, out: &mut Vec<Delta>| -> Result<()> {
            for (q, fe) in self.flatten(id)? {
                let path = format!("{p}/{q}");
                out.push(if added { Delta::of(&path, None, Some(fe)) } else { Delta::of(&path, Some(fe), None) });
            }
            Ok(())
        };
        let fe = |e: &Entry| FileEnt::new(e.mode, e.id.clone());
        let bmap: HashMap<&str, &Entry> = eb.iter().map(|e| (e.name.as_str(), e)).collect();
        for x in &ea {
            let p = format!("{prefix}{}", x.name);
            match bmap.get(x.name.as_str()) {
                Some(y) if x == *y => {}
                Some(y) if x.mode == Mode::Dir && y.mode == Mode::Dir => {
                    self.diff_trees_into(&x.id, &y.id, &format!("{p}/"), out)?
                }
                Some(y) if x.mode == Mode::Dir => {
                    side(&x.id, &p, false, out)?;
                    out.push(Delta::of(&p, None, Some(fe(y))));
                }
                Some(y) if y.mode == Mode::Dir => {
                    out.push(Delta::of(&p, Some(fe(x)), None));
                    side(&y.id, &p, true, out)?;
                }
                Some(y) => out.push(Delta::of(&p, Some(fe(x)), Some(fe(y)))),
                None if x.mode == Mode::Dir => side(&x.id, &p, false, out)?,
                None => out.push(Delta::of(&p, Some(fe(x)), None)),
            }
        }
        let amap: HashSet<&str> = ea.iter().map(|e| e.name.as_str()).collect();
        for y in eb.iter().filter(|y| !amap.contains(y.name.as_str())) {
            let p = format!("{prefix}{}", y.name);
            if y.mode == Mode::Dir {
                side(&y.id, &p, true, out)?;
            } else {
                out.push(Delta::of(&p, None, Some(fe(y))));
            }
        }
        Ok(())
    }

    /// Pairs deleted and added files into renames with the default threshold.
    pub fn with_renames(&self, ds: Vec<Delta>) -> Vec<Delta> {
        self.with_renames_at(ds, RENAME_THRESHOLD, None)
    }

    /// Pairs deleted and added files into renames: identical content first,
    /// then pairs whose content is at least threshold percent the same (0
    /// turns similarity off). Content is hashed in chunks (lines, cut at 64
    /// bytes); an inverted index from chunks to deleted files means each added
    /// file is only scored against files it actually shares content with.
    /// Added files whose paths are in work are read from the working tree.
    pub fn with_renames_at(&self, mut ds: Vec<Delta>, threshold: usize, work: Option<&HashSet<String>>) -> Vec<Delta> {
        let mut gone: HashMap<String, Vec<usize>> = HashMap::new(); // blob id -> indexes of deletions
        for (i, d) in ds.iter().enumerate() {
            if d.kind == 'D' {
                gone.entry(d.old.as_ref().unwrap().id.clone()).or_default().push(i);
            }
        }
        for v in gone.values_mut() {
            v.reverse(); // popped from the end, so the first deletion is used first
        }
        let mut drop = vec![false; ds.len()];
        for i in 0..ds.len() {
            if ds[i].kind != 'A' {
                continue;
            }
            let Some(j) = gone.get_mut(&ds[i].new.as_ref().unwrap().id).and_then(Vec::pop) else { continue };
            ds[i] = Delta { kind: 'R', old: ds[j].old.clone(), from: ds[j].path.clone(), ..ds[i].clone() };
            drop[j] = true;
        }
        let adds: Vec<usize> = (0..ds.len()).filter(|&i| ds[i].kind == 'A').collect();
        let dels: Vec<usize> = (0..ds.len()).filter(|&i| ds[i].kind == 'D' && !drop[i]).collect();
        // shortcut: above 2,000 x 2,000 candidates only exact renames are found, to keep huge diffs fast.
        if threshold > 0 && !adds.is_empty() && !dels.is_empty() && adds.len() <= 2000 && dels.len() <= 2000 {
            struct Sig {
                chunks: HashMap<u64, usize>,
                total: usize,
            }
            let sig_of = |d: &Delta, added: bool| -> Option<Sig> {
                let fe = if added { d.new.as_ref() } else { d.old.as_ref() }?;
                let data = if added && work.is_some_and(|w| w.contains(&d.path)) {
                    self.read_work(&d.path, fe.mode).ok()?
                } else {
                    self.get_blob(&fe.id).map(|b| b.to_vec()).ok()?
                };
                if data.len() >= 1 << 20 || crate::diff::is_binary(&data) {
                    return None;
                }
                let mut s = Sig { chunks: HashMap::new(), total: 0 };
                let mut rest = &data[..];
                while !rest.is_empty() {
                    let n = match rest.iter().position(|&c| c == b'\n') {
                        Some(i) if i < 64 => i + 1,
                        _ => rest.len().min(64),
                    };
                    *s.chunks.entry(fnv64a(&rest[..n])).or_default() += 1;
                    s.total += 1;
                    rest = &rest[n..];
                }
                Some(s)
            };
            let dsigs: HashMap<usize, Sig> = dels.iter().filter_map(|&d| Some((d, sig_of(&ds[d], false)?))).collect();
            let mut index: HashMap<u64, Vec<usize>> = HashMap::new(); // chunk -> deleted files containing it
            for &d in &dels {
                if let Some(s) = dsigs.get(&d) {
                    for c in s.chunks.keys() {
                        index.entry(*c).or_default().push(d);
                    }
                }
            }
            let mut pairs = vec![]; // (score, add, del)
            for &a in &adds {
                let Some(asig) = sig_of(&ds[a], true) else { continue };
                let mut cands: Vec<usize> = asig
                    .chunks
                    .keys()
                    .filter_map(|c| index.get(c))
                    .filter(|l| l.len() <= 50) // chunks everywhere ("}", blank lines) say nothing
                    .flatten()
                    .copied()
                    .collect();
                cands.sort_unstable();
                cands.dedup();
                for d in cands {
                    let dsig = &dsigs[&d];
                    let (am, dm) = (ds[a].new.as_ref().unwrap().mode, ds[d].old.as_ref().unwrap().mode);
                    if am != dm || 200 * asig.total.min(dsig.total) / (asig.total + dsig.total) < threshold {
                        continue; // even a perfect overlap could not reach the threshold
                    }
                    let common: usize =
                        asig.chunks.iter().map(|(c, n)| (*n).min(dsig.chunks.get(c).copied().unwrap_or(0))).sum();
                    let score = 200 * common / (asig.total + dsig.total);
                    if score >= threshold {
                        pairs.push((score, a, d));
                    }
                }
            }
            // best first; deterministic among ties
            pairs.sort_by(|x, y| y.0.cmp(&x.0).then_with(|| ds[x.1].path.cmp(&ds[y.1].path)).then(x.2.cmp(&y.2)));
            let mut used = HashSet::new();
            for (_, a, d) in pairs {
                if used.contains(&a) || used.contains(&d) {
                    continue;
                }
                used.insert(a);
                used.insert(d);
                drop[d] = true;
                ds[a] = Delta { kind: 'R', old: ds[d].old.clone(), from: ds[d].path.clone(), ..ds[a].clone() };
            }
        }
        ds.into_iter().zip(drop).filter(|(_, x)| !x).map(|(d, _)| d).collect()
    }

    /// Maps old path -> new path for files renamed between two trees.
    pub fn renames(&self, a: &Flat, b: &Flat) -> HashMap<String, String> {
        self.with_renames(diff_flat(a, b)).into_iter().filter(|d| d.kind == 'R').map(|d| (d.from, d.path)).collect()
    }

    // -----------------------------------------------------------------------
    // Working tree

    /// A working file's content (a symlink's target for links).
    pub fn read_work(&self, rel: &str, mode: Mode) -> std::io::Result<Vec<u8>> {
        let p = self.abs(rel);
        if mode == Mode::Link {
            return Ok(link_bytes(fs::read_link(p)?));
        }
        fs::read(p)
    }

    pub fn load_ignore(&self) -> Vec<String> {
        let Ok(s) = fs::read_to_string(self.root.join(".sprigignore")) else { return vec![] };
        s.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect()
    }

    /// Makes the working tree match `to`, given that it currently matches
    /// `from`. Ignored files and files outside a sparse view are left alone.
    pub fn materialize(&self, from: &Flat, to: &Flat) -> Result<()> {
        let cfg = self.load_config();
        for p in from.keys().filter(|p| !to.contains_key(*p) && cfg.in_view(p)) {
            self.remove_work(p)?;
        }
        let mut dirs = HashSet::new();
        for (p, te) in to.iter().filter(|(p, _)| cfg.in_view(p)) {
            let fe = from.get(p);
            if fe == Some(te) {
                continue;
            }
            let same_mode = fe.is_some_and(|fe| fe.mode == te.mode && te.mode != Mode::Link);
            self.write_file_in(p, te, Some(&mut dirs), same_mode)?;
        }
        Ok(())
    }

    /// Removes a working file and the directories it leaves empty.
    pub fn remove_work(&self, p: &str) -> Result<()> {
        let a = self.abs(p);
        match fs::remove_file(&a) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(err!("remove {}: {e}", a.display())),
            _ => {}
        }
        self.prune_dirs(a.parent().unwrap_or(&self.root));
        Ok(())
    }

    pub fn prune_dirs(&self, dir: &Path) {
        let mut dir = dir.to_path_buf();
        while dir != self.root && dir.starts_with(&self.root) {
            if fs::remove_dir(&dir).is_err() {
                return;
            }
            dir.pop();
        }
    }

    pub fn write_file(&self, rel: &str, fe: &FileEnt) -> Result<()> {
        self.write_file_in(rel, fe, None, false)
    }

    /// Writes one working file. dirs remembers directories already checked
    /// during this checkout; same_mode says the file is already there with
    /// the right permissions.
    pub fn write_file_in(&self, rel: &str, fe: &FileEnt, dirs: Option<&mut HashSet<String>>, same_mode: bool) -> Result<()> {
        if !valid_path(rel) {
            bail!("refusing to write outside the repository: {rel}");
        }
        let a = self.abs(rel);
        // Clear anything in the way: a file where a directory must go, or a
        // directory where a file must go.
        let dir = util::dir_name(rel).to_string();
        if !dirs.as_ref().is_some_and(|d| d.contains(&dir)) {
            let mut cur = self.root.clone();
            let parts: Vec<&str> = rel.split('/').collect();
            for seg in &parts[..parts.len() - 1] {
                cur.push(seg);
                if fs::symlink_metadata(&cur).is_ok_and(|m| !m.is_dir()) {
                    fs::remove_file(&cur)?;
                }
            }
            if let Some(parent) = a.parent() {
                fs::create_dir_all(parent).map_err(|e| err!("mkdir {}: {e}", parent.display()))?;
            }
            if let Some(d) = dirs {
                d.insert(dir);
            }
        }
        if fs::symlink_metadata(&a).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(&a)?;
        }
        let data = self.get_blob(&fe.id)?;
        if fe.mode == Mode::Link {
            let _ = fs::remove_file(&a);
            return symlink(&data, &a).map_err(|e| err!("symlink {}: {e}", a.display()));
        }
        let perm = if fe.mode == Mode::Exec { 0o755 } else { 0o644 };
        write_with_perm(&a, &data, perm, same_mode).map_err(|e| err!("write {}: {e}", a.display()))
    }
}

pub fn ignored(pats: &[String], rel: &str) -> bool {
    let base = util::base_name(rel);
    pats.iter().any(|p| {
        let p = p.strip_suffix('/').unwrap_or(p);
        util::path_match(p, rel) || util::path_match(p, base) || crate::store::under(rel, p) && rel != p
    })
}

pub fn has_markers(data: &[u8]) -> bool {
    let has = |m: &[u8]| data.starts_with(m) || data.windows(m.len() + 1).any(|w| w[0] == b'\n' && &w[1..] == m);
    has(b"<<<<<<< ") || has(b">>>>>>> ")
}

fn fnv64a(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &c in b {
        h ^= c as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(unix)]
pub fn link_bytes(p: std::path::PathBuf) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    p.into_os_string().into_vec()
}

#[cfg(not(unix))]
pub fn link_bytes(p: std::path::PathBuf) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn symlink(target: &[u8], at: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), at)
}

#[cfg(not(unix))]
fn symlink(target: &[u8], at: &Path) -> std::io::Result<()> {
    fs::write(at, target) // shortcut: no symlinks without unix; the target is written as the file
}

/// Writes a file; new files get perm (minus umask), existing ones are
/// chmodded unless same_mode says they already have it.
fn write_with_perm(p: &Path, data: &[u8], perm: u32, same_mode: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(perm).open(p)?;
        f.write_all(data)?;
        if !same_mode {
            f.set_permissions(fs::Permissions::from_mode(perm))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (perm, same_mode);
        fs::write(p, data)
    }
}
