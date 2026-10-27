//! Converting to and from git: import reads objects through one long-lived
//! `git cat-file --batch`, export writes a `git fast-import` stream.

use crate::store::{CHANGE, Change, Repo, TREE, is_id, short, valid_ref_name};
use crate::tree::Mode;
use crate::util::{Result, go_json_str, go_lossy};
use crate::{bail, err};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Runs git in dir and returns its output without trailing newlines.
pub fn git_output(dir: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| err!("cannot run git: {e}"))?;
    if !out.status.success() {
        bail!("git {}: {}: {}", args.join(" "), out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
}

/// A long-lived `git cat-file --batch`, so importing does not fork once per
/// object.
struct GitReader {
    child: Child,
    input: ChildStdin,
    out: BufReader<ChildStdout>,
}

impl GitReader {
    fn new(dir: &str) -> Result<GitReader> {
        let mut child = Command::new("git")
            .args(["-C", dir, "cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| err!("cannot run git: {e}"))?;
        let input = child.stdin.take().unwrap();
        let out = BufReader::with_capacity(1 << 20, child.stdout.take().unwrap());
        Ok(GitReader { child, input, out })
    }

    fn read(&mut self, sha: &str) -> Result<(String, Vec<u8>)> {
        writeln!(self.input, "{sha}")?;
        self.input.flush()?;
        let mut line = String::new();
        self.out.read_line(&mut line)?;
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            [_, "missing"] => bail!("git object {sha} missing"),
            [_, typ, size] => {
                let size: usize = size.parse().map_err(|_| err!("unexpected git output {line:?}"))?;
                let mut data = vec![0; size + 1]; // and the trailing newline
                self.out.read_exact(&mut data)?;
                data.pop();
                Ok((typ.to_string(), data))
            }
            _ => bail!("unexpected git output {line:?}"),
        }
    }
}

impl Drop for GitReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
pub struct ImportResult {
    pub changes: usize,
    pub blobs: usize,
    /// submodule entries
    pub skipped: usize,
    pub branches: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

struct Importer<'a> {
    r: &'a Repo,
    g: GitReader,
    raw_len: usize,
    trees: HashMap<String, String>,   // git tree sha -> sprig tree id
    blobs: HashMap<String, String>,   // git blob sha -> sprig blob id
    changes: HashMap<String, String>, // git commit sha -> sprig change id
    res: ImportResult,
}

impl Repo {
    /// Converts the branches of a git repository into sprig changes. It is
    /// deterministic: re-importing the same repository yields the same ids.
    pub fn import_git(&self, git_dir: &str, force: bool) -> Result<ImportResult> {
        let refs = git_output(git_dir, &["for-each-ref", "--format=%(refname:short) %(objectname)", "refs/heads"])?;
        let branches: Vec<(String, String)> = refs
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f.len() == 2).then(|| (f[0].to_string(), f[1].to_string()))
            })
            .collect();
        if branches.is_empty() {
            bail!("git repository has no branches with commits");
        }
        let raw_len = if git_output(git_dir, &["rev-parse", "--show-object-format"]).is_ok_and(|f| f == "sha256") {
            32
        } else {
            20
        };
        let mut args = vec!["rev-list", "--topo-order", "--reverse", "--parents"];
        args.extend(branches.iter().map(|(_, sha)| sha.as_str()));
        let revs = git_output(git_dir, &args)?;
        let mut imp = Importer {
            r: self,
            g: GitReader::new(git_dir)?,
            raw_len,
            trees: HashMap::new(),
            blobs: HashMap::new(),
            changes: HashMap::new(),
            res: ImportResult::default(),
        };
        self.record("import-git", || {
            let (_, head_before) = self.head();
            for line in revs.lines() {
                let f: Vec<&str> = line.split_whitespace().collect();
                if let Some((sha, parents)) = f.split_first() {
                    imp.import_commit(sha, parents).map_err(|e| err!("commit {}: {e}", &sha[..sha.len().min(10)]))?;
                }
            }
            for (b, sha) in &branches {
                let id = imp.changes[sha].clone();
                let name = format!("heads/{b}");
                if !valid_ref_name(&name) {
                    imp.res.warnings.push(format!("branch {b:?} has a name sprig does not accept; skipped"));
                    continue;
                }
                if let (Some(cur), false) = (self.read_ref(&name), force) {
                    if self.is_ancestor(&id, &cur).unwrap_or(false) {
                        continue; // sprig already has everything git has here
                    }
                    if !self.is_ancestor(&cur, &id).unwrap_or(false) {
                        // diverged: keep git's side as git/<branch> so it can be merged
                        self.write_ref(&format!("remotes/git/{b}"), &id)?;
                        imp.res.warnings.push(format!(
                            "branch {b:?} has diverged; git's version is at git/{b} (run `sprig merge git/{b}`, or --force to overwrite)"
                        ));
                        continue;
                    }
                }
                // Moving the checked-out branch must move the working tree with it.
                let (head_ref, cur) = self.head();
                if head_ref == name && !self.bare && !cur.is_empty() && cur != id {
                    if !self.working_deltas()?.is_empty() {
                        imp.res.warnings.push(format!("branch {b:?} is checked out with uncommitted changes; skipped (commit first)"));
                        continue;
                    }
                    self.materialize(&self.change_flat(&cur)?, &self.change_flat(&id)?)?;
                }
                self.write_ref(&name, &id)?;
                imp.res.branches.insert(b.clone(), id);
            }
            // A fresh repo adopts the imported HEAD branch and checks it out,
            // as long as that cannot overwrite files that are already there.
            if head_before.is_empty() && !self.bare {
                let Ok(head) = git_output(git_dir, &["symbolic-ref", "--short", "HEAD"]) else {
                    return Ok(()); // detached or unborn: leave HEAD alone
                };
                let Some(id) = imp.res.branches.get(&head).cloned() else { return Ok(()) };
                if self.working_deltas()?.iter().any(|d| d.new.is_some()) {
                    imp.res.warnings.push(format!(
                        "working tree is not empty; not checking out {head:?} (run `sprig checkout -f {head}` to do so)"
                    ));
                    return Ok(());
                }
                self.materialize(&BTreeMap::new(), &self.change_flat(&id)?)?;
                self.set_head_ref(&format!("heads/{head}"))?;
            }
            Ok(())
        })?;
        imp.res.changes = imp.changes.len();
        imp.res.blobs = imp.blobs.len();
        Ok(imp.res)
    }
}

impl Importer<'_> {
    // Changes and trees are written as JSON straight from git's bytes, as
    // the Go version wrote them, so re-importing gives the same ids even for
    // text that is not UTF-8.

    fn import_commit(&mut self, sha: &str, parents: &[&str]) -> Result<()> {
        let (typ, data) = self.g.read(sha)?;
        if typ != "commit" {
            bail!("{sha} is a {typ}, not a commit");
        }
        let cut = data.windows(2).position(|w| w == b"\n\n");
        let (head, msg) = match cut {
            Some(i) => (&data[..i], &data[i + 2..]),
            None => (&data[..], &b""[..]),
        };
        let (mut tree_sha, mut author, mut ts, mut tz) = (String::new(), Vec::new(), 0i64, Vec::new());
        for line in head.split(|&c| c == b'\n') {
            if let Some(t) = line.strip_prefix(b"tree ") {
                tree_sha = String::from_utf8_lossy(t).into_owned();
            } else if let Some(rest) = line.strip_prefix(b"author ") {
                let f = go_fields(rest);
                if f.len() >= 3 {
                    author = f[..f.len() - 2].join(&b' ');
                    ts = std::str::from_utf8(f[f.len() - 2]).ok().and_then(|t| t.parse().ok()).unwrap_or(0);
                    tz = f[f.len() - 1].to_vec();
                } else {
                    author = rest.to_vec();
                }
            }
        }
        if tree_sha.is_empty() {
            bail!("commit has no tree");
        }
        let tree = self.import_tree(&tree_sha)?;
        let mut pids = vec![];
        for p in parents {
            pids.push(self.changes.get(*p).cloned().ok_or_else(|| err!("parent {p} not imported yet"))?);
        }
        let msg = &msg[..msg.iter().rposition(|&c| c != b'\n').map_or(0, |i| i + 1)];
        // A commit written by export-git maps back to its change, provided
        // the content and parents really match (the trailer alone is just text).
        if let Some(xid) = sprig_trailer(&go_lossy(msg))
            && self.r.get_change(&xid).is_ok_and(|x| x.tree == tree && x.parents == pids)
        {
            self.changes.insert(sha.to_string(), xid);
            return Ok(());
        }
        let mut j =
            format!("{{\"parents\":{},\"tree\":\"{tree}\",\"author\":", serde_json::to_string(&pids)?).into_bytes();
        go_json_str(&mut j, &author);
        j.extend_from_slice(format!(",\"time\":{ts}").as_bytes());
        if !tz.is_empty() {
            j.extend_from_slice(b",\"tz\":");
            go_json_str(&mut j, &tz);
        }
        j.extend_from_slice(b",\"message\":");
        go_json_str(&mut j, msg);
        j.extend_from_slice(format!(",\"meta\":{{\"git_sha\":\"{sha}\"}}}}").as_bytes());
        let id = self.r.put_object(CHANGE, &j)?;
        self.changes.insert(sha.to_string(), id);
        Ok(())
    }

    fn import_tree(&mut self, sha: &str) -> Result<String> {
        if let Some(id) = self.trees.get(sha) {
            return Ok(id.clone());
        }
        let (typ, data) = self.g.read(sha)?;
        if typ != "tree" {
            bail!("{sha} is a {typ}, not a tree");
        }
        let mut entries: Vec<(Vec<u8>, Mode, String)> = vec![];
        let mut rest = &data[..];
        while !rest.is_empty() {
            let sp = rest.iter().position(|&c| c == b' ');
            let nul = rest.iter().position(|&c| c == 0);
            let (Some(sp), Some(nul)) = (sp, nul) else { bail!("malformed tree") };
            if nul < sp || rest.len() < nul + 1 + self.raw_len {
                bail!("malformed tree");
            }
            let (mode, name) = (&rest[..sp], rest[sp + 1..nul].to_vec());
            let child = crate::util::hex(&rest[nul + 1..nul + 1 + self.raw_len]);
            rest = &rest[nul + 1 + self.raw_len..];
            if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0) {
                self.res.warnings.push(format!("skipped entry with unsupported name {:?}", go_lossy(&name)));
                continue;
            }
            match mode {
                b"40000" | b"040000" => {
                    let id = self.import_tree(&child)?;
                    entries.push((name, Mode::Dir, id));
                }
                b"100644" | b"100664" | b"100755" | b"120000" => {
                    let id = self.import_blob(&child)?;
                    let m = match mode {
                        b"100755" => Mode::Exec,
                        b"120000" => Mode::Link,
                        _ => Mode::File,
                    };
                    entries.push((name, m, id));
                }
                _ => self.res.skipped += 1, // 160000 gitlink (submodule): not representable
            }
        }
        entries.sort();
        let mut j = b"{\"entries\":[".to_vec();
        for (i, (name, mode, id)) in entries.iter().enumerate() {
            j.extend_from_slice(if i == 0 { b"{\"name\":" } else { b",{\"name\":" });
            go_json_str(&mut j, name);
            j.extend_from_slice(format!(",\"mode\":\"{}\",\"id\":\"{id}\"}}", mode.as_str()).as_bytes());
        }
        j.extend_from_slice(b"]}");
        let id = self.r.put_object(TREE, &j)?;
        self.trees.insert(sha.to_string(), id.clone());
        Ok(id)
    }

    fn import_blob(&mut self, sha: &str) -> Result<String> {
        if let Some(id) = self.blobs.get(sha) {
            return Ok(id.clone());
        }
        let (typ, data) = self.g.read(sha)?;
        if typ != "blob" {
            bail!("{sha} is a {typ}, not a blob");
        }
        let id = self.r.put_blob(&data)?;
        self.blobs.insert(sha.to_string(), id.clone());
        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// Export

/// The scratch ref fast-import builds commits on; branches are pointed at
/// the results at the end and the scratch ref is deleted.
pub const EXPORT_REF: &str = "refs/sprig-export";

#[derive(Default)]
pub struct ExportResult {
    /// new git commits written
    pub commits: usize,
    /// branch -> git commit sha
    pub branches: BTreeMap<String, String>,
    pub warnings: Vec<String>,
}

struct Exporter<'a, W: Write> {
    r: &'a Repo,
    dir: &'a str,
    w: W,
    next: usize,
    blobs: HashMap<String, usize>,    // sprig blob id -> fast-import mark
    commits: HashMap<String, String>, // sprig change id -> git sha or ":mark"
    known: BTreeMap<String, String>,  // sprig change id -> git sha from earlier exports
    marks: HashMap<usize, String>,    // fast-import mark -> sprig change id
    written: usize,
}

impl Repo {
    /// The commit an exported change became is the same in every git repo
    /// (export is deterministic), so one map per sprig repo lets later
    /// exports stop there.
    fn git_map_path(&self) -> std::path::PathBuf {
        self.dir.join("git-exported.json")
    }

    /// Writes every sprig branch into a git repository via `git
    /// fast-import`, so it can be pushed with git. Changes that came from git
    /// reuse their original commit; the rest become commits with a
    /// Sprig-Change trailer, which import-git maps back. Output is
    /// deterministic, so exporting twice yields the same commits. Branch
    /// updates must fast-forward unless force.
    pub fn export_git(&self, git_dir: &str, force: bool) -> Result<ExportResult> {
        let mut res = ExportResult::default();
        git_output(git_dir, &["rev-parse", "--git-dir"])?;
        let refs = self.list_refs();
        let names: Vec<String> = refs.keys().filter_map(|n| n.strip_prefix("heads/")).map(String::from).collect();
        if names.is_empty() {
            bail!("nothing to export: no branches");
        }
        let _ = git_output(git_dir, &["update-ref", "-d", EXPORT_REF]); // leftover from a failed run
        let known: BTreeMap<String, String> =
            std::fs::read(self.git_map_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut rnd = [0u8; 8];
        getrandom::fill(&mut rnd).map_err(|e| err!("random: {e}"))?;
        let marks_file = std::env::temp_dir().join(format!("sprig-marks-{}", crate::util::hex(&rnd)));
        struct Rm(std::path::PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Rm(marks_file.clone());
        let mut cmd = Command::new("git");
        cmd.args(["-C", git_dir, "fast-import", "--quiet"]).arg(format!("--export-marks={}", marks_file.display()));
        if force {
            cmd.arg("--force");
        }
        let mut child =
            cmd.stdin(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| err!("cannot run git: {e}"))?;
        let mut ex = Exporter {
            r: self,
            dir: git_dir,
            w: BufWriter::with_capacity(1 << 20, child.stdin.take().unwrap()),
            next: 0,
            blobs: HashMap::new(),
            commits: HashMap::new(),
            known: known.clone(),
            marks: HashMap::new(),
            written: 0,
        };
        let mut exported = vec![];
        let stream = (|| -> Result<()> {
            for b in &names {
                let id = &refs[&format!("heads/{b}")];
                let c = self.get_change(id)?;
                if !c.conflicts.is_empty() && !force {
                    res.warnings.push(format!(
                        "branch {b:?} has {} unresolved conflict(s); skipped (use --force to export anyway)",
                        c.conflicts.len()
                    ));
                    continue;
                }
                let r = ex.export(id).map_err(|e| err!("change {}: {e}", short(id)))?;
                write!(ex.w, "reset refs/heads/{b}\nfrom {r}\n\n")?;
                exported.push(b.clone());
            }
            ex.w.flush()?;
            Ok(())
        })();
        let (marks, written) = (std::mem::take(&mut ex.marks), ex.written);
        drop(ex); // closes fast-import's input
        let out = child.wait_with_output()?;
        let _ = git_output(git_dir, &["update-ref", "-d", EXPORT_REF]);
        if !out.status.success() {
            bail!("git fast-import: {}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
        }
        stream?;
        res.commits = written;
        if let Ok(b) = std::fs::read_to_string(&marks_file) {
            let mut known = known;
            for line in b.lines() {
                let Some((m, sha)) = line.strip_prefix(':').and_then(|l| l.split_once(' ')) else { continue };
                if let Some(id) = m.parse().ok().and_then(|m: usize| marks.get(&m)) {
                    known.insert(id.clone(), sha.to_string());
                }
            }
            let _ = crate::store::write_file_atomic(&self.git_map_path(), &crate::util::to_json(&known), false);
        }
        for b in exported {
            res.branches.insert(b.clone(), git_output(git_dir, &["rev-parse", &format!("refs/heads/{b}")])?);
        }
        Ok(res)
    }
}

impl<W: Write> Exporter<'_, W> {
    fn mark(&mut self) -> usize {
        self.next += 1;
        self.next
    }

    /// Writes change tip (and any ancestors git lacks) to the stream and
    /// returns how to refer to it in a later `from`/`merge`.
    fn export(&mut self, tip: &str) -> Result<String> {
        // parents before children, without recursing down long histories
        let mut stack = vec![(tip.to_string(), false)];
        while let Some((id, ready)) = stack.pop() {
            if self.commits.contains_key(&id) {
                continue;
            }
            let c = self.r.get_change(&id)?;
            if ready {
                self.write_commit(&id, &c)?;
                continue;
            }
            let sha = if c.meta.git_sha.is_empty() {
                self.known.get(&id).cloned().unwrap_or_default()
            } else {
                c.meta.git_sha.clone()
            };
            let exists = !sha.is_empty()
                && Command::new("git")
                    .args(["-C", self.dir, "cat-file", "-e", &format!("{sha}^{{commit}}")])
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
            if exists {
                self.commits.insert(id, sha);
                continue;
            }
            stack.push((id, true));
            for p in c.parents.iter().rev() {
                stack.push((p.clone(), false));
            }
        }
        Ok(self.commits[tip].clone())
    }

    fn write_commit(&mut self, id: &str, c: &Change) -> Result<()> {
        let flat = self.r.flatten(&c.tree)?;
        for fe in flat.values() {
            if self.blobs.contains_key(&fe.id) {
                continue;
            }
            let data = self.r.get_blob(&fe.id)?;
            let m = self.mark();
            self.blobs.insert(fe.id.clone(), m);
            write!(self.w, "blob\nmark :{m}\ndata {}\n", data.len())?;
            self.w.write_all(&data)?;
            self.w.write_all(b"\n")?;
        }
        let m = self.mark();
        if c.parents.is_empty() {
            writeln!(self.w, "reset {EXPORT_REF}")?; // otherwise fast-import continues from the ref's tip
        }
        let tz = if c.tz.is_empty() { "+0000" } else { &c.tz };
        let sig = format!("{} {} {tz}", git_ident(&c.author), c.time);
        let msg = export_message(id, c);
        write!(self.w, "commit {EXPORT_REF}\nmark :{m}\nauthor {sig}\ncommitter {sig}\ndata {}\n{msg}\n", msg.len())?;
        for (i, p) in c.parents.iter().enumerate() {
            writeln!(self.w, "{} {}", if i == 0 { "from" } else { "merge" }, self.commits[p])?;
        }
        self.w.write_all(b"deleteall\n")?;
        for (p, fe) in &flat {
            let mode = match fe.mode {
                Mode::Exec => "100755",
                Mode::Link => "120000",
                _ => "100644",
            };
            writeln!(self.w, "M {mode} :{} {}", self.blobs[&fe.id], fast_import_path(p))?;
        }
        self.w.write_all(b"\n")?;
        self.commits.insert(id.to_string(), format!(":{m}"));
        self.marks.insert(m, id.to_string());
        self.written += 1;
        Ok(())
    }
}

/// "Name <email>" out of a sprig author string.
fn git_ident(a: &str) -> String {
    if a.contains('<') && a.ends_with('>') {
        return a.to_string();
    }
    format!("{} <>", a.replace(['<', '>'], ""))
}

/// The change message plus its provenance as git trailers.
fn export_message(id: &str, c: &Change) -> String {
    let one_line = |s: &str| s.replace('\n', " ");
    let mut b = format!("{}\n\n", c.message.trim_end_matches('\n'));
    for (k, v) in [("Intent", &c.meta.intent), ("Agent", &c.meta.agent), ("Model", &c.meta.model)] {
        if !v.is_empty() {
            b += &format!("Sprig-{k}: {}\n", one_line(v));
        }
    }
    for (k, v) in &c.meta.checks {
        b += &format!("Sprig-Check: {}={}\n", one_line(k), one_line(v));
    }
    b + &format!("Sprig-Change: {id}\n")
}

/// Go's strings.Fields: the text split around runs of white space.
fn go_fields(b: &[u8]) -> Vec<&[u8]> {
    let mut out = vec![];
    let (mut i, mut start) = (0, None);
    while i < b.len() {
        let (space, n) = match std::str::from_utf8(&b[i..(i + 4).min(b.len())])
            .or_else(|e| std::str::from_utf8(&b[i..i + e.valid_up_to()]))
        {
            Ok(s) if !s.is_empty() => {
                let c = s.chars().next().unwrap();
                (c.is_whitespace(), c.len_utf8())
            }
            _ => (false, 1), // not UTF-8: never space
        };
        match (space, start) {
            (true, Some(s)) => {
                out.push(&b[s..i]);
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
        i += n;
    }
    if let Some(s) = start {
        out.push(&b[s..]);
    }
    out
}

/// The Sprig-Change id from an exported commit message.
fn sprig_trailer(msg: &str) -> Option<String> {
    let last = msg.trim_end_matches('\n').rsplit('\n').next()?;
    last.strip_prefix("Sprig-Change: ").filter(|id| is_id(id)).map(String::from)
}

fn fast_import_path(p: &str) -> String {
    if p.starts_with('"') || p.contains('\n') {
        return format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"));
    }
    p.to_string()
}
