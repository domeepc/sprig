//! The server: the agent-facing side. Everything a CI job or tool needs to
//! ask about the repo (history, files, refs) is an API call; no local clone.
//!
//! Landing: the client sends only the files it changed and the change it
//! edited against, and the server applies them onto the branch's current
//! tip. The merge runs outside the lock; only the ref swap is serialized, and
//! it retries on a fresh tip if another landing got in first, so landings
//! don't queue behind each other and nothing has to be cloned or pushed. The
//! lock is a file lock, so several server processes can share one
//! repository.
//!
//! GET /v1/refs?wait=<etag> answers at once if refs differ from that etag,
//! otherwise when they change (or after ~25 s with the same etag). Caches
//! use it to keep a fresh copy of upstream refs; a standby (`serve
//! --follow`) uses it to mirror another server.

use crate::pack::MAX_OBJECT_SIZE;
use crate::project::{scope_key, Projection};
use crate::remote::{read_records, write_record, LandRequest, LandResult, RefUpdate, Remote, BATCH_BYTES, BATCH_IDS};
use crate::sign::{check_sig, LandProof, Signature};
use crate::store::{children, hash_object, is_id, short, valid_path, valid_ref_name, Change, Repo, BLOB};
use crate::tree::{FileEnt, Flat, Mode};
use crate::util::{self, Error, Result};
use crate::{bail, err};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const MAX_LAND_RETRIES: usize = 20;
const LONG_POLL: Duration = Duration::from_secs(25);

/// Limits a token. In tokens.json a plain list means Write only.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scope {
    /// path prefixes it may land in
    #[serde(default, deserialize_with = "util::nullable")]
    pub write: Vec<String>,
    /// if set, the only paths it may read
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Vec<String>>,
}

impl Scope {
    pub fn from_json(v: &serde_json::Value) -> Result<Scope> {
        if let Ok(list) = serde_json::from_value::<Vec<String>>(v.clone()) {
            return Ok(Scope { write: list, read: None });
        }
        Ok(serde_json::from_value(v.clone())?)
    }
}

pub fn in_scope(scope: &[String], p: &str) -> bool {
    scope.iter().any(|pre| crate::store::under(p, pre))
}

pub struct Server {
    pub repo: Arc<Repo>,
    /// full access
    pub token: String,
    /// limited tokens
    pub scopes: BTreeMap<String, Scope>,
    /// set when this server is a cache in front of another
    pub upstream: Option<Arc<Remote>>,
    /// set when this server is a read-only standby of another
    pub follow: Option<Arc<Remote>>,

    stopped: AtomicBool,
    listeners: Mutex<Vec<Arc<tiny_http::Server>>>,
    /// bumped when refs change (long-poll wake-up)
    refs_gen: Mutex<u64>,
    refs_cv: Condvar,
    /// a cache's copy of upstream refs, kept by follow_upstream; None when stale
    up_refs: Mutex<Option<BTreeMap<String, String>>>,
    projections: Mutex<HashMap<String, Arc<Mutex<Projection>>>>,
}

/// One HTTP request, read whole.
pub struct Req {
    pub method: String,
    pub path: String,
    query: HashMap<String, String>,
    raw_url: String,
    auth: Option<String>,
    sprig_type: String,
    content_type: String,
    body: Vec<u8>,
    scope: Option<Scope>,
}

impl Req {
    fn q(&self, k: &str) -> &str {
        self.query.get(k).map_or("", String::as_str)
    }

    /// Whether path p, or something below it, is readable: (whole, part).
    fn may_read(&self, p: &str) -> (bool, bool) {
        let Some(read) = self.scope.as_ref().and_then(|s| s.read.as_ref()) else { return (true, true) };
        if in_scope(read, p) {
            return (true, true);
        }
        let part = read.iter().any(|pre| p.is_empty() || pre.starts_with(&format!("{p}/")));
        (false, part)
    }

    fn read_limited(&self) -> bool {
        self.scope.as_ref().is_some_and(|s| s.read.is_some())
    }

    fn token_kind(&self) -> &'static str {
        match (&self.scope, &self.auth) {
            (Some(_), _) => "scoped token",
            (None, Some(a)) if !a.is_empty() => "full token",
            _ => "no token",
        }
    }
}

/// A response.
pub struct Out {
    code: u16,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

fn json<T: Serialize + ?Sized>(code: u16, v: &T) -> Out {
    let mut body = util::to_json(v);
    body.push(b'\n');
    Out { code, body, headers: vec![("Content-Type", "application/json".into())] }
}

fn http_err(code: u16, msg: impl std::fmt::Display) -> Out {
    json(code, &serde_json::json!({ "error": msg.to_string() }))
}

fn raw(data: &[u8], hdr: Vec<(&'static str, String)>) -> Out {
    let mut headers = vec![("Content-Type", "application/octet-stream".to_string())];
    headers.extend(hdr);
    Out { code: 200, body: data.to_vec(), headers }
}

/// Decodes %XX escapes, and '+' as a space in query strings.
fn percent_decode(s: &str, query: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let esc = (b[i] == b'%').then(|| b.get(i + 1..i + 3)).flatten();
        if let Some(v) = esc.and_then(|h| util::unhex(std::str::from_utf8(h).ok()?)) {
            out.push(v[0]);
            i += 3;
            continue;
        }
        out.push(if query && b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn refs_etag(refs: &BTreeMap<String, String>) -> String {
    let s: String = refs.iter().map(|(n, id)| format!("{n} {id}\n")).collect();
    hash_object("refs", s.as_bytes())[..16].to_string()
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct ReflogEntry {
    #[serde(default)]
    pub time: i64,
    #[serde(default, rename = "ref")]
    pub reference: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub old: String,
    /// empty: deleted
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub new: String,
    /// land | push | delete
    #[serde(default)]
    pub via: String,
    /// landing author, or the kind of token
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub who: String,
}

// The server's reflog: one JSON line per ref change, appended under the ref
// lock. It is for recovery ("what did main point at before?") and auditing.
impl Repo {
    fn append_reflog(&self, mut e: ReflogEntry) -> Result<()> {
        use std::io::Write;
        e.time = util::now_unix();
        let mut line = util::to_json(&e);
        line.push(b'\n');
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("reflog.jsonl"))?;
        f.write_all(&line)?;
        if self.durable {
            f.sync_all()?;
        }
        Ok(())
    }

    pub fn reflog(&self) -> Result<Vec<ReflogEntry>> {
        let b = match std::fs::read(self.dir.join("reflog.jsonl")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            r => r?,
        };
        // a torn last line after a crash is skipped
        Ok(b.split(|&c| c == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect())
    }

    /// An exclusive lock shared by every process using this repository, so
    /// ref compare-and-swaps from several servers cannot interleave.
    fn lock_refs(&self) -> Result<std::fs::File> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("refs.lock"))?;
        f.lock()?; // dropping the file releases it
        Ok(f)
    }
}

impl Server {
    pub fn new(repo: Repo, token: &str) -> Server {
        Server {
            repo: Arc::new(repo),
            token: token.into(),
            scopes: BTreeMap::new(),
            upstream: None,
            follow: None,
            stopped: AtomicBool::new(false),
            listeners: Mutex::new(vec![]),
            refs_gen: Mutex::new(0),
            refs_cv: Condvar::new(),
            up_refs: Mutex::new(None),
            projections: Mutex::new(HashMap::new()),
        }
    }

    /// Ends long-polls, background followers and listeners.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.notify_refs();
        for l in self.listeners.lock().unwrap().iter() {
            l.unblock();
        }
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Listens on addr and serves in background threads; returns the URL.
    pub fn start(self: &Arc<Self>, addr: &str) -> Result<String> {
        let l = Arc::new(tiny_http::Server::http(addr).map_err(|e| err!("listen {addr}: {e}"))?);
        let url = match l.server_addr() {
            tiny_http::ListenAddr::IP(a) => format!("http://{a}"),
            #[allow(unreachable_patterns)]
            _ => bail!("not an IP address: {addr}"),
        };
        self.listeners.lock().unwrap().push(l.clone());
        if self.upstream.is_some() {
            let s = self.clone();
            std::thread::spawn(move || s.follow_upstream());
        }
        if self.follow.is_some() {
            let s = self.clone();
            std::thread::spawn(move || s.run_follow());
        }
        let s = self.clone();
        std::thread::spawn(move || {
            while let Ok(req) = l.recv() {
                if s.is_stopped() {
                    break;
                }
                let s = s.clone();
                let _ = std::thread::Builder::new().stack_size(8 << 20).spawn(move || s.serve_one(req));
            }
        });
        Ok(url)
    }

    fn serve_one(&self, mut req: tiny_http::Request) {
        let url = req.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        let path = percent_decode(path, false);
        let header = |k: &'static str| {
            req.headers().iter().find(|h| h.field.equiv(k)).map(|h| h.value.as_str().to_string())
        };
        let limit = match path.as_str() {
            "/v1/objects/missing" | "/v1/objects/get" => 8 << 20,
            "/v1/objects/put" => BATCH_BYTES + MAX_OBJECT_SIZE,
            p if p.starts_with("/v1/refs/") => 1 << 20,
            _ => MAX_OBJECT_SIZE,
        };
        let mut r = Req {
            method: req.method().as_str().to_string(),
            path,
            query: query
                .split('&')
                .filter(|kv| !kv.is_empty())
                .map(|kv| {
                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                    (percent_decode(k, true), percent_decode(v, true))
                })
                .collect(),
            raw_url: url.clone(),
            auth: header("Authorization"),
            sprig_type: header("X-Sprig-Type").unwrap_or_default(),
            content_type: header("Content-Type").unwrap_or_default(),
            body: vec![],
            scope: None,
        };
        let mut body = vec![];
        let out = match req.as_reader().take(limit as u64 + 1).read_to_end(&mut body) {
            _ if body.len() > limit => http_err(413, "body too large"),
            Err(_) => http_err(400, "body unreadable"),
            Ok(_) => {
                r.body = body;
                self.handle(r)
            }
        };
        let mut resp = tiny_http::Response::from_data(out.body).with_status_code(out.code);
        for (k, v) in out.headers {
            if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                resp = resp.with_header(h);
            }
        }
        let _ = req.respond(resp);
    }

    /// Answers one request: authentication, then routing.
    pub fn handle(&self, mut r: Req) -> Out {
        if !self.token.is_empty() || !self.scopes.is_empty() {
            let got = r.auth.as_deref().unwrap_or("");
            let got = got.strip_prefix("Bearer ").unwrap_or(got).as_bytes();
            if self.token.is_empty() || !ct_eq(got, self.token.as_bytes()) {
                match self.scopes.iter().find(|(tok, _)| ct_eq(got, tok.as_bytes())) {
                    Some((_, sc)) => r.scope = Some(sc.clone()),
                    None => return http_err(401, "unauthorized"),
                }
            }
        }
        if let Some(up) = &self.upstream {
            // Cache tier: objects never change, so they are served locally
            // once fetched; everything else (refs, log, ...) goes upstream.
            // Read-scoped tokens see a projection only the main server has.
            let local = !r.read_limited()
                && match (r.method.as_str(), r.path.as_str()) {
                    ("GET", p) => p.starts_with("/v1/object/") || p.starts_with("/v1/changes/"),
                    ("POST", p) => p == "/v1/objects/get" || p == "/v1/land", // merge here; only the ref swap goes upstream
                    _ => false,
                };
            if !local {
                return self.proxy(up, &r);
            }
        }
        if let Some(f) = &self.follow {
            let write = r.method == "PUT"
                || r.method == "POST"
                    && (r.path.starts_with("/v1/refs/") || r.path == "/v1/land" || r.path == "/v1/objects/put");
            if write {
                return http_err(503, format!("read-only standby of {}; send writes there", f.base()));
            }
        }
        let tail = |pre: &str| r.path.strip_prefix(pre).map(str::to_string);
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/v1/refs") => self.handle_refs(&r),
            ("GET", p) if p.starts_with("/v1/object/") => self.handle_get_object(&r, &tail("/v1/object/").unwrap()),
            ("PUT", p) if p.starts_with("/v1/object/") => self.handle_put_object(&r, &tail("/v1/object/").unwrap()),
            ("POST", "/v1/objects/missing") => self.handle_missing(&r),
            ("POST", "/v1/objects/get") => self.handle_get_objects(&r),
            ("POST", "/v1/objects/put") => self.handle_put_objects(&r),
            ("POST", p) if p.starts_with("/v1/refs/") => self.handle_update_ref(&r, &tail("/v1/refs/").unwrap()),
            ("POST", "/v1/land") => self.handle_land(&r),
            ("GET", "/v1/log") => self.handle_log(&r),
            ("GET", p) if p.starts_with("/v1/changes/") => self.handle_change(&r, &tail("/v1/changes/").unwrap()),
            ("GET", "/v1/file") => self.handle_file(&r),
            ("GET", "/v1/ls") => self.handle_ls(&r),
            _ => http_err(404, "404 page not found"),
        }
    }

    fn proxy(&self, up: &Remote, r: &Req) -> Out {
        let path = r.raw_url.as_str();
        let mut hdr = vec![("Authorization", r.auth.as_deref().unwrap_or(""))];
        if !r.sprig_type.is_empty() {
            hdr.push(("X-Sprig-Type", &r.sprig_type));
        }
        if !r.content_type.is_empty() {
            hdr.push(("Content-Type", &r.content_type));
        }
        match up.send(&r.method, path, &r.body, &hdr) {
            Ok(resp) => {
                let mut headers = vec![];
                for (k, v) in [("Content-Type", resp.content_type), ("X-Sprig-Type", resp.typ), ("X-Sprig-Mode", resp.mode)] {
                    if !v.is_empty() {
                        headers.push((k, v));
                    }
                }
                Out { code: resp.status, body: resp.data, headers }
            }
            Err(e) => http_err(502, format!("upstream: {e}")),
        }
    }

    // -----------------------------------------------------------------------
    // Projections

    /// The request's projection, or None for tokens that may read everything.
    fn view_of(&self, r: &Req) -> Option<Arc<Mutex<Projection>>> {
        let read = r.scope.as_ref()?.read.as_ref()?;
        let mut m = self.projections.lock().unwrap();
        Some(m.entry(scope_key(read)).or_insert_with(|| Arc::new(Mutex::new(Projection::load(&self.repo, read)))).clone())
    }

    // -----------------------------------------------------------------------
    // Refs

    fn notify_refs(&self) {
        *self.refs_gen.lock().unwrap() += 1;
        self.refs_cv.notify_all();
    }

    fn list_refs_for(&self, r: &Req) -> BTreeMap<String, String> {
        let refs = self.repo.list_refs();
        match self.view_of(r) {
            Some(v) => v.lock().unwrap().project_refs(&self.repo, &refs).unwrap_or_default(),
            None => refs,
        }
    }

    fn handle_refs(&self, r: &Req) -> Out {
        let mut refs = self.list_refs_for(r);
        let mut etag = refs_etag(&refs);
        let wait = r.q("wait");
        if !wait.is_empty() && wait == etag {
            let deadline = Instant::now() + LONG_POLL;
            while etag == wait && Instant::now() < deadline && !self.is_stopped() {
                // other processes sharing the repo don't notify us: poll too
                let g = self.refs_gen.lock().unwrap();
                let seen = *g;
                let _ = self.refs_cv.wait_timeout_while(g, Duration::from_millis(250), |g| *g == seen && !self.is_stopped());
                refs = self.list_refs_for(r);
                etag = refs_etag(&refs);
            }
        }
        json(200, &serde_json::json!({ "refs": refs, "etag": etag }))
    }

    /// Records a ref change; called with the ref lock held. A failure to
    /// log does not undo the change, but is reported on stderr.
    fn log_ref(&self, e: ReflogEntry) {
        if let Err(err) = self.repo.append_reflog(e) {
            eprintln!("sprig: reflog: {err}");
        }
        self.notify_refs();
    }

    fn handle_update_ref(&self, r: &Req, name: &str) -> Out {
        if !valid_ref_name(name) {
            return http_err(400, "bad ref name");
        }
        let Ok(u) = serde_json::from_slice::<RefUpdate>(&r.body) else { return http_err(400, "bad json") };
        if r.scope.is_some() {
            return http_err(403, "this token may only land changes, not move refs");
        }
        let _lock = match self.repo.lock_refs() {
            Ok(l) => l,
            Err(e) => return http_err(500, e),
        };
        let cur = self.repo.read_ref(name).unwrap_or_default();
        if cur != u.old {
            return http_err(409, format!("ref moved: expected {:?}, found {:?}", short(&u.old), short(&cur)));
        }
        if u.new.is_empty() {
            if let Err(e) = self.repo.delete_ref(name) {
                return http_err(500, e);
            }
            self.log_ref(ReflogEntry { reference: name.into(), old: cur, via: "delete".into(), who: r.token_kind().into(), ..Default::default() });
            return json(200, &serde_json::json!({ "ok": "deleted" }));
        }
        let c = match self.repo.get_change(&u.new) {
            Ok(c) => c,
            Err(e) => return http_err(400, format!("new value is not a known change: {e}")),
        };
        if !u.force {
            if !cur.is_empty() {
                match self.repo.is_ancestor(&cur, &u.new) {
                    Err(e) => return http_err(500, e),
                    Ok(false) => return http_err(409, "non-fast-forward update rejected"),
                    Ok(true) => {}
                }
            }
            // Policy gate: unresolved merge conflicts may not land on a ref.
            if let Some(cf) = c.conflicts.first() {
                return http_err(422, format!("tip change has {} unresolved conflict(s): {}", c.conflicts.len(), cf.path));
            }
        }
        if let Err(e) = self.repo.write_ref(name, &u.new) {
            return http_err(500, e);
        }
        self.log_ref(ReflogEntry {
            reference: name.into(),
            old: cur,
            new: u.new,
            via: "push".into(),
            who: r.token_kind().into(),
            ..Default::default()
        });
        json(200, &serde_json::json!({ "ok": "updated" }))
    }

    /// A branch tip, from upstream when this server is a cache.
    fn read_ref(&self, name: &str) -> Result<String> {
        let Some(up) = &self.upstream else { return Ok(self.repo.read_ref(name).unwrap_or_default()) };
        if let Some(refs) = &*self.up_refs.lock().unwrap() {
            return Ok(refs.get(name).cloned().unwrap_or_default());
        }
        Ok(up.refs()?.remove(name).unwrap_or_default())
    }

    /// Moves name from old to new, reporting true if it no longer pointed at
    /// old. A cache sends this one compare-and-swap upstream; it is the only
    /// step of a landing that the authoritative server has to do itself.
    fn swap_ref(&self, name: &str, old: &str, new: &str, who: &str) -> Result<bool> {
        if let Some(up) = &self.upstream {
            let res = up.update_ref(name, old, new, false);
            let mut cache = self.up_refs.lock().unwrap();
            return match res {
                Err(Error::Remote { code: 409, msg }) if msg.starts_with("ref moved") => {
                    *cache = None; // our copy is stale: ask upstream next time
                    Ok(true)
                }
                Err(e) => Err(e),
                Ok(()) => {
                    if let Some(refs) = cache.as_mut() {
                        refs.insert(name.into(), new.into());
                    }
                    Ok(false)
                }
            };
        }
        let _lock = self.repo.lock_refs()?;
        if self.repo.read_ref(name).unwrap_or_default() != old {
            return Ok(true);
        }
        self.repo.write_ref(name, new)?;
        self.log_ref(ReflogEntry { reference: name.into(), old: old.into(), new: new.into(), via: "land".into(), who: who.into(), ..Default::default() });
        Ok(false)
    }

    /// Keeps a cache's copy of upstream refs current, so landings start from
    /// the right tip without asking first.
    fn follow_upstream(&self) {
        let up = self.upstream.as_ref().unwrap();
        let mut etag = String::new();
        while !self.is_stopped() {
            match up.wait_refs(&etag) {
                Ok((refs, next)) => {
                    *self.up_refs.lock().unwrap() = Some(refs);
                    etag = next;
                }
                Err(_) => {
                    *self.up_refs.lock().unwrap() = None;
                    self.sleep_or_stop(Duration::from_secs(1));
                    etag.clear();
                }
            }
        }
    }

    fn sleep_or_stop(&self, d: Duration) {
        let g = self.refs_gen.lock().unwrap();
        let _ = self.refs_cv.wait_timeout_while(g, d, |_| !self.is_stopped());
    }

    /// Mirrors another server into this repo: for every ref change, fetch
    /// what is missing (dependencies first), then move the ref.
    fn run_follow(&self) {
        let f = self.follow.as_ref().unwrap();
        let mut etag = String::new();
        while !self.is_stopped() {
            match f.wait_refs(&etag).and_then(|(refs, next)| self.mirror(f, &refs).map(|_| next)) {
                Ok(next) => etag = next,
                Err(e) => {
                    if self.is_stopped() {
                        return;
                    }
                    eprintln!("sprig: following {}: {e}", f.base());
                    self.sleep_or_stop(Duration::from_secs(1));
                    etag.clear();
                }
            }
        }
    }

    fn mirror(&self, f: &Remote, refs: &BTreeMap<String, String>) -> Result<()> {
        let local = self.repo.list_refs();
        for (name, id) in refs {
            if local.get(name) == Some(id) || !valid_ref_name(name) {
                continue;
            }
            self.repo.fetch_tip(f, id)?;
            self.repo.write_ref(name, id)?;
        }
        for name in local.keys().filter(|n| !refs.contains_key(*n)) {
            self.repo.delete_ref(name)?;
        }
        self.notify_refs();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Objects

    /// Copies an object from upstream into a cache server's store.
    fn fill(&self, id: &str) {
        if let Some(up) = &self.upstream {
            if !self.repo.has_object(id) {
                if let Ok((typ, data)) = up.get_object(id) {
                    let _ = self.repo.put_object(typ, &data); // hash-verified
                }
            }
        }
    }

    fn handle_get_object(&self, r: &Req, id: &str) -> Out {
        if !is_id(id) {
            return http_err(400, "bad id");
        }
        if self.view_of(r).is_some_and(|v| !v.lock().unwrap().may(id)) {
            return http_err(403, format!("object {} is not readable with this token", short(id)));
        }
        self.fill(id);
        match self.repo.get_object(id) {
            Ok((typ, data)) => raw(&data, vec![("X-Sprig-Type", typ.into())]),
            Err(e) => http_err(404, e),
        }
    }

    /// Why an object may not be stored, or None.
    fn check_object(&self, typ: &str, data: &[u8]) -> Option<String> {
        match children(typ, data) {
            Err(e) => Some(e.to_string()),
            Ok(kids) => kids.iter().find(|k| !self.repo.has_object(k)).map(|k| format!("missing dependency {}", short(k))),
        }
    }

    fn handle_put_object(&self, r: &Req, id: &str) -> Out {
        if r.read_limited() {
            return http_err(403, "a read-scoped token can't upload objects; use land");
        }
        if !is_id(id) {
            return http_err(400, "bad id");
        }
        if hash_object(&r.sprig_type, &r.body) != id {
            return http_err(400, "hash mismatch");
        }
        if let Some(msg) = self.check_object(&r.sprig_type, &r.body) {
            return http_err(409, msg);
        }
        match self.repo.put_object(&r.sprig_type, &r.body) {
            Ok(_) => json(200, &serde_json::json!({ "id": id })),
            Err(e) => http_err(500, e),
        }
    }

    fn ids_body(r: &Req, max: usize) -> std::result::Result<Vec<String>, Out> {
        #[derive(Deserialize)]
        struct B {
            #[serde(default, deserialize_with = "util::nullable")]
            ids: Vec<String>,
        }
        let Ok(b) = serde_json::from_slice::<B>(&r.body) else { return Err(http_err(400, "bad json")) };
        if b.ids.len() > max {
            return Err(http_err(400, "too many ids"));
        }
        if let Some(bad) = b.ids.iter().find(|id| !is_id(id)) {
            return Err(http_err(400, format!("bad id {bad:?}")));
        }
        Ok(b.ids)
    }

    fn handle_missing(&self, r: &Req) -> Out {
        if r.read_limited() {
            return http_err(403, "a read-scoped token can't upload objects; use land");
        }
        match Self::ids_body(r, 20000) {
            Err(o) => o,
            Ok(ids) => {
                let missing: Vec<&String> = ids.iter().filter(|id| !self.repo.has_object(id)).collect();
                json(200, &serde_json::json!({ "missing": missing }))
            }
        }
    }

    /// Returns the requested objects it has, stopping early once the
    /// response is large; the client asks again for the rest.
    fn handle_get_objects(&self, r: &Req) -> Out {
        let ids = match Self::ids_body(r, BATCH_IDS) {
            Err(o) => return o,
            Ok(ids) => ids,
        };
        if let Some(v) = self.view_of(r) {
            let v = v.lock().unwrap();
            if let Some(id) = ids.iter().find(|id| !v.may(id)) {
                return http_err(403, format!("object {} is not readable with this token", short(id)));
            }
        }
        if let Some(up) = &self.upstream {
            let need: Vec<String> = ids.iter().filter(|id| !self.repo.has_object(id)).cloned().collect();
            if !need.is_empty() {
                let _ = up.get_objects(&need, |_, typ, data| self.repo.put_object(typ, &data).map(|_| ()));
            }
        }
        let mut out = vec![];
        for id in &ids {
            if out.len() >= BATCH_BYTES {
                break;
            }
            if let Ok((typ, data)) = self.repo.get_object(id) {
                write_record(&mut out, id, typ, &data); // absent: the client notices and reports it
            }
        }
        raw(&out, vec![])
    }

    /// Stores a record stream in order. Each object's dependencies must
    /// already be stored, here or earlier in the same stream.
    fn handle_put_objects(&self, r: &Req) -> Out {
        if r.read_limited() {
            return http_err(403, "a read-scoped token can't upload objects; use land");
        }
        let mut n = 0;
        let res = read_records(&r.body, |id, typ, data| {
            if let Some(msg) = self.check_object(typ, &data) {
                bail!("{}: {msg}", short(id));
            }
            self.repo.put_object(typ, &data)?;
            n += 1;
            Ok(())
        });
        if let Err(e) = res {
            return http_err(409, format!("{e} (stored {n} objects before it)"));
        }
        self.repo.maybe_auto_gc();
        json(200, &serde_json::json!({ "stored": n }))
    }

    // -----------------------------------------------------------------------
    // Reading

    fn resolve(&self, r: &Req) -> Result<String> {
        let rf = r.q("ref");
        self.repo.resolve_rev(if rf.is_empty() { "main" } else { rf })
    }

    fn handle_log(&self, r: &Req) -> Out {
        let mut id = match self.resolve(r) {
            Ok(id) => id,
            Err(e) => return http_err(404, e),
        };
        if let Some(v) = self.view_of(r) {
            id = match v.lock().unwrap().project(&self.repo, &id) {
                Ok(id) => id,
                Err(e) => return http_err(500, e),
            };
            if id.is_empty() {
                return json(200, &serde_json::json!([]));
            }
        }
        let limit = r.q("limit").parse().ok().filter(|v| (1..=1000).contains(v)).unwrap_or(50);
        let mut out = vec![];
        let res = self.repo.walk(&id, |id, c| {
            out.push(LogItem { id: id.into(), change: c.clone() });
            out.len() < limit
        });
        match res {
            Ok(()) => json(200, &out),
            Err(e) => http_err(500, e),
        }
    }

    fn handle_change(&self, r: &Req, id: &str) -> Out {
        if self.view_of(r).is_some_and(|v| !v.lock().unwrap().may(id)) {
            return http_err(404, format!("no change {}", short(id)));
        }
        if is_id(id) {
            self.fill(id);
        }
        match self.repo.get_change(id) {
            Ok(c) => json(200, &c),
            Err(e) => http_err(404, e),
        }
    }

    fn handle_file(&self, r: &Req) -> Out {
        let id = match self.resolve(r) {
            Ok(id) => id,
            Err(e) => return http_err(404, e),
        };
        let p = r.q("path").trim_matches('/');
        if !r.may_read(p).0 {
            return http_err(403, format!("this token may not read {p:?}"));
        }
        let fe = match self.repo.tree_of(&id).and_then(|t| self.repo.lookup(&t, p)) {
            Err(e) => return http_err(500, e),
            Ok(Some(fe)) if fe.mode != Mode::Dir => fe,
            Ok(_) => return http_err(404, format!("no such file {p:?}")),
        };
        match self.repo.get_blob(&fe.id) {
            Ok(data) => raw(&data, vec![("X-Sprig-Mode", fe.mode.as_str().into())]),
            Err(e) => http_err(500, e),
        }
    }

    fn handle_ls(&self, r: &Req) -> Out {
        let id = match self.resolve(r) {
            Ok(id) => id,
            Err(e) => return http_err(404, e),
        };
        let prefix = r.q("path").trim_matches('/');
        if !r.may_read(prefix).1 {
            return http_err(403, format!("this token may not read {prefix:?}"));
        }
        let tree = match self.repo.tree_of(&id).and_then(|t| {
            if prefix.is_empty() { Ok(Some(FileEnt::new(Mode::Dir, t))) } else { self.repo.lookup(&t, prefix) }
        }) {
            Err(e) => return http_err(500, e),
            Ok(Some(fe)) if fe.mode == Mode::Dir => fe.id,
            Ok(_) => return json(200, &serde_json::json!({ "entries": {} })),
        };
        let mut seen = BTreeMap::new();
        if !tree.is_empty() {
            match self.repo.get_tree(&tree) {
                Err(e) => return http_err(500, e),
                Ok(t) => {
                    for e in &t.entries {
                        let full = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
                        if r.may_read(&full).1 {
                            seen.insert(e.name.clone(), e.mode.as_str());
                        }
                    }
                }
            }
        }
        json(200, &serde_json::json!({ "entries": seen }))
    }

    // -----------------------------------------------------------------------
    // Landing

    fn handle_land(&self, r: &Req) -> Out {
        match self.land(r) {
            Ok(out) => out,
            Err(e) => http_err(500, e),
        }
    }

    fn land(&self, r: &Req) -> Result<Out> {
        let Ok(mut u) = serde_json::from_slice::<LandRequest>(&r.body) else { return Ok(http_err(400, "bad json")) };
        let name = format!("heads/{}", u.branch);
        if u.branch.is_empty() || !valid_ref_name(&name) {
            return Ok(http_err(400, "bad branch name"));
        }
        if u.files.is_empty() {
            return Ok(http_err(400, "nothing to land"));
        }
        if u.message.trim().is_empty() {
            return Ok(http_err(400, "a message is required"));
        }
        if !u.base.is_empty() && !is_id(&u.base) {
            return Ok(http_err(400, "bad base id"));
        }
        let view = self.view_of(r);
        if let (Some(v), false) = (&view, u.base.is_empty()) {
            // a projected clone names a projected base
            let v = v.lock().unwrap();
            match v.real(&u.base) {
                Some(real) => u.base = real,
                None if !v.may(&u.base) => return Ok(http_err(400, format!("unknown base change {}", short(&u.base)))),
                None => {}
            }
        }
        let Ok(base_tree) = self.repo.tree_of(&u.base) else {
            return Ok(http_err(400, format!("unknown base change {}", short(&u.base))));
        };
        let scope = r.scope.as_ref().map(|s| s.write.clone());
        // Only the edited paths are looked at, so a landing costs the same in
        // a small repo and a huge one.
        let (mut base, mut ours) = (Flat::new(), Flat::new());
        for (p, f) in &u.files {
            if !valid_path(p) {
                return Ok(http_err(400, format!("bad path {p:?}")));
            }
            if scope.as_ref().is_some_and(|s| !in_scope(s, p)) {
                return Ok(http_err(403, format!("this token may not write {p:?}")));
            }
            if let Some(be) = self.repo.lookup(&base_tree, p)? {
                base.insert(p.clone(), be);
            }
            let Some(f) = f else { continue };
            let Some(mode) = Mode::parse(&f.mode).filter(|m| *m != Mode::Dir) else {
                return Ok(http_err(400, format!("bad mode {:?} for {p:?}", f.mode)));
            };
            ours.insert(p.clone(), FileEnt::new(mode, self.repo.put_blob(&f.data)?));
        }
        let mut sig = None;
        if let Some(s) = &u.sig {
            let proof = LandProof {
                branch: u.branch.clone(),
                base: u.base.clone(),
                files: u
                    .files
                    .iter()
                    .map(|(p, f)| (p.clone(), f.as_ref().map_or(String::new(), |f| hash_object(BLOB, &f.data))))
                    .collect(),
            };
            if let Err(e) = check_sig(s, &crate::sign::land_payload(&proof, &u.message, &u.author, &u.meta)) {
                return Ok(http_err(400, format!("bad signature: {e}")));
            }
            sig = Some(Signature { key: s.key.clone(), sig: s.sig.clone(), land: Some(proof), moved: BTreeMap::new() });
        }
        if u.author.is_empty() {
            if sig.is_some() {
                return Ok(http_err(400, "a signed landing needs an author"));
            }
            u.author = "unknown <unknown@localhost>".into();
        }
        for retries in 0..MAX_LAND_RETRIES {
            let tip = match self.read_ref(&name) {
                Ok(t) => t,
                Err(e) => return Ok(http_err(502, e)),
            };
            let tip_tree = self.repo.tree_of(&tip)?;
            let mut theirs = Flat::new();
            for p in u.files.keys() {
                let te = self.repo.lookup(&tip_tree, p)?;
                if te.as_ref().is_some_and(|e| e.mode == Mode::Dir) || base.get(p).is_some_and(|e| e.mode == Mode::Dir) {
                    return Ok(http_err(422, format!("path {p:?} is a directory")));
                }
                if let Some(te) = te {
                    theirs.insert(p.clone(), te);
                }
            }
            // An edited file someone else renamed since the base: edit it at
            // its new path (the only case that needs whole trees).
            let mut paths: Vec<String> = u.files.keys().cloned().collect();
            let (mut b2, mut o2, mut t2) = (base.clone(), ours.clone(), theirs.clone());
            let mut moved = BTreeMap::new();
            let gone: Vec<&String> =
                paths.iter().filter(|p| base.contains_key(*p) && ours.contains_key(*p) && !theirs.contains_key(*p)).collect();
            if !gone.is_empty() {
                let bf = self.repo.flatten(&base_tree)?;
                let tf = self.repo.flatten(&tip_tree)?;
                let ren = self.repo.renames(&bf, &tf);
                let gone: Vec<String> = gone.into_iter().cloned().collect();
                for p in gone {
                    let Some(q) = ren.get(&p) else { continue };
                    if scope.as_ref().is_some_and(|s| !in_scope(s, q)) {
                        return Ok(http_err(403, format!("{p} was moved to {q}, which this token may not write")));
                    }
                    moved.insert(p.clone(), q.clone());
                    if let Some(e) = b2.remove(&p) {
                        b2.insert(q.clone(), e);
                    }
                    if let Some(e) = o2.remove(&p) {
                        o2.insert(q.clone(), e);
                    }
                    if let Some(te) = tf.get(q) {
                        t2.insert(q.clone(), te.clone());
                    }
                    let i = paths.iter().position(|x| *x == p).unwrap();
                    paths[i] = q.clone();
                }
            }
            let (merged, conflicts) = self.repo.merge_flat(&b2, &o2, &t2, "landing", &u.branch)?;
            if !conflicts.is_empty() {
                let paths: Vec<&str> = conflicts.iter().map(|c| c.path.as_str()).collect();
                return Ok(json(
                    409,
                    &serde_json::json!({
                        "error": format!("conflicts with changes landed since your base: {}", paths.join(", ")),
                        "conflicts": paths, "tip": tip }),
                ));
            }
            let edits: BTreeMap<String, Option<FileEnt>> = paths.iter().map(|p| (p.clone(), merged.get(p).cloned())).collect();
            let tree = match self.repo.update_tree(&tip_tree, &edits) {
                Ok(t) => t,
                Err(e) => return Ok(http_err(422, e)),
            };
            let mut sig = sig.clone();
            if let Some(s) = sig.as_mut() {
                s.moved = moved.clone();
            }
            let c = Change {
                parents: if tip.is_empty() { vec![] } else { vec![tip.clone()] },
                tree: tree.clone(),
                author: u.author.clone(),
                time: util::now_unix(),
                tz: util::tz_now(),
                message: u.message.clone(),
                meta: u.meta.clone(),
                sig,
                ..Default::default()
            };
            let id = self.repo.put_change(&c)?;
            if self.upstream.is_some() {
                if let Err(e) = self.publish(&id, &tree, &edits) {
                    return Ok(http_err(502, e));
                }
            }
            if self.swap_ref(&name, &tip, &id, &u.author)? {
                continue; // someone landed meanwhile: re-merge onto the new tip
            }
            let mut res = LandResult { id: id.clone(), parent: tip.clone(), retries };
            if let Some(v) = &view {
                // answer in the token's own view of history
                let mut v = v.lock().unwrap();
                res.id = v.project(&self.repo, &id)?;
                if !tip.is_empty() {
                    res.parent = v.project(&self.repo, &tip)?;
                }
            }
            self.repo.maybe_auto_gc();
            return Ok(json(200, &res));
        }
        Ok(http_err(503, format!("{} moved {MAX_LAND_RETRIES} times during landing; try again", u.branch)))
    }

    /// Uploads what a landing created (the edited files, the directories
    /// along their paths, the change) to upstream, dependencies first.
    fn publish(&self, id: &str, tree: &str, edits: &BTreeMap<String, Option<FileEnt>>) -> Result<()> {
        let up = self.upstream.as_ref().unwrap();
        let mut blobs = vec![];
        let mut depth: HashMap<String, usize> = HashMap::new();
        let mut trees = vec![];
        for (p, fe) in edits {
            if let Some(fe) = fe {
                blobs.push(fe.id.clone());
            }
            let parts: Vec<&str> = p.split('/').collect();
            let mut cur = tree.to_string();
            for d in 0.. {
                if !depth.contains_key(&cur) {
                    trees.push(cur.clone());
                }
                let e = depth.entry(cur.clone()).or_default();
                *e = (*e).max(d);
                if d == parts.len() - 1 {
                    break;
                }
                match self.repo.lookup(tree, &parts[..=d].join("/"))? {
                    Some(fe) if fe.mode == Mode::Dir => cur = fe.id,
                    _ => break,
                }
            }
        }
        trees.sort_by_key(|t| std::cmp::Reverse(depth[t]));
        let order: Vec<String> = blobs.into_iter().chain(trees).chain([id.to_string()]).collect();
        let mut need: std::collections::HashSet<String> = up.missing(&order)?.into_iter().collect();
        let send: Vec<String> = order.into_iter().filter(|o| need.remove(o)).collect();
        up.put_objects(&send, |id| self.repo.get_object(id))
    }
}

#[derive(Serialize, Deserialize)]
pub struct LogItem {
    pub id: String,
    pub change: Change,
}

/// Parses tokens.json: {"<token>": ["dir", ...] or {"write": [...], "read": [...]}}.
pub fn parse_scopes(b: &[u8], file: &str) -> Result<BTreeMap<String, Scope>> {
    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_slice(b).map_err(|e| err!("{file}: {e}"))?;
    let mut out = BTreeMap::new();
    for (tok, v) in raw {
        if tok.len() < 16 {
            bail!("{file}: tokens must be at least 16 characters");
        }
        let mut sc = Scope::from_json(&v).map_err(|e| err!("{file}: {e}"))?;
        for list in std::iter::once(&mut sc.write).chain(sc.read.as_mut()) {
            for pre in list.iter_mut() {
                let p = pre.trim_matches('/').to_string();
                if !valid_path(&p) {
                    bail!("{file}: bad path {pre:?}");
                }
                *pre = p;
            }
        }
        out.insert(tok, sc);
    }
    Ok(out)
}

pub fn is_loopback(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

/// Serves until the process ends.
pub fn serve(s: Arc<Server>, addr: &str, out: &mut dyn std::io::Write) -> Result<()> {
    if !is_loopback(addr) && s.token.is_empty() && s.scopes.is_empty() {
        bail!("refusing to listen on a non-loopback address without --token (or SPRIG_TOKEN) or --tokens");
    }
    let mut what = s.repo.dir.display().to_string();
    if let Some(up) = &s.upstream {
        what = format!("cache of {}", up.urls.join(", "));
    }
    if let Some(f) = &s.follow {
        what += &format!(" (read-only standby of {})", f.urls.join(", "));
    }
    s.start(addr)?;
    writeln!(out, "serving {what} on http://{addr}")?;
    out.flush()?;
    loop {
        std::thread::park();
    }
}
