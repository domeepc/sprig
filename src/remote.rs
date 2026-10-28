//! The client side of the server API: a Remote, batched object transfer, and
//! push, fetch and land.

use crate::pack::MAX_OBJECT_SIZE;
use crate::sign::{self, LandProof, Signature};
use crate::store::{Meta, Repo, TREE, children, hash_object, obj_type, short};
use crate::tree::{Mode, apply_deltas, parse_tree};
use crate::util::{self, Error, Result};
use crate::{bail, err};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

// Batched object transfer: many objects per HTTP request instead of one.
// The wire format is a stream of records, each "<id> <type> <len>\n"
// followed by exactly <len> bytes of object data.

/// ids per request
pub const BATCH_IDS: usize = 2000;
/// soft cap on one response or upload
pub const BATCH_BYTES: usize = 64 << 20;

pub fn write_record(w: &mut Vec<u8>, id: &str, typ: &str, data: &[u8]) {
    w.extend_from_slice(format!("{id} {typ} {}\n", data.len()).as_bytes());
    w.extend_from_slice(data);
}

/// Parses a record stream, verifying every object's hash.
pub fn read_records(mut b: &[u8], mut f: impl FnMut(&str, &'static str, Vec<u8>) -> Result<()>) -> Result<()> {
    while !b.is_empty() {
        let Some(nl) = b.iter().position(|&c| c == b'\n') else { bail!("truncated object stream") };
        let line = String::from_utf8_lossy(&b[..nl]).into_owned();
        b = &b[nl + 1..];
        let f3: Vec<&str> = line.split_whitespace().collect();
        if f3.len() != 3 || !crate::store::is_id(f3[0]) {
            bail!("bad record header {:?}", line.trim());
        }
        let n: usize = match f3[2].parse() {
            Ok(n) if n <= MAX_OBJECT_SIZE => n,
            _ => bail!("bad record size {:?}", f3[2]),
        };
        if b.len() < n {
            bail!("truncated object stream");
        }
        let (data, rest) = b.split_at(n);
        b = rest;
        match obj_type(f3[1]) {
            Some(typ) if hash_object(typ, data) == f3[0] => f(f3[0], typ, data.to_vec())?,
            _ => bail!("object {} corrupt: hash mismatch", short(f3[0])),
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LandFile {
    /// file | exec | link
    #[serde(default)]
    pub mode: String,
    #[serde(default, with = "b64")]
    pub data: Vec<u8>,
}

mod b64 {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = Option::<String>::deserialize(d)?.unwrap_or_default();
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LandRequest {
    #[serde(default)]
    pub branch: String,
    /// change the edits were made against ("" = empty tree)
    #[serde(default)]
    pub base: String,
    /// null deletes the path
    #[serde(default, deserialize_with = "util::nullable")]
    pub files: BTreeMap<String, Option<LandFile>>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub meta: Meta,
    /// over land_payload; see sign.rs
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<Signature>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct LandResult {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub retries: usize,
}

#[derive(Serialize, Deserialize, Default)]
pub struct RefUpdate {
    #[serde(default)]
    pub old: String,
    #[serde(default)]
    pub new: String,
    #[serde(default)]
    pub force: bool,
}

pub struct Remote {
    pub name: String,
    pub token: String,
    /// all servers to try, in order (a comma-separated URL)
    pub urls: Vec<String>,
    /// index into urls of the one in use
    cur: AtomicUsize,
    agent: ureq::Agent,
}

/// A response: body and the headers sprig uses.
#[derive(Debug)]
pub struct Resp {
    pub data: Vec<u8>,
    pub typ: String,
    pub mode: String,
    pub content_type: String,
    pub status: u16,
}

impl Remote {
    /// A client for one server, or for several given as a comma-separated
    /// list: requests go to the first that answers.
    pub fn new(name: &str, u: &str) -> Remote {
        let mut urls: Vec<String> =
            u.split(',').map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()).collect();
        if urls.is_empty() {
            urls.push(String::new());
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .http_status_as_error(false)
            .build()
            .into();
        Remote {
            name: name.to_string(),
            token: std::env::var("SPRIG_TOKEN").unwrap_or_default(),
            urls,
            cur: AtomicUsize::new(0),
            agent,
        }
    }

    /// The server to use now.
    pub fn base(&self) -> &str {
        &self.urls[self.cur.load(Ordering::Relaxed) % self.urls.len()]
    }

    /// Moves on to the next server after one did not answer.
    pub fn failed_over(&self, from: &str) {
        if self.urls.len() > 1 && self.base() == from {
            self.cur.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Sends a request and returns the response whatever its status.
    pub fn send(&self, method: &str, path: &str, body: &[u8], hdr: &[(&str, &str)]) -> Result<Resp> {
        let mut last = None;
        for _ in 0..self.urls.len() {
            let base = self.base().to_string();
            let mut req = ureq::http::Request::builder().method(method).uri(format!("{base}{path}"));
            if !self.token.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", self.token));
            }
            for (k, v) in hdr {
                req = req.header(*k, *v);
            }
            let req = req.body(body.to_vec()).map_err(|e| err!("{e}"))?;
            match self.agent.run(req) {
                Ok(mut resp) => {
                    let h = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    let (typ, mode, content_type) = (h("X-Sprig-Type"), h("X-Sprig-Mode"), h("Content-Type"));
                    let status = resp.status().as_u16();
                    let data = resp
                        .body_mut()
                        .with_config()
                        .limit((MAX_OBJECT_SIZE + BATCH_BYTES) as u64)
                        .read_to_vec()
                        .map_err(|e| err!("{e}"))?;
                    return Ok(Resp { data, typ, mode, content_type, status });
                }
                Err(e) => {
                    last = Some(e);
                    self.failed_over(&base); // no answer at all: try the next server
                }
            }
        }
        Err(err!("{}", last.map_or("no server".to_string(), |e| e.to_string())))
    }

    /// Sends a request; an error status becomes an Error::Remote.
    pub fn call(&self, method: &str, path: &str, body: &[u8], hdr: &[(&str, &str)]) -> Result<Resp> {
        let r = self.send(method, path, body, hdr)?;
        if r.content_type.starts_with("text/html") {
            // sprig servers only send JSON or raw bytes
            return Err(Error::Remote {
                code: r.status,
                msg: format!(
                    "{} is not a sprig server; to publish to GitHub use `sprig export-git` and `git push`",
                    self.base()
                ),
            });
        }
        if r.status >= 300 {
            #[derive(Deserialize)]
            struct E {
                #[serde(default)]
                error: String,
            }
            let msg = serde_json::from_slice::<E>(&r.data)
                .map(|e| e.error)
                .ok()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| String::from_utf8_lossy(&r.data).trim().to_string());
            return Err(Error::Remote { code: r.status, msg });
        }
        Ok(r)
    }

    fn json<T: serde::de::DeserializeOwned>(&self, method: &str, path: &str, body: &[u8]) -> Result<T> {
        Ok(serde_json::from_slice(&self.call(method, path, body, &[])?.data)?)
    }

    pub fn refs(&self) -> Result<BTreeMap<String, String>> {
        Ok(self.wait_refs("")?.0)
    }

    /// The remote's refs once they differ from etag ("" = now).
    pub fn wait_refs(&self, etag: &str) -> Result<(BTreeMap<String, String>, String)> {
        #[derive(Deserialize)]
        struct R {
            #[serde(default, deserialize_with = "util::nullable")]
            refs: BTreeMap<String, String>,
            #[serde(default)]
            etag: String,
        }
        let path = if etag.is_empty() { "/v1/refs".to_string() } else { format!("/v1/refs?wait={etag}") };
        let r: R = self.json("GET", &path, &[])?;
        Ok((r.refs, r.etag))
    }

    pub fn get_object(&self, id: &str) -> Result<(&'static str, Vec<u8>)> {
        let r = self.call("GET", &format!("/v1/object/{id}"), &[], &[])?;
        match obj_type(&r.typ) {
            Some(typ) if hash_object(typ, &r.data) == id => Ok((typ, r.data)),
            _ => bail!("remote sent corrupt object {}", short(id)),
        }
    }

    pub fn missing(&self, ids: &[String]) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct M {
            #[serde(default, deserialize_with = "util::nullable")]
            missing: Vec<String>,
        }
        let mut out = vec![];
        for chunk in ids.chunks(5000) {
            let body = util::to_json(&serde_json::json!({ "ids": chunk }));
            out.extend(self.json::<M>("POST", "/v1/objects/missing", &body)?.missing);
        }
        Ok(out)
    }

    pub fn put_object(&self, id: &str, typ: &str, data: &[u8]) -> Result<()> {
        self.call("PUT", &format!("/v1/object/{id}"), data, &[("X-Sprig-Type", typ)]).map(|_| ())
    }

    pub fn update_ref(&self, name: &str, old: &str, new: &str, force: bool) -> Result<()> {
        let body = util::to_json(&RefUpdate { old: old.into(), new: new.into(), force });
        self.call("POST", &format!("/v1/refs/{name}"), &body, &[]).map(|_| ())
    }

    pub fn land(&self, lr: &LandRequest) -> Result<LandResult> {
        self.json("POST", "/v1/land", &util::to_json(lr))
    }

    /// Downloads ids in batches, calling f for each (hash-verified).
    pub fn get_objects(
        &self,
        ids: &[String],
        mut f: impl FnMut(&str, &'static str, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        let mut ids = ids.to_vec();
        while !ids.is_empty() {
            let chunk = &ids[..ids.len().min(BATCH_IDS)];
            let body = util::to_json(&serde_json::json!({ "ids": chunk }));
            let data = self.call("POST", "/v1/objects/get", &body, &[])?.data;
            let mut got = HashSet::new();
            read_records(&data, |id, typ, d| {
                got.insert(id.to_string());
                f(id, typ, d)
            })?;
            if got.is_empty() {
                bail!("remote is missing object {}", short(&chunk[0]));
            }
            ids.retain(|id| !got.contains(id));
        }
        Ok(())
    }

    /// Uploads objects in dependency order, in batches.
    pub fn put_objects(
        &self,
        ids: &[String],
        get: impl Fn(&str) -> Result<(&'static str, std::sync::Arc<[u8]>)>,
    ) -> Result<()> {
        let mut buf = vec![];
        let flush = |buf: &mut Vec<u8>| -> Result<()> {
            if !buf.is_empty() {
                self.call("POST", "/v1/objects/put", buf, &[("Content-Type", "application/octet-stream")])?;
                buf.clear();
            }
            Ok(())
        };
        for id in ids {
            let (typ, data) = get(id)?;
            write_record(&mut buf, id, typ, &data);
            if buf.len() >= BATCH_BYTES {
                flush(&mut buf)?;
            }
        }
        flush(&mut buf)
    }
}

impl Repo {
    pub fn remote(&self, name: &str) -> Result<Remote> {
        let cfg = self.load_config();
        let Some(u) = cfg.remotes.get(name) else {
            bail!("no such remote {name:?} (add one with `sprig remote add`)");
        };
        Ok(Remote::new(name, u))
    }

    /// Every object reachable from id, dependencies first.
    fn closure(&self, id: &str) -> Result<Vec<String>> {
        let lazy = !self.load_config().lazy.is_empty();
        let mut order = vec![];
        let mut seen = HashSet::new();
        let mut stack = vec![(id.to_string(), false)];
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                order.push(id);
                continue;
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            if lazy && !self.has_object(&id) {
                continue; // never downloaded, so it came from the remote, which has it
            }
            let (typ, data) = self.get_object(&id)?;
            let kids = children(typ, &data)?;
            stack.push((id, true));
            for k in kids.into_iter().rev() {
                if !seen.contains(&k) {
                    stack.push((k, false));
                }
            }
        }
        Ok(order)
    }

    pub fn push(&self, rm: &Remote, branch: &str, force: bool) -> Result<String> {
        let Some(local) = self.read_ref(&format!("heads/{branch}")) else {
            bail!("no local branch {branch:?}");
        };
        let old = rm.refs()?.remove(&format!("heads/{branch}")).unwrap_or_default();
        if old == local {
            return Ok("up to date".into());
        }
        let order = self.closure(&local)?;
        let need: HashSet<String> = rm.missing(&order)?.into_iter().collect();
        // dependency order: the server enforces closure
        let send: Vec<String> = order.into_iter().filter(|id| need.contains(id)).collect();
        rm.put_objects(&send, |id| self.get_object(id))?;
        if let Err(e) = rm.update_ref(&format!("heads/{branch}"), &old, &local, force) {
            if matches!(e, Error::Remote { code: 409, .. }) {
                bail!("{e}\nhint: pull and merge first, or push with --force");
            }
            return Err(e);
        }
        self.write_ref(&format!("remotes/{}/{branch}", rm.name), &local)?;
        Ok(format!("pushed {branch} ({} objects sent)", send.len()))
    }

    /// Sends the working tree's edits against HEAD to the remote, which
    /// applies them onto the branch tip. The local branch and working tree
    /// then move to the landed change. HEAD must be a change the remote
    /// already has.
    pub fn land(&self, rm: &Remote, branch: &str, msg: &str, meta: Meta) -> Result<LandResult> {
        let (r, head) = self.head();
        if r != format!("heads/{branch}") {
            bail!("land works on the checked-out branch; run `sprig checkout {branch}` first");
        }
        let st = self.work_status(true)?;
        st.save(self);
        let mut files = BTreeMap::new();
        let mut proof = LandProof { branch: branch.into(), base: head.clone(), files: BTreeMap::new() };
        for d in &st.deltas {
            match &d.new {
                None => {
                    proof.files.insert(d.path.clone(), String::new());
                    files.insert(d.path.clone(), None);
                }
                Some(fe) => {
                    proof.files.insert(d.path.clone(), fe.id.clone());
                    let data = self.get_blob(&fe.id)?.to_vec();
                    files.insert(d.path.clone(), Some(LandFile { mode: fe.mode.as_str().into(), data }));
                }
            }
        }
        if files.is_empty() {
            return Err(Error::NothingToCommit);
        }
        let mut lr = LandRequest {
            branch: branch.into(),
            base: head.clone(),
            files,
            message: msg.into(),
            author: self.author(),
            meta,
            sig: None,
        };
        if let Some(key) = sign::signing_key()? {
            lr.sig = Some(sign::sign(&key, &sign::land_payload(&proof, &lr.message, &lr.author, &lr.meta)));
        }
        let res = rm.land(&lr).map_err(|e| match e {
            Error::Remote { ref msg, .. } if msg.contains("unknown base") => err!(
                "{e}\nhint: HEAD has local changes the remote lacks; `sprig push` them or land from a fetched change"
            ),
            e => e,
        })?;
        self.fetch(rm, branch).map_err(|e| err!("landed {} but fetching it failed: {e}", short(&res.id)))?;
        let cur = apply_deltas(self.flatten(&st.tree)?, &st.deltas);
        self.materialize(&cur, &self.change_flat(&res.id)?)?;
        self.write_ref(&r, &res.id)?;
        Ok(res)
    }

    /// Downloads the closure of the remote branch tip, dependencies first, so
    /// that "object present" always implies "its whole history is present".
    pub fn fetch(&self, rm: &Remote, branch: &str) -> Result<(String, usize)> {
        let Some(tip) = rm.refs()?.remove(&format!("heads/{branch}")) else {
            bail!("remote has no branch {branch:?}");
        };
        let n = self.fetch_tip(rm, &tip)?;
        self.write_ref(&format!("remotes/{}/{branch}", rm.name), &tip)?;
        Ok((tip, n))
    }

    /// Downloads everything reachable from tip that is missing here.
    pub fn fetch_tip(&self, rm: &Remote, tip: &str) -> Result<usize> {
        let lazy = !self.load_config().lazy.is_empty() && !self.bare;
        // Download level by level, one batched request per level, into a
        // staging store; then move objects in, dependencies first, so a
        // partial fetch never looks complete. Only ids are kept in memory.
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd).map_err(|e| err!("random: {e}"))?;
        let stage_dir = self.dir.join(format!(".fetch-{}", util::hex(&rnd)));
        std::fs::create_dir_all(&stage_dir)?;
        struct Rm(std::path::PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Rm(stage_dir.clone());
        let stage = Repo::bare_at(&stage_dir);
        let mut got = HashSet::new();
        let mut kids: HashMap<String, Vec<String>> = HashMap::new();
        let mut level = vec![tip.to_string()];
        while !level.is_empty() {
            let mut want = vec![];
            for id in level.drain(..) {
                if !got.contains(&id) && !self.has_object(&id) {
                    got.insert(id.clone());
                    want.push(id);
                }
            }
            if want.is_empty() {
                break;
            }
            rm.get_objects(&want, |id, typ, data| {
                if !got.contains(id) {
                    bail!("remote sent unrequested object {}", short(id));
                }
                let mut ks = children(typ, &data)?;
                if lazy && typ == TREE {
                    // file contents are fetched when first read
                    ks = parse_tree(&data)?.entries.into_iter().filter(|e| e.mode == Mode::Dir).map(|e| e.id).collect();
                }
                stage.put_object(typ, &data)?;
                level.extend(ks.iter().cloned());
                kids.insert(id.to_string(), ks);
                Ok(())
            })?;
        }
        let mut stored = HashSet::new();
        let mut stack = vec![(tip.to_string(), false)];
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                let to = self.obj_path(&id);
                std::fs::create_dir_all(to.parent().unwrap())?;
                std::fs::rename(stage.obj_path(&id), to)?; // same format, same filesystem
                continue;
            }
            if !got.contains(&id) || !stored.insert(id.clone()) {
                continue; // already present locally
            }
            stack.push((id.clone(), true));
            for k in kids.get(&id).into_iter().flatten().rev() {
                stack.push((k.clone(), false));
            }
        }
        Ok(stored.len())
    }
}
