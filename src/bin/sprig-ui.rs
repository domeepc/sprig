//! sprig-ui: a desktop window on a sprig repository (Tauri). The page in
//! ui/web draws everything; these commands only read the repository and run
//! sprig commands, so every action takes the same code path as the CLI and
//! lands in the operation log (`sprig undo` undoes it).
//!
//!     sprig-ui [repository]

use serde::Serialize;
use sprig::store::{Change, Repo, short};
use sprig::tree::{Delta, RENAME_THRESHOLD};
use sprig::util::{Error, fmt_date};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

type Res<T> = Result<T, String>;

fn repo() -> Res<Repo> {
    Repo::find().map_err(|e| e.to_string())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// What the page is sent

#[derive(Serialize)]
struct FileChange {
    kind: char,
    path: String,
    from: String,
    add: usize,
    del: usize,
}

#[derive(Serialize)]
struct Row {
    id: String,
    short: String,
    title: String,
    body: String,
    author: String,
    agent: String,
    model: String,
    intent: String,
    checks: BTreeMap<String, String>,
    time: i64,
    date: String,
    parents: Vec<String>,
    conflicts: usize,
    signed: bool,
}

impl Row {
    fn new(id: &str, c: &Change) -> Row {
        let mut lines = c.message.trim().lines();
        Row {
            id: id.into(),
            short: short(id).into(),
            title: lines.next().unwrap_or("").into(),
            body: lines.collect::<Vec<_>>().join("\n").trim().into(),
            author: c.author.split(" <").next().unwrap_or("").into(),
            agent: c.meta.agent.clone(),
            model: c.meta.model.clone(),
            intent: c.meta.intent.clone(),
            checks: c.meta.checks.clone(),
            time: c.time,
            date: fmt_date(c.time),
            parents: c.parents.iter().map(|p| short(p).to_string()).collect(),
            conflicts: c.conflicts.len(),
            signed: c.sig.is_some(),
        }
    }
}

/// A file HEAD's merge left to resolve.
#[derive(Serialize)]
struct ConflictRow {
    path: String,
    /// what each side did to it: added, deleted, modified or unchanged
    ours: &'static str,
    theirs: &'static str,
    /// the working file no longer has conflict markers
    resolved: bool,
}

fn side(base: &str, id: &str) -> &'static str {
    match (base.is_empty(), id.is_empty()) {
        (true, false) => "added",
        (false, true) => "deleted",
        _ if base == id => "unchanged",
        _ => "modified",
    }
}

#[derive(Serialize)]
struct State {
    name: String,
    root: String,
    branch: String,
    head: String,
    branches: Vec<String>,
    remotes: Vec<String>,
    github: String,
    /// commits on the branch GitHub lacks, and the other way round, as of the last pull
    ahead_behind: Option<(usize, usize)>,
    changes: Vec<FileChange>,
    conflicts: Vec<ConflictRow>,
    history: Vec<Row>,
    /// branches that share no history with HEAD: switching swaps the whole tree
    unrelated: Vec<String>,
    now: i64,
}

/// One diff line: old and new numbers, ' ' '+' or '-', text.
#[derive(Serialize)]
struct Line(Option<u32>, Option<u32>, char, String);

#[derive(Serialize, Default)]
struct Hunk {
    header: String,
    lines: Vec<Line>,
}

#[derive(Serialize, Default)]
struct DiffFile {
    path: String,
    notes: Vec<String>,
    add: usize,
    del: usize,
    hunks: Vec<Hunk>,
}

/// Parses `sprig diff` output into files and hunks.
fn parse_diff(text: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = vec![];
    let mut notes = vec![];
    let (mut old, mut new) = (0u32, 0u32);
    let mut lines = text.lines().take(50_000);
    while let Some(line) = lines.next() {
        if let Some(p) = line.strip_prefix("--- a/") {
            lines.next(); // +++ b/…
            files.push(DiffFile { path: p.into(), notes: std::mem::take(&mut notes), ..Default::default() });
            continue;
        }
        if let Some(h) = line.strip_prefix("@@ ") {
            let num = |s: &str| s.get(1..).and_then(|s| s.split(',').next()?.parse().ok()).unwrap_or(1u32);
            let mut parts = h.split(' ');
            old = num(parts.next().unwrap_or(""));
            new = num(parts.next().unwrap_or(""));
            if let Some(f) = files.last_mut() {
                f.hunks.push(Hunk { header: line.into(), lines: vec![] });
            }
            continue;
        }
        let (Some(f), Some(&op @ (b'+' | b'-' | b' '))) = (files.last_mut(), line.as_bytes().first()) else {
            if !line.is_empty() && !line.starts_with('\\') {
                notes.push(line.to_string());
            }
            continue;
        };
        let Some(h) = f.hunks.last_mut() else { continue };
        let t = line[1..].replace('\t', "    ");
        match op {
            b'+' => {
                f.add += 1;
                h.lines.push(Line(None, Some(new), '+', t));
                new += 1;
            }
            b'-' => {
                f.del += 1;
                h.lines.push(Line(Some(old), None, '-', t));
                old += 1;
            }
            _ => {
                h.lines.push(Line(Some(old), Some(new), ' ', t));
                old += 1;
                new += 1;
            }
        }
    }
    files
}

fn render(r: &Repo, ds: Vec<Delta>, renames: usize, work: Option<&HashSet<String>>) -> Res<Vec<DiffFile>> {
    let mut out = vec![];
    sprig::cli::render_diff(r, &mut out, ds, renames, work).map_err(|e| e.to_string())?;
    Ok(parse_diff(&String::from_utf8_lossy(&out)))
}

fn change_diff(r: &Repo, c: &Change) -> Res<Vec<DiffFile>> {
    let pt = r.tree_of(c.parents.first().map_or("", String::as_str)).map_err(|e| e.to_string())?;
    let ds = r.diff_trees(&pt, &c.tree).map_err(|e| e.to_string())?;
    render(r, ds, RENAME_THRESHOLD, None)
}

// ---------------------------------------------------------------------------
// Commands

#[tauri::command(async)]
fn state() -> Res<State> {
    let r = repo()?;
    let e = |e: Error| e.to_string();
    let (rf, head) = r.head();
    let branch = rf.strip_prefix("heads/").unwrap_or("(detached)").to_string();
    let cfg = r.load_config();
    let ds = r.working_deltas().map_err(e)?;
    let work: HashSet<String> = ds.iter().map(|d| d.path.clone()).collect();
    let ds = r.with_renames_at(ds, RENAME_THRESHOLD, Some(&work));
    let counts: HashMap<String, (usize, usize)> =
        render(&r, ds.clone(), RENAME_THRESHOLD, Some(&work))?.into_iter().map(|f| (f.path, (f.add, f.del))).collect();
    let changes = ds
        .into_iter()
        .map(|d| {
            let (add, del) = counts.get(&d.path).copied().unwrap_or_default();
            FileChange { kind: d.kind, path: d.path, from: d.from, add, del }
        })
        .collect();
    let mut history = vec![];
    let mut conflicts = vec![];
    if !head.is_empty() {
        conflicts = r
            .get_change(&head)
            .map_err(e)?
            .conflicts
            .into_iter()
            .map(|c| {
                let resolved = std::fs::read(r.abs(&c.path)).is_ok_and(|b| !sprig::tree::has_markers(&b));
                ConflictRow { ours: side(&c.base, &c.ours), theirs: side(&c.base, &c.theirs), resolved, path: c.path }
            })
            .collect();
        r.walk(&head, |id, c| {
            history.push(Row::new(id, c));
            history.len() < 500
        })
        .map_err(e)?;
    }
    // ponytail: walks both histories on every refresh; cache by tip if big repos make it slow
    let ahead_behind = match r.read_ref(&format!("remotes/git/{branch}")) {
        Some(theirs) if !head.is_empty() && !cfg.github.is_empty() => {
            let (a, b) = (r.ancestors(&head).map_err(e)?, r.ancestors(&theirs).map_err(e)?);
            Some((a.keys().filter(|k| !b.contains_key(*k)).count(), b.keys().filter(|k| !a.contains_key(*k)).count()))
        }
        _ => None,
    };
    let unrelated = r
        .list_refs()
        .iter()
        .filter_map(|(n, id)| Some((n.strip_prefix("heads/")?, id)))
        .filter(|(_, id)| !head.is_empty() && **id != head && r.merge_bases(&head, id).is_ok_and(|b| b.is_empty()))
        .map(|(n, _)| n.to_string())
        .collect();
    let home = std::env::var("HOME").unwrap_or_default();
    let root = r.root.display().to_string();
    Ok(State {
        name: r.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        root: match root.strip_prefix(&home) {
            Some(rest) if !home.is_empty() => format!("~{rest}"),
            _ => root,
        },
        branch,
        head,
        unrelated,
        branches: r.list_refs().keys().filter_map(|n| n.strip_prefix("heads/")).map(String::from).collect(),
        remotes: cfg.remotes.keys().cloned().collect(),
        github: cfg.github,
        ahead_behind,
        changes,
        conflicts,
        history,
        now: now(),
    })
}

/// The working tree's diff, of one path or all of them.
#[tauri::command(async)]
fn working_diff(path: Option<String>) -> Res<Vec<DiffFile>> {
    let r = repo()?;
    let ds = r.working_deltas().map_err(|e| e.to_string())?;
    let work: HashSet<String> = ds.iter().map(|d| d.path.clone()).collect();
    let ds = r.with_renames_at(ds, RENAME_THRESHOLD, Some(&work));
    match path {
        Some(p) => render(&r, ds.into_iter().filter(|d| d.path == p).collect(), 0, Some(&HashSet::from([p]))),
        None => render(&r, ds, RENAME_THRESHOLD, Some(&work)),
    }
}

#[derive(Serialize)]
struct Detail {
    row: Row,
    /// who signed it, or why it is not validly signed
    signature: Result<String, String>,
    trusted: bool,
    diff: Vec<DiffFile>,
}

#[tauri::command(async)]
fn change(id: String) -> Res<Detail> {
    let r = repo()?;
    let c = r.get_change(&id).map_err(|e| e.to_string())?;
    let trusted = r.load_config().trusted;
    let (signature, ok) = match r.verify_change(&c) {
        Ok(key) => match trusted.get(&key) {
            Some(who) => (Ok(format!("{who} · {key}")), true),
            None => (Ok(key), false),
        },
        Err((_, e)) => (Err(e.to_string()), false),
    };
    Ok(Detail { row: Row::new(&id, &c), signature, trusted: ok, diff: change_diff(&r, &c)? })
}

type Cache = Mutex<HashMap<String, (usize, usize)>>;

/// +N −N of the given changes, worked out once each (changes never change).
#[tauri::command(async)]
fn stats(ids: Vec<String>, cache: tauri::State<Cache>) -> Res<HashMap<String, (usize, usize)>> {
    let r = repo()?;
    let mut out = HashMap::new();
    for id in ids {
        let known = cache.lock().unwrap().get(&id).copied();
        let s = match known {
            Some(s) => s,
            None => {
                let c = r.get_change(&id).map_err(|e| e.to_string())?;
                let s = change_diff(&r, &c)?.iter().fold((0, 0), |(a, d), f| (a + f.add, d + f.del));
                cache.lock().unwrap().insert(id.clone(), s);
                s
            }
        };
        out.insert(id, s);
    }
    Ok(out)
}

/// Runs a sprig command, as the CLI would, and returns what it printed.
#[tauri::command(async)]
fn run(args: Vec<String>) -> Res<String> {
    let (cmd, rest) = args.split_first().ok_or("no command")?;
    let mut out = vec![];
    let res = sprig::cli::run(cmd, rest, &mut out);
    let text = String::from_utf8_lossy(&out).trim().to_string();
    match res {
        Ok(()) | Err(Error::Exit(3, _)) => Ok(text), // 3: a merge that recorded conflicts
        Err(Error::Exit(_, m)) if m.is_empty() => Err(text),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Serialize)]
struct Sides {
    ours: String,
    base: String,
    theirs: String,
    /// the merged result in the working tree
    working: String,
}

/// HEAD's recorded conflict at path.
fn recorded(r: &Repo, path: &str) -> Res<sprig::store::Conflict> {
    let (_, head) = r.head();
    let c = r.get_change(&head).map_err(|e| e.to_string())?;
    c.conflicts.into_iter().find(|c| c.path == path).ok_or_else(|| "not a recorded conflict".into())
}

#[tauri::command(async)]
fn conflict(path: String) -> Res<Sides> {
    let r = repo()?;
    let cf = recorded(&r, &path)?;
    let text = |id: &str| {
        if id.is_empty() {
            return "(no file on this side)".to_string();
        }
        r.get_blob(id).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|e| format!("({e})"))
    };
    let working = std::fs::read(r.abs(&path)).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    Ok(Sides { ours: text(&cf.ours), base: text(&cf.base), theirs: text(&cf.theirs), working })
}

/// Writes an edited merge result, only to a recorded conflict's path.
#[tauri::command(async)]
fn save(path: String, text: String) -> Res<()> {
    let r = repo()?;
    recorded(&r, &path)?;
    std::fs::write(r.abs(&path), text).map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct Fetched {
    summary: String,
    /// local branches whose GitHub branch was deleted
    orphans: Vec<String>,
}

/// Fetches from GitHub, dropping branches deleted there; nothing without one.
#[tauri::command(async)]
fn fetch() -> Res<Fetched> {
    let r = repo()?;
    let url = r.load_config().github;
    if url.is_empty() {
        return Ok(Fetched { summary: String::new(), orphans: vec![] });
    }
    let f = r.github_fetch(&url).map_err(|e| e.to_string())?;
    Ok(Fetched { summary: f.summary, orphans: f.orphans })
}

#[derive(Serialize)]
struct SshKey {
    path: String,
    comment: String,
    /// its sprig public key
    key: String,
    /// passphrase-protected: sprig can't sign with it
    locked: bool,
}

#[derive(Serialize)]
struct Settings {
    /// this repository's commit author, split; empty when it uses the default
    name: String,
    email: String,
    /// the default for all repositories (~/.config/sprig/author), split
    global_name: String,
    global_email: String,
    /// SPRIG_AUTHOR is set and wins over both
    env_author: String,
    /// the public half of the signing key, if there is one, and its file
    key: Option<String>,
    key_file: String,
    /// whether this repository trusts the key
    trusted: bool,
    ssh: Vec<SshKey>,
    /// keys this repository trusts: (key, name)
    trusted_keys: Vec<(String, String)>,
}

fn split_author(a: &str) -> (String, String) {
    let (n, e) = a.split_once(" <").unwrap_or((a, ""));
    (n.to_string(), e.trim_end_matches('>').to_string())
}

#[tauri::command(async)]
fn settings() -> Res<Settings> {
    let r = repo()?;
    let e = |e: Error| e.to_string();
    let cfg = r.load_config();
    let (name, email) = split_author(&cfg.author);
    let (global_name, global_email) = split_author(&sprig::sign::global_author());
    let key = sprig::sign::my_public_key().map_err(e)?;
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".ssh");
    let mut ssh = vec![];
    for ent in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = ent.path();
        let Some(private) = p.to_str().and_then(|s| s.strip_suffix(".pub")) else { continue };
        let line = std::fs::read_to_string(&p).unwrap_or_default();
        let Some(public) = sprig::sign::ssh_public(&line) else { continue };
        if !std::path::Path::new(private).exists() {
            continue;
        }
        let locked = sprig::sign::read_ssh_key(std::path::Path::new(private))
            .is_err_and(|e| e.to_string().contains("passphrase"));
        let comment = line.split_whitespace().nth(2).unwrap_or("").to_string();
        ssh.push(SshKey { path: private.into(), comment, key: public, locked });
    }
    ssh.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Settings {
        name,
        email,
        global_name,
        global_email,
        env_author: std::env::var("SPRIG_AUTHOR").unwrap_or_default(),
        trusted: key.as_ref().is_some_and(|k| cfg.trusted.contains_key(k)),
        key,
        key_file: sprig::sign::key_file().map_err(e)?.display().to_string(),
        ssh,
        trusted_keys: cfg.trusted.into_iter().collect(),
    })
}

/// Sets the commit author for this repository, or the default for all of
/// them (which also drops this repository's own, so the default shows).
#[tauri::command(async)]
fn set_author(name: String, email: String, global: bool) -> Res<()> {
    let (name, email) = (name.trim(), email.trim());
    if name.is_empty() || [name, email].iter().any(|s| s.contains(['<', '>', '\n'])) {
        return Err("give a name, and an email without < > or line breaks".into());
    }
    let author = format!("{name} <{email}>");
    let r = repo()?;
    let mut cfg = r.load_config();
    if global {
        sprig::sign::set_global_author(&author).map_err(|e| e.to_string())?;
        cfg.author.clear();
    } else {
        cfg.author = author;
    }
    r.save_config(&cfg).map_err(|e| e.to_string())
}

/// Signs with the given SSH key, or a new sprig key when ssh is None, and
/// trusts that key in this repository so its changes verify. Without
/// replace, an existing key is kept and only trusted.
#[tauri::command(async)]
fn setup_signing(ssh: Option<String>, replace: bool) -> Res<String> {
    let r = repo()?;
    let e = |e: Error| e.to_string();
    let current = sprig::sign::my_public_key().map_err(e)?;
    let key = match current {
        Some(k) if !replace => k,
        _ => {
            if let Some(p) = &ssh {
                sprig::sign::read_ssh_key(std::path::Path::new(p)).map_err(e)?; // before the old key moves
            }
            sprig::sign::retire_key().map_err(e)?;
            match ssh {
                Some(p) => sprig::sign::link_ssh_key(std::path::Path::new(&p)).map_err(e)?.1,
                None => sprig::sign::generate_key().map_err(e)?.1,
            }
        }
    };
    let mut cfg = r.load_config();
    let name = cfg.author.split(" <").next().filter(|n| !n.is_empty()).map(String::from);
    let name = name.or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "me".into());
    cfg.trusted.insert(key.clone(), name);
    r.save_config(&cfg).map_err(e)?;
    Ok(key)
}

fn main() {
    let dir = std::env::args().nth(1).map_or_else(|| std::env::current_dir().unwrap(), PathBuf::from);
    // the commands find the repository from the current directory, as the CLI does
    if let Err(e) = std::env::set_current_dir(&dir) {
        eprintln!("sprig-ui: {}: {e}", dir.display());
        std::process::exit(1);
    }
    tauri::Builder::default()
        .manage(Cache::default())
        .invoke_handler(tauri::generate_handler![
            state,
            working_diff,
            change,
            stats,
            run,
            conflict,
            save,
            settings,
            set_author,
            fetch,
            setup_signing
        ])
        .run(tauri::generate_context!("ui/tauri.conf.json"))
        .expect("sprig-ui failed to start");
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_diff_numbers_lines() {
        let text = "modified: a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -2,3 +2,3 @@ hunk 1\n x\n-old\n+new\n y\n";
        let f = &super::parse_diff(text)[0];
        assert_eq!((f.path.as_str(), f.add, f.del, f.notes.len()), ("a.txt", 1, 1, 1));
        let l: Vec<_> = f.hunks[0].lines.iter().map(|l| (l.0, l.1, l.2)).collect();
        assert_eq!(l, [(Some(2), Some(2), ' '), (Some(3), None, '-'), (None, Some(3), '+'), (Some(4), Some(4), ' ')]);
    }
}
