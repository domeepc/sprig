//! `sprig watch` keeps a list of paths that changed, so status and commit
//! can re-check only those instead of stat-ing every file. The index scan
//! asks it over a unix socket in .sprig; with no watcher running, nothing
//! changes.
//!
//! Protocol: the client sends "since <ns>\n", the time its index was saved.
//! The answer is "unknown\n" if the watcher cannot vouch for that whole
//! period, or "ok\n" followed by one changed path per line and an empty
//! line.

use crate::store::Repo;
use crate::util::Result;

/// Covers edits in the same timestamp tick as an index save.
#[cfg(unix)]
const WATCH_SLACK: i64 = 1_000_000_000;

#[cfg(unix)]
mod imp {
    use super::WATCH_SLACK;
    use crate::store::{hash_object, Repo};
    use crate::util::{now_ns, Result};
    use crate::{bail, err};
    use notify::{EventKind, RecursiveMode, Watcher};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    /// The watcher's socket: in .sprig, or, when that path is too long for a
    /// unix socket, in a private per-user directory under the system temp
    /// dir. That directory must be ours and mode 0700, so nobody else can
    /// plant a socket that answers "nothing changed".
    fn watch_sock(r: &Repo) -> Result<PathBuf> {
        let p = r.dir.join("watch.sock");
        if p.as_os_str().len() <= 100 {
            return Ok(p);
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let dir = std::env::temp_dir().join(format!("sprig-{uid}"));
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e.into());
            }
        }
        let st = std::fs::symlink_metadata(&dir)?;
        if !st.is_dir() || st.permissions().mode() & 0o777 != 0o700 || st.uid() != uid {
            bail!("{} is not a private directory", dir.display());
        }
        let p = dir.join(format!("{}.sock", &hash_object("sock", r.dir.to_string_lossy().as_bytes())[..16]));
        if p.as_os_str().len() > 100 {
            bail!("no usable socket path for {}", r.dir.display());
        }
        Ok(p)
    }

    struct State {
        /// earliest time this watcher can vouch for
        since: i64,
        changed: HashMap<String, i64>,
    }

    fn skipped(name: &std::ffi::OsStr) -> bool {
        name == ".sprig" || name == ".git"
    }

    /// Watches dir and every directory below it.
    fn add_tree(w: &mut impl Watcher, dir: &Path, st: &Mutex<State>) {
        if w.watch(dir, RecursiveMode::NonRecursive).is_err() {
            st.lock().unwrap().since = now_ns(); // can't see this directory: vouch for nothing older
        }
        let Ok(ents) = std::fs::read_dir(dir) else { return };
        for e in ents.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) && !skipped(&e.file_name()) {
                add_tree(w, &e.path(), st);
            }
        }
    }

    pub fn watch(r: &Repo, ready: impl FnOnce()) -> Result<()> {
        if r.bare {
            bail!("bare repository has no working tree");
        }
        let sock = watch_sock(r)?;
        let st = Arc::new(Mutex::new(State { since: now_ns(), changed: HashMap::new() }));
        let (tx, rx) = mpsc::channel();
        let mut w = notify::recommended_watcher(tx).map_err(|e| err!("watch: {e}"))?;
        add_tree(&mut w, &r.root, &st);
        let root = r.root.clone();
        let st2 = st.clone();
        std::thread::spawn(move || {
            for ev in rx {
                let ev: notify::Event = match ev {
                    Ok(ev) if matches!(ev.kind, EventKind::Access(_)) => continue, // reads, such as a status walking a directory
                    Ok(ev) if !ev.need_rescan() => ev,
                    _ => {
                        // e.g. queue overflow: events may be lost
                        let mut s = st2.lock().unwrap();
                        s.changed.clear();
                        s.since = now_ns();
                        continue;
                    }
                };
                for p in &ev.paths {
                    let Ok(rel) = p.strip_prefix(&root) else { continue };
                    if rel.components().next().is_some_and(|c| skipped(c.as_os_str())) {
                        continue;
                    }
                    let now = now_ns();
                    let rel = match rel.to_string_lossy() {
                        r if r.is_empty() => ".".to_string(), // the root itself; an empty line ends the answer
                        r => r.into_owned(),
                    };
                    {
                        let mut s = st2.lock().unwrap();
                        s.changed.insert(rel, now);
                        if s.changed.len() > 200_000 {
                            // keep memory bounded; older queries get "unknown"
                            s.changed.clear();
                            s.since = now;
                        }
                    }
                    if matches!(ev.kind, EventKind::Create(_)) && std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
                        // files created before the watch was added are covered: the client walks this dir
                        add_tree(&mut w, p, &st2);
                    }
                }
            }
        });
        let _ = std::fs::remove_file(&sock);
        let ln = UnixListener::bind(&sock)?;
        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Rm(sock);
        ready();
        for conn in ln.incoming() {
            let Ok(conn) = conn else { continue };
            let st = st.clone();
            std::thread::spawn(move || answer(conn, &st));
        }
        Ok(())
    }

    fn answer(mut conn: UnixStream, st: &Mutex<State>) {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(1)));
        let mut line = String::new();
        if BufReader::new(&conn).read_line(&mut line).is_err() {
            return;
        }
        let Ok(ts) = line.trim().trim_start_matches("since ").parse::<i64>() else { return };
        let mut out = String::new();
        {
            let s = st.lock().unwrap();
            if ts - WATCH_SLACK < s.since {
                out.push_str("unknown\n");
            } else {
                out.push_str("ok\n");
                for (p, &t) in &s.changed {
                    if t >= ts - WATCH_SLACK {
                        out.push_str(p);
                        out.push('\n');
                    }
                }
                out.push('\n');
            }
        }
        let _ = conn.write_all(out.as_bytes());
    }

    /// The paths changed since a time (unix ns), whether a watcher could
    /// vouch for that, and whether one answered at all.
    pub fn ask(r: &Repo, since: i64) -> (Vec<String>, bool, bool) {
        let Ok(sock) = watch_sock(r) else { return (vec![], false, false) };
        let Ok(mut conn) = UnixStream::connect(sock) else { return (vec![], false, false) };
        let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = conn.set_write_timeout(Some(Duration::from_millis(500)));
        if writeln!(conn, "since {since}").is_err() {
            return (vec![], false, false);
        }
        let mut br = BufReader::new(conn);
        let mut l = String::new();
        if br.read_line(&mut l).is_err() || l.is_empty() {
            return (vec![], false, false);
        }
        if l != "ok\n" {
            return (vec![], false, true);
        }
        let mut dirty = vec![];
        loop {
            l.clear();
            match br.read_line(&mut l) {
                Ok(n) if n > 0 && l.ends_with('\n') => {}
                _ => return (vec![], false, true), // cut short: don't trust a partial list
            }
            if l == "\n" {
                return (dirty, true, true);
            }
            match l.trim_end_matches('\n') {
                ".sprigignore" => return (vec![], false, true), // what counts as a file changed: look at everything
                "." => dirty.push(String::new()),                // the root
                p => dirty.push(p.to_string()),
            }
        }
    }
}

/// Serves change queries until the process is stopped.
pub fn watch(r: &Repo, ready: impl FnOnce()) -> Result<()> {
    #[cfg(unix)]
    return imp::watch(r, ready);
    #[cfg(not(unix))]
    {
        let _ = (r, ready);
        crate::bail!("sprig watch needs a unix system")
    }
}

/// The paths changed since a time (unix ns), whether a watcher could vouch
/// for that whole period, and whether one answered at all.
pub fn ask_watcher_since(r: &Repo, since: i64) -> (Vec<String>, bool, bool) {
    let off = std::env::var_os("SPRIG_NO_WATCH").is_some_and(|v| !v.is_empty());
    if r.bare || since == 0 || off {
        return (vec![], false, false);
    }
    #[cfg(unix)]
    return imp::ask(r, since);
    #[cfg(not(unix))]
    (vec![], false, false)
}
