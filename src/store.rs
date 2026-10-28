//! The repository: objects, refs, HEAD, the operation log, config and
//! revision parsing.

use crate::pack::{ObjCache, PackSet};
use crate::remote::Remote;
use crate::sign::Signature;
use crate::util::{self, Error, IoContext, Result, hex, nullable};
use crate::{bail, err};
use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// Object types. Everything in the store is one of these, addressed by the
// SHA-256 of "<type> <len>\x00<data>".
pub const BLOB: &str = "blob";
pub const TREE: &str = "tree";
pub const CHANGE: &str = "change";

/// The object types, interned so they can be passed around as &'static str.
pub fn obj_type(t: &str) -> Option<&'static str> {
    [BLOB, TREE, CHANGE].into_iter().find(|&k| k == t)
}

/// Whether s is a full object id (64 lowercase hex digits).
pub fn is_id(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub fn valid_ref_name(n: &str) -> bool {
    !n.is_empty()
        && n.split('/').all(|seg| {
            !seg.is_empty()
                && seg.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                && !seg.starts_with('.')
                && !seg.ends_with(".lock")
        })
}

pub fn hash_object(typ: &str, data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(format!("{typ} {}\0", data.len()).as_bytes());
    h.update(data);
    hex(&h.finalize())
}

pub fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

// ---------------------------------------------------------------------------
// Change objects: the unit of history. Unlike a git commit it carries
// structured provenance (who/what produced it, why, and what verified it),
// and unresolved merge conflicts are stored as data inside the change.

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Meta {
    /// what was asked / why
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub intent: String,
    /// tool or agent that produced it
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// model identifier, if any
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// e.g. tests=pass, review=approved
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub git_sha: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Conflict {
    pub path: String,
    /// blob ids; empty = absent on that side
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ours: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub theirs: String,
    /// blob written into the tree at merge time
    #[serde(default)]
    pub result: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Change {
    #[serde(default, deserialize_with = "nullable")]
    pub parents: Vec<String>,
    #[serde(default)]
    pub tree: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub time: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tz: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub meta: Meta,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<Signature>,
    /// set on a read-scoped view of this real change
    #[serde(default, rename = "projected_from", skip_serializing_if = "String::is_empty")]
    pub projected: String,
}

// ---------------------------------------------------------------------------

pub struct Repo {
    /// working tree root (empty for a bare repo)
    pub root: PathBuf,
    /// storage directory
    pub dir: PathBuf,
    pub bare: bool,
    /// Durable makes object and ref writes fsync before they count as done.
    /// Servers set it: an acknowledged push or landing must survive a crash.
    pub durable: bool,
    /// a cache's upstream: missing objects are fetched from it
    pub fallback: Option<Arc<Remote>>,

    pub(crate) objects: Mutex<ObjCache>,
    pub(crate) trees: Mutex<HashMap<String, Arc<crate::tree::Tree>>>,
    pub(crate) packs: Mutex<PackSet>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "BTreeMap::is_empty")]
    pub remotes: BTreeMap<String, String>,
    /// remote that missing objects are fetched from on demand
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lazy: String,
    /// the GitHub (or any git host) repository this one syncs with
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub github: String,
    /// ssh private key used for the GitHub repository (else ssh's defaults)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub github_key: String,
    /// only these path prefixes are in the working tree
    #[serde(default, deserialize_with = "nullable", skip_serializing_if = "Vec::is_empty")]
    pub sparse: Vec<String>,
    /// signing key -> who it belongs to
    #[serde(
        default,
        rename = "trusted_keys",
        deserialize_with = "nullable",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub trusted: BTreeMap<String, String>,
}

/// Whether p is pre or below it.
pub fn under(p: &str, pre: &str) -> bool {
    p == pre || (p.len() > pre.len() && p.starts_with(pre) && p.as_bytes()[pre.len()] == b'/')
}

impl Config {
    /// Whether path p is part of a sparse working tree.
    pub fn in_view(&self, p: &str) -> bool {
        self.sparse.is_empty() || self.sparse.iter().any(|pre| under(p, pre))
    }

    /// Whether a directory holds anything in the sparse view.
    pub fn dir_in_view(&self, dir: &str) -> bool {
        self.in_view(dir) || self.sparse.iter().any(|pre| under(pre, dir))
    }
}

impl Repo {
    fn at(root: PathBuf, dir: PathBuf, bare: bool) -> Repo {
        Repo {
            root,
            dir,
            bare,
            durable: false,
            fallback: None,
            objects: Default::default(),
            trees: Default::default(),
            packs: Default::default(),
        }
    }

    /// A bare repository at dir, opened without checks (a staging area, or a
    /// second handle on a repository).
    pub fn bare_at(dir: &Path) -> Repo {
        Repo::at(PathBuf::new(), dir.to_path_buf(), true)
    }

    pub fn init(root: &Path, bare: bool, branch: &str) -> Result<Repo> {
        let r = if bare { Repo::bare_at(root) } else { Repo::at(root.to_path_buf(), root.join(".sprig"), false) };
        if r.dir.join("objects").exists() {
            bail!("repository already exists in {}", r.dir.display());
        }
        for d in ["objects", "refs"] {
            fs::create_dir_all(r.dir.join(d))?;
        }
        if bare {
            fs::write(r.dir.join("BARE"), "1\n")?;
        }
        let branch = if branch.is_empty() { "main" } else { branch };
        r.set_head_ref(&format!("heads/{branch}"))?;
        Ok(r)
    }

    pub fn open(path: &Path) -> Result<Repo> {
        let abs = std::path::absolute(path)?;
        if abs.join(".sprig").join("objects").is_dir() {
            return Ok(Repo::at(abs.clone(), abs.join(".sprig"), false));
        }
        if abs.join("BARE").exists() {
            return Ok(Repo::bare_at(&abs));
        }
        bail!("not a sprig repository: {}", path.display())
    }

    pub fn find() -> Result<Repo> {
        let mut dir = std::env::current_dir()?;
        loop {
            if let Ok(r) = Repo::open(&dir) {
                return Ok(r);
            }
            if !dir.pop() {
                bail!("not inside a sprig repository (run `sprig init`)");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Objects

    pub fn obj_path(&self, id: &str) -> PathBuf {
        self.dir.join("objects").join(&id[..2]).join(&id[2..])
    }

    pub fn has_object(&self, id: &str) -> bool {
        if !is_id(id) {
            return false;
        }
        self.in_pack(id).is_some() || self.obj_path(id).exists() || self.in_pack_fresh(id).is_some()
    }

    pub fn put_object(&self, typ: &str, data: &[u8]) -> Result<String> {
        let id = hash_object(typ, data);
        if self.has_object(&id) {
            return Ok(id);
        }
        let mut z = ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 64), Compression::fast());
        z.write_all(format!("{typ} {}\0", data.len()).as_bytes())?;
        z.write_all(data)?;
        write_file_atomic(&self.obj_path(&id), &z.finish()?, self.durable)?;
        Ok(id)
    }

    /// Reads an object; in a lazy clone a missing one is fetched from the
    /// configured remote (hash-verified) and kept.
    pub fn get_object(&self, id: &str) -> Result<(&'static str, Arc<[u8]>)> {
        match self.get_local(id) {
            Err(e) if e.is_not_found() => {}
            other => return other,
        }
        let rm = match &self.fallback {
            Some(rm) => Some(rm.clone()),
            None if !self.bare => match self.load_config().lazy.as_str() {
                "" => None,
                name => Some(Arc::new(self.remote(name)?)),
            },
            None => None,
        };
        let Some(rm) = rm else {
            return Err(Error::NotFound(format!("object {} not found", short(id))));
        };
        let (typ, data) = rm.get_object(id)?;
        self.put_object(typ, &data)?;
        Ok((typ, data.into()))
    }

    pub fn get_local(&self, id: &str) -> Result<(&'static str, Arc<[u8]>)> {
        self.read_local_at(id, 0).map(|o| (o.typ, o.data))
    }

    pub(crate) fn read_loose(&self, id: &str) -> Result<(&'static str, Vec<u8>)> {
        if !is_id(id) {
            bail!("invalid object id {id:?}");
        }
        let p = self.obj_path(id);
        let f = match fs::File::open(&p) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(format!("object {} not found", short(id))));
            }
            Err(e) => return Err(e.into()),
        };
        let mut raw = Vec::new();
        ZlibDecoder::new(f).read_to_end(&mut raw).map_err(|e| err!("object {} corrupt: {e}", short(id)))?;
        let i = raw.iter().position(|&c| c == 0).ok_or_else(|| err!("object {} corrupt: no header", short(id)))?;
        let head = std::str::from_utf8(&raw[..i]).unwrap_or("");
        let typ = head.split(' ').next().and_then(obj_type);
        let data = raw.split_off(i + 1);
        match typ {
            Some(typ) if hash_object(typ, &data) == id => Ok((typ, data)),
            _ => bail!("object {} corrupt: hash mismatch", short(id)),
        }
    }

    pub fn put_blob(&self, data: &[u8]) -> Result<String> {
        self.put_object(BLOB, data)
    }

    pub fn get_blob(&self, id: &str) -> Result<Arc<[u8]>> {
        let (typ, data) = self.get_object(id)?;
        if typ != BLOB {
            bail!("object {} is a {typ}, not a blob", short(id));
        }
        Ok(data)
    }

    pub fn put_change(&self, c: &Change) -> Result<String> {
        self.put_object(CHANGE, &util::to_json(c))
    }

    pub fn get_change(&self, id: &str) -> Result<Change> {
        let (typ, data) = self.get_object(id)?;
        if typ != CHANGE {
            bail!("object {} is a {typ}, not a change", short(id));
        }
        serde_json::from_slice(&data).map_err(|e| err!("change {}: {e}", short(id)))
    }

    // -----------------------------------------------------------------------
    // Refs and HEAD

    fn ref_path(&self, name: &str) -> PathBuf {
        self.dir.join("refs").join(name)
    }

    pub fn read_ref(&self, name: &str) -> Option<String> {
        if !valid_ref_name(name) {
            return None;
        }
        let s = fs::read_to_string(self.ref_path(name)).ok()?;
        let id = s.trim();
        is_id(id).then(|| id.to_string())
    }

    pub fn write_ref(&self, name: &str, id: &str) -> Result<()> {
        if !valid_ref_name(name) {
            bail!("invalid ref name {name:?}");
        }
        if !is_id(id) {
            bail!("invalid id {id:?}");
        }
        write_file_atomic(&self.ref_path(name), format!("{id}\n").as_bytes(), self.durable)
    }

    pub fn delete_ref(&self, name: &str) -> Result<()> {
        if !valid_ref_name(name) {
            bail!("invalid ref name {name:?}");
        }
        match fs::remove_file(self.ref_path(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    pub fn list_refs(&self) -> BTreeMap<String, String> {
        fn walk(r: &Repo, dir: &Path, prefix: &str, out: &mut BTreeMap<String, String>) {
            let Ok(ents) = fs::read_dir(dir) else { return };
            for e in ents.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let full = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    walk(r, &e.path(), &full, out);
                } else if !name.starts_with(".tmp-")
                    && let Some(id) = r.read_ref(&full)
                {
                    out.insert(full, id);
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(self, &self.dir.join("refs"), "", &mut out);
        out
    }

    fn head_path(&self) -> PathBuf {
        self.dir.join("HEAD")
    }

    pub fn read_head_raw(&self) -> String {
        fs::read_to_string(self.head_path()).unwrap_or_default().trim().to_string()
    }

    pub fn set_head_ref(&self, r: &str) -> Result<()> {
        write_file_atomic(&self.head_path(), format!("ref: {r}\n").as_bytes(), false)
    }

    pub fn set_head_detached(&self, id: &str) -> Result<()> {
        write_file_atomic(&self.head_path(), format!("{id}\n").as_bytes(), false)
    }

    /// The symbolic ref ("" if detached) and the change id HEAD points to
    /// ("" for an unborn branch).
    pub fn head(&self) -> (String, String) {
        let raw = self.read_head_raw();
        if let Some(r) = raw.strip_prefix("ref: ") {
            let id = self.read_ref(r).unwrap_or_default();
            return (r.to_string(), id);
        }
        if is_id(&raw) {
            return (String::new(), raw);
        }
        (String::new(), String::new())
    }

    pub fn advance_head(&self, id: &str) -> Result<()> {
        let (r, _) = self.head();
        if r.is_empty() { self.set_head_detached(id) } else { self.write_ref(&r, id) }
    }

    // -----------------------------------------------------------------------
    // Operation log: every command that moves refs/HEAD is recorded with
    // the state before and after, so any of them can be undone in one step.

    pub fn capture_state(&self) -> State {
        State { head: self.read_head_raw(), refs: self.list_refs() }
    }

    pub fn restore_state(&self, s: &State) -> Result<()> {
        for name in self.list_refs().keys() {
            if !s.refs.contains_key(name) {
                self.delete_ref(name)?;
            }
        }
        for (name, id) in &s.refs {
            self.write_ref(name, id)?;
        }
        write_file_atomic(&self.head_path(), format!("{}\n", s.head).as_bytes(), false)
    }

    fn ops_path(&self) -> PathBuf {
        self.dir.join("oplog.jsonl")
    }

    pub fn ops(&self) -> Result<Vec<Op>> {
        let b = match fs::read(self.ops_path()) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            r => r?,
        };
        b.split(|&c| c == b'\n')
            .filter(|l| !l.trim_ascii().is_empty())
            .map(|l| serde_json::from_slice(l).map_err(|e| err!("oplog corrupt: {e}")))
            .collect()
    }

    fn record_op(&self, cmd: &str, before: State) -> Result<()> {
        let after = self.capture_state();
        if before == after {
            return Ok(());
        }
        let n = self.ops()?.len() + 1;
        let op = Op { n, time: util::now_unix(), cmd: cmd.to_string(), before, after };
        let mut line = util::to_json(&op);
        line.push(b'\n');
        let mut f = fs::OpenOptions::new().append(true).create(true).open(self.ops_path())?;
        f.write_all(&line)?;
        Ok(())
    }

    /// Runs f and records the resulting ref/HEAD movement in the oplog, even
    /// if f failed part-way.
    pub fn record<T>(&self, cmd: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let before = self.capture_state();
        let res = f();
        let rec = self.record_op(cmd, before);
        let v = res?;
        rec?;
        Ok(v)
    }

    // -----------------------------------------------------------------------
    // Config

    fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    pub fn load_config(&self) -> Config {
        fs::read(self.config_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_config(&self, c: &Config) -> Result<()> {
        let mut b = util::to_json_pretty(c);
        b.push('\n');
        write_file_atomic(&self.config_path(), b.as_bytes(), false)
    }

    pub fn author(&self) -> String {
        author_from(&self.load_config())
    }

    // -----------------------------------------------------------------------
    // Revision resolution

    pub fn resolve_rev(&self, s: &str) -> Result<String> {
        let mut s = s.trim();
        let mut steps = 0;
        if let Some(i) = s.rfind('~') {
            let rest = &s[i + 1..];
            steps = if rest.is_empty() { 1 } else { leading_int(rest).ok_or_else(|| err!("bad revision {s:?}"))? };
            s = &s[..i];
        }
        let mut id = self.resolve_base(s)?;
        for _ in 0..steps {
            let c = self.get_change(&id)?;
            match c.parents.first() {
                Some(p) => id = p.clone(),
                None => bail!("{} has no parent", short(&id)),
            }
        }
        Ok(id)
    }

    fn resolve_base(&self, s: &str) -> Result<String> {
        if s.is_empty() || s == "HEAD" || s == "@" {
            let (_, id) = self.head();
            if id.is_empty() {
                bail!("no changes yet");
            }
            return Ok(id);
        }
        for cand in [format!("heads/{s}"), format!("remotes/{s}"), s.to_string()] {
            if let Some(id) = self.read_ref(&cand) {
                return Ok(id);
            }
        }
        if is_id(s) {
            if !self.has_object(s) {
                bail!("unknown change {}", short(s));
            }
            return Ok(s.to_string());
        }
        if s.len() >= 4 && s.len() < 64 && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
            let mut cands = self.packed_prefix(s);
            if let Ok(ents) = fs::read_dir(self.dir.join("objects").join(&s[..2])) {
                for e in ents.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if n.starts_with(&s[2..]) {
                        cands.push(format!("{}{n}", &s[..2]));
                    }
                }
            }
            let mut found: Vec<String> =
                cands.into_iter().filter(|id| matches!(self.get_object(id), Ok((CHANGE, _)))).collect();
            found.sort();
            found.dedup();
            match found.len() {
                1 => return Ok(found.remove(0)),
                0 => {}
                _ => bail!("ambiguous change prefix {s:?}"),
            }
        }
        bail!("unknown revision {s:?}")
    }

    /// The tree of a change ("" for none).
    pub fn tree_of(&self, change: &str) -> Result<String> {
        if change.is_empty() {
            return Ok(String::new());
        }
        Ok(self.get_change(change)?.tree)
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

/// Like Go's fmt.Sscanf("%d"): an optionally signed integer at the start.
fn leading_int(s: &str) -> Option<usize> {
    let end = s.bytes().position(|c| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

pub fn author_from(c: &Config) -> String {
    if let Ok(a) = std::env::var("SPRIG_AUTHOR")
        && !a.is_empty()
    {
        return a;
    }
    if !c.author.is_empty() {
        return c.author.clone();
    }
    let u = std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "unknown".into());
    format!("{u} <{u}@localhost>")
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct State {
    #[serde(default)]
    pub head: String,
    #[serde(default, deserialize_with = "nullable")]
    pub refs: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Op {
    pub n: usize,
    pub time: i64,
    pub cmd: String,
    pub before: State,
    pub after: State,
}

/// Replaces path in one step (write a temp file, rename it). With durable
/// set it also fsyncs the file before the rename and the directory after it,
/// so the new content survives a crash or power loss.
pub fn write_file_atomic(path: &Path, data: &[u8], durable: bool) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).ctx("mkdir", dir)?;
    let mut rnd = [0u8; 8];
    getrandom::fill(&mut rnd).map_err(|e| err!("random: {e}"))?;
    let tmp = dir.join(format!(".tmp-{}", hex(&rnd)));
    let res = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
        if durable {
            f.sync_all()?;
        }
        drop(f);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(err!("write {}: {e}", path.display()));
    }
    if durable {
        sync_dir(dir)?;
    }
    Ok(())
}

pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir; // some platforms can't sync directories
    Ok(())
}

/// Children validates a serialized object and returns the ids it
/// references. The server uses it to guarantee closure: an object is only
/// accepted once everything it points at is already present.
pub fn children(typ: &str, data: &[u8]) -> Result<Vec<String>> {
    match typ {
        BLOB => Ok(vec![]),
        TREE => Ok(crate::tree::parse_tree(data)?.entries.into_iter().map(|e| e.id).collect()),
        CHANGE => {
            let c: Change = serde_json::from_slice(data).map_err(|e| err!("bad change: {e}"))?;
            let mut ids = vec![];
            for p in c.parents {
                if !is_id(&p) {
                    bail!("bad parent id {p:?}");
                }
                ids.push(p);
            }
            if !is_id(&c.tree) {
                bail!("bad tree id {:?}", c.tree);
            }
            ids.push(c.tree);
            for cf in c.conflicts {
                if !valid_path(&cf.path) {
                    bail!("bad conflict path {:?}", cf.path);
                }
                for b in [cf.base, cf.ours, cf.theirs, cf.result] {
                    if b.is_empty() {
                        continue;
                    }
                    if !is_id(&b) {
                        bail!("bad conflict blob id {b:?}");
                    }
                    ids.push(b);
                }
            }
            Ok(ids)
        }
        _ => bail!("unknown object type {typ:?}"),
    }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\0'])
}

pub fn valid_path(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && p.split('/').all(valid_name)
}
