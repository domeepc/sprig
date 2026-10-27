//! Test helpers: a scratch directory per test, sprig run as a subprocess in
//! it (so tests run in parallel), and servers started in-process.

#![allow(dead_code)]

use sprig::remote::{LandFile, LandRequest, Remote};
use sprig::server::Server;
use sprig::store::Repo;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd).unwrap();
        let p = std::env::temp_dir().join(format!("sprig-test-{}", sprig::util::hex(&rnd)));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A failed command: exit code and output.
#[derive(Debug)]
pub struct Failed {
    pub code: i32,
    pub out: String,
    pub err: String,
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "exit {}: {}{}", self.code, self.err, self.out)
    }
}

/// One test's world: scratch directories, a current directory and the
/// environment sprig runs with.
pub struct Ctx {
    pub tmp: Vec<TempDir>,
    pub cwd: RefCell<PathBuf>,
    pub env: RefCell<BTreeMap<String, String>>,
}

impl Ctx {
    /// A fresh repository in a scratch directory, as Go's newRepo made.
    pub fn new() -> Ctx {
        let c = Ctx::bare();
        c.must(&["init"]);
        c
    }

    /// A scratch directory and environment, no repository.
    pub fn bare() -> Ctx {
        let t = TempDir::new();
        let env = BTreeMap::from(
            [
                ("SPRIG_AUTHOR", "Test <test@example.com>".to_string()),
                ("SPRIG_AGENT", String::new()),
                ("SPRIG_MODEL", String::new()),
                ("SPRIG_KEY", t.path().join("no-key").display().to_string()), // never sign with a real key
                ("SPRIG_NO_AUTOGC", "1".into()),
                ("SPRIG_NO_WATCH", String::new()),
                ("SPRIG_TOKEN", String::new()),
                ("HOME", t.path().display().to_string()),
            ]
            .map(|(k, v)| (k.to_string(), v)),
        );
        let work = t.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        Ctx { tmp: vec![t], cwd: RefCell::new(work), env: RefCell::new(env) }
    }

    /// Another scratch directory, removed with the context.
    pub fn temp(&mut self) -> PathBuf {
        let t = TempDir::new();
        let p = t.path().to_path_buf();
        self.tmp.push(t);
        p
    }

    pub fn dir(&self) -> PathBuf {
        self.cwd.borrow().clone()
    }

    pub fn cd(&self, p: impl AsRef<Path>) {
        let p = self.dir().join(p);
        std::fs::create_dir_all(&p).unwrap();
        *self.cwd.borrow_mut() = p;
    }

    pub fn setenv(&self, k: &str, v: &str) {
        self.env.borrow_mut().insert(k.into(), v.into());
    }

    pub fn sp(&self, args: &[&str]) -> Result<String, Failed> {
        let out = Command::new(env!("CARGO_BIN_EXE_sprig"))
            .args(args)
            .current_dir(self.dir())
            .envs(self.env.borrow().iter())
            .output()
            .unwrap();
        let (o, e) = (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
        if out.status.success() {
            Ok(o)
        } else {
            Err(Failed { code: out.status.code().unwrap_or(-1), out: o, err: e })
        }
    }

    #[track_caller]
    pub fn must(&self, args: &[&str]) -> String {
        match self.sp(args) {
            Ok(o) => o,
            Err(f) => panic!("sprig {}: {f}", args.join(" ")),
        }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir().join(rel)
    }

    pub fn write(&self, rel: &str, content: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[track_caller]
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    }

    pub fn exists(&self, rel: &str) -> bool {
        std::fs::symlink_metadata(self.path(rel)).is_ok()
    }

    pub fn repo(&self) -> Repo {
        let mut d = self.dir();
        loop {
            if let Ok(r) = Repo::open(&d) {
                return r;
            }
            assert!(d.pop(), "no repository");
        }
    }

    pub fn head(&self) -> String {
        self.repo().head().1
    }

    /// A remote client with this context's token.
    pub fn remote(&self, url: &str) -> Remote {
        let mut rm = Remote::new("t", url);
        rm.token = self.env.borrow()["SPRIG_TOKEN"].clone();
        rm
    }
}

/// Starts a server over repo; returns it and its URL.
pub fn start(s: Server) -> (Arc<Server>, String) {
    let s = Arc::new(s);
    let url = s.start("127.0.0.1:0").unwrap();
    (s, url)
}

pub fn bare_repo(dir: &Path) -> Repo {
    Repo::init(dir, true, "main").unwrap()
}

/// Seeds a server over a bare repo with a.txt and d/x.txt on main, from a
/// fresh working copy in ctx (which stays in that working copy).
pub fn seed(c: &Ctx, url: &str) {
    c.must(&["init"]);
    c.must(&["remote", "add", "origin", url]);
    c.write("a.txt", "a\n");
    c.write("d/x.txt", "x\n");
    c.must(&["commit", "-m", "seed"]);
    c.must(&["push"]);
}

pub fn file(data: &[u8]) -> Option<LandFile> {
    Some(LandFile { mode: "file".into(), data: data.to_vec() })
}

/// Lands files on main through url, based on its current tip.
pub fn land(url: &str, token: &str, files: Vec<(&str, Option<LandFile>)>) -> sprig::util::Result<()> {
    let mut rm = Remote::new("t", url);
    rm.token = token.into();
    let base = rm.refs()?.remove("heads/main").unwrap_or_default();
    let lr = LandRequest {
        branch: "main".into(),
        base,
        message: "m".into(),
        files: files.into_iter().map(|(p, f)| (p.to_string(), f)).collect(),
        ..Default::default()
    };
    rm.land(&lr).map(|_| ())
}

/// A proxy in front of url that records each request's method and path.
pub fn counting_proxy(url: &str) -> (String, Arc<Mutex<Vec<String>>>) {
    let l = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let me = format!("http://{}", l.server_addr().to_ip().unwrap());
    let seen = Arc::new(Mutex::new(vec![]));
    let (seen2, up) = (seen.clone(), Arc::new(Remote::new("up", url)));
    std::thread::spawn(move || {
        for mut req in l.incoming_requests() {
            let (seen, up) = (seen2.clone(), up.clone());
            std::thread::spawn(move || {
                let path = req.url().to_string();
                seen.lock().unwrap().push(format!("{} {path}", req.method()));
                let mut body = vec![];
                std::io::Read::read_to_end(req.as_reader(), &mut body).unwrap();
                let h = |k: &'static str| req.headers().iter().find(|h| h.field.equiv(k)).map(|h| h.value.to_string());
                let mut hdr = vec![("Authorization", h("Authorization").unwrap_or_default())];
                for k in ["X-Sprig-Type", "Content-Type"] {
                    if let Some(v) = h(k) {
                        hdr.push((k, v));
                    }
                }
                let hdr: Vec<(&str, &str)> = hdr.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let resp = up.send(req.method().as_str(), &path, &body, &hdr).unwrap();
                let mut out = tiny_http::Response::from_data(resp.data).with_status_code(resp.status);
                for (k, v) in [("Content-Type", &resp.content_type), ("X-Sprig-Type", &resp.typ), ("X-Sprig-Mode", &resp.mode)] {
                    if !v.is_empty() {
                        out = out.with_header(tiny_http::Header::from_bytes(k, v.as_bytes()).unwrap());
                    }
                }
                let _ = req.respond(out);
            });
        }
    });
    (me, seen)
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Gina")
        .env("GIT_AUTHOR_EMAIL", "gina@example.com")
        .env("GIT_COMMITTER_NAME", "Gina")
        .env("GIT_COMMITTER_EMAIL", "gina@example.com")
        .env("GIT_AUTHOR_DATE", "2024-03-01T12:00:00+0100")
        .env("GIT_COMMITTER_DATE", "2024-03-01T12:00:00+0100")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn has_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

#[cfg(unix)]
pub fn chmod(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
pub fn is_exec(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

pub fn set_mtime(p: &Path, t: std::time::SystemTime) {
    std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
}

pub fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
