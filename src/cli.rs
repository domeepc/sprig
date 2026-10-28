//! The command line.

use crate::diff::{is_binary, split_lines, unified_diff};
use crate::merge::CommitOpts;
use crate::remote::Remote;
use crate::server::{Server, parse_scopes, serve};
use crate::sign::{Signature, check_sig};
use crate::store::{Change, Meta, Repo, short, valid_path, valid_ref_name};
use crate::tree::{Delta, FileEnt, RENAME_THRESHOLD, apply_deltas, diff_flat};
use crate::util::{self, Error, Result};
use crate::{bail, err};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

pub type Out<'a> = &'a mut dyn Write;

pub fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let res = run(&args[1], &args[2..], &mut out);
    let _ = out.flush();
    match res {
        Ok(()) => {}
        Err(Error::Exit(code, msg)) => {
            if !msg.is_empty() {
                eprintln!("sprig: {msg}");
            }
            std::process::exit(code);
        }
        Err(e) => {
            eprintln!("sprig: {e}");
            std::process::exit(1);
        }
    }
}

fn usage() {
    eprint!("{USAGE}");
}

/// What `sprig help` prints; shell completion reads the flags from it.
pub const USAGE: &str = "sprig - a small version control system for humans and agents

usage: sprig <command> [flags] [args]     (flags go before positional args)

  init [--bare] [-b branch] [dir]     create a repository
  clone [-b branch] [--path dir]... <url> <dir>   lazy (and optionally sparse) working copy
  commit -m msg [--intent s] [--agent s] [--model s] [--check k=v]... [--hunk path:N]... [path...]
  status [--json] [-M pct]            working tree vs HEAD, pending conflicts
  log [-n N] [--agent A] [--signatures] [--json] [rev]
  show [-M pct] [rev]                 a change and its diff (-M: rename similarity, default 50)
  diff [-M pct] [revA [revB]]         working tree vs HEAD, or between two revs
  branch [-d] [name [start]]          list, create or delete branches
  checkout [-f] <branch|rev> | checkout -b <name> [start]
  merge [--no-ff] [-m msg] <rev>      conflicts are recorded in the change
  conflicts                           list conflicts recorded in HEAD
  resolve (--ours|--theirs|--base) <path>...
  ops                                 operation log
  undo [-f]                           undo the last operation (undo again = redo)
  audit [--require check] [--signed] [--json] [rev]   agent changes lacking a check or trusted signature
  keygen                              create a signing key; commits, merges and landings get signed
  trust [<key> <name>]                list or add keys that audit --signed accepts
  remote add <name> <url> | remote list
  push [-f] [remote] [branch]    fetch [remote] [branch]    pull [remote] [branch]
  land -m msg [--intent s] [--check k=v]... [remote] [branch]
                                      send working-tree edits; the server rebases and lands them
  serve [--addr host:port] [--token t] [--tokens file] [--upstream url[,url]] [--follow url] [dir]
  import-git [--force] <git-repo>     convert a git repository's branches
  export-git [--force] <git-repo>     write branches into a git repo (then git push)
  cat <id>                            print a raw object
  gc [--repack]                       pack loose objects with deltas (runs by itself when they pile up)
  watch                               keep a list of changed files so status needn't stat every file
  github set <url> | github pull | github push
                                      sync the current branch with a GitHub (or any git) repository
  github key <file>                   ssh private key for it, e.g. a deploy key ('' = ssh's defaults)
  sparse [path... | .]                 show or change which paths are checked out (. = all)
  completion bash|zsh|fish            print a shell completion script (Tab completes commands,
                                      flags, branches, remotes and conflicted paths)
  version                             print the version

environment: SPRIG_AUTHOR, SPRIG_AGENT, SPRIG_MODEL, SPRIG_TOKEN, SPRIG_KEY (signing key file)
";

// ---------------------------------------------------------------------------
// Flags, parsed as Go's flag package does: "-f" or "--f", "-f=v" or "-f v",
// and parsing stops at the first argument that is not a flag (or "--").

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Value,
    Multi,
}

struct Flags {
    vals: HashMap<&'static str, Vec<String>>,
    args: Vec<String>,
}

fn parse(specs: &[(&'static str, Kind)], args: &[String]) -> Result<Flags> {
    let mut vals: HashMap<&'static str, Vec<String>> = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let s = &args[i];
        if s.len() < 2 || !s.starts_with('-') {
            break;
        }
        if s == "--" {
            i += 1;
            break;
        }
        let name = s.strip_prefix("--").unwrap_or(&s[1..]);
        if name.is_empty() || name.starts_with('-') || name.starts_with('=') {
            bail!("bad flag syntax: {s}");
        }
        let (name, value) = match name.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (name, None),
        };
        let Some(&(key, kind)) = specs.iter().find(|(n, _)| *n == name) else {
            if name == "h" || name == "help" {
                usage();
                return Err(Error::Exit(0, String::new()));
            }
            bail!("flag provided but not defined: -{name}");
        };
        let v = match (kind, value) {
            (Kind::Bool, None) => "true".to_string(),
            (Kind::Bool, Some(v)) => match v.as_str() {
                "1" | "t" | "T" | "TRUE" | "true" | "True" => "true".into(),
                "0" | "f" | "F" | "FALSE" | "false" | "False" => "false".into(),
                _ => bail!("invalid boolean value {v:?} for -{name}"),
            },
            (_, Some(v)) => v,
            (_, None) => {
                i += 1;
                args.get(i).cloned().ok_or_else(|| err!("flag needs an argument: -{name}"))?
            }
        };
        let e = vals.entry(key).or_default();
        if kind != Kind::Multi {
            e.clear();
        }
        e.push(v);
        i += 1;
    }
    Ok(Flags { vals, args: args[i..].to_vec() })
}

impl Flags {
    fn bool(&self, n: &str) -> bool {
        self.vals.get(n).is_some_and(|v| v[0] == "true")
    }

    fn str(&self, n: &str, def: &str) -> String {
        self.vals.get(n).map_or(def.to_string(), |v| v[0].clone())
    }

    fn int(&self, n: &str, def: i64) -> Result<i64> {
        match self.vals.get(n) {
            None => Ok(def),
            Some(v) => v[0].parse().map_err(|_| err!("invalid value {:?} for flag -{n}: parse error", v[0])),
        }
    }

    fn multi(&self, n: &str) -> Vec<String> {
        self.vals.get(n).cloned().unwrap_or_default()
    }

    fn arg(&self, i: usize) -> &str {
        self.args.get(i).map_or("", String::as_str)
    }
}

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}

/// The provenance flags shared by commit, merge and land.
const META_FLAGS: [(&str, Kind); 4] =
    [("intent", Kind::Value), ("agent", Kind::Value), ("model", Kind::Value), ("check", Kind::Multi)];

fn meta_of(f: &Flags) -> Result<Meta> {
    let mut checks = BTreeMap::new();
    for kv in f.multi("check") {
        match kv.split_once('=') {
            Some((k, v)) if !k.is_empty() => checks.insert(k.to_string(), v.to_string()),
            _ => bail!("bad --check {kv:?} (want name=value)"),
        };
    }
    Ok(Meta {
        intent: f.str("intent", ""),
        agent: f.str("agent", &env("SPRIG_AGENT")),
        model: f.str("model", &env("SPRIG_MODEL")),
        checks,
        git_sha: String::new(),
    })
}

fn with(a: &[(&'static str, Kind)], b: &[(&'static str, Kind)]) -> Vec<(&'static str, Kind)> {
    a.iter().chain(b).copied().collect()
}

fn print_json<T: serde::Serialize + ?Sized>(out: Out, v: &T) -> Result<()> {
    writeln!(out, "{}", util::to_json_pretty(v))?;
    Ok(())
}

fn first_line(s: &str) -> &str {
    s.split('\n').next().unwrap_or("")
}

/// Go's filepath.Clean for slash paths.
fn clean_path(p: &str) -> String {
    let mut out: Vec<&str> = vec![];
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." if out.last().is_some_and(|s| *s != "..") => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let s = out.join("/");
    if p.starts_with('/') {
        format!("/{s}")
    } else if s.is_empty() {
        ".".into()
    } else {
        s
    }
}

pub fn run(cmd: &str, args: &[String], out: Out) -> Result<()> {
    match cmd {
        "help" | "-h" | "--help" => {
            usage();
            return Ok(());
        }
        "version" => {
            writeln!(out, "sprig {}", env!("CARGO_PKG_VERSION"))?;
            return Ok(());
        }
        "init" => return cmd_init(args, out),
        "serve" => return cmd_serve(args, out),
        "keygen" => {
            let (path, key) = crate::sign::generate_key()?;
            writeln!(
                out,
                "wrote {}\npublic key: {key}\nyour changes are now signed; others can trust you with `sprig trust {key} <name>`",
                path.display()
            )?;
            return Ok(());
        }
        "clone" => return cmd_clone(args, out),
        "completion" => {
            out.write_all(crate::complete::script(args.first().map_or("", String::as_str))?.as_bytes())?;
            return Ok(());
        }
        "__complete" => return crate::complete::complete(args, out),
        _ => {}
    }
    let r = Repo::find()?;
    let res = match cmd {
        "commit" => cmd_commit(&r, args, out),
        "status" => cmd_status(&r, args, out),
        "log" => cmd_log(&r, args, out),
        "show" => cmd_show(&r, args, out),
        "diff" => cmd_diff(&r, args, out),
        "branch" => cmd_branch(&r, args, out),
        "checkout" => cmd_checkout(&r, args, out),
        "merge" => cmd_merge(&r, args, out),
        "conflicts" => cmd_conflicts(&r, out),
        "resolve" => cmd_resolve(&r, args, out),
        "ops" => cmd_ops(&r, out),
        "undo" => cmd_undo(&r, args, out),
        "audit" => cmd_audit(&r, args, out),
        "remote" => cmd_remote(&r, args, out),
        "push" => cmd_push(&r, args, out),
        "land" => cmd_land(&r, args, out),
        "fetch" => cmd_fetch(&r, args, out, false),
        "pull" => cmd_fetch(&r, args, out, true),
        "import-git" => cmd_import_git(&r, args, out),
        "export-git" => cmd_export_git(&r, args, out),
        "cat" => cmd_cat(&r, args, out),
        "sparse" => cmd_sparse(&r, args, out),
        "trust" => cmd_trust(&r, args, out),
        "github" => cmd_github(&r, args, out),
        "watch" => {
            let root = r.root.display().to_string();
            crate::watch::watch(&r, || {
                let _ = writeln!(out, "watching {root}; status and commit now ask this process (Ctrl-C to stop)");
                let _ = out.flush();
            })
        }
        "gc" => cmd_gc(&r, args, out),
        _ => {
            usage();
            return Err(Error::Exit(2, format!("unknown command {cmd:?}")));
        }
    };
    if matches!(cmd, "commit" | "merge" | "land" | "fetch" | "pull" | "resolve" | "import-git" | "github") {
        r.maybe_auto_gc(); // these add objects
    }
    res
}

// ---------------------------------------------------------------------------

fn cmd_init(args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("bare", Kind::Bool), ("b", Kind::Value)], args)?;
    let dir = if f.args.is_empty() { "." } else { f.arg(0) };
    std::fs::create_dir_all(dir)?;
    let r = Repo::init(&std::path::absolute(dir)?, f.bool("bare"), &f.str("b", "main"))?;
    writeln!(out, "initialized empty sprig repository in {}", r.dir.display())?;
    Ok(())
}

/// A lazy working copy: history and directory trees are downloaded, file
/// contents only when first read, and --path limits which files are written
/// to disk at all.
fn cmd_clone(args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("b", Kind::Value), ("path", Kind::Multi)], args)?;
    if f.args.len() != 2 {
        bail!("usage: clone [-b branch] [--path dir]... <url> <dir>");
    }
    let branch = f.str("b", "main");
    let mut sparse = vec![];
    for p in f.multi("path") {
        let p = p.trim_matches('/').to_string();
        if !valid_path(&p) {
            bail!("bad path {p:?}");
        }
        sparse.push(p);
    }
    std::fs::create_dir_all(f.arg(1))?;
    let abs = std::path::absolute(f.arg(1))?;
    let r = Repo::init(&abs, false, &branch)?;
    let mut cfg = r.load_config();
    cfg.remotes.insert("origin".into(), f.arg(0).trim_end_matches('/').to_string());
    cfg.lazy = "origin".into();
    cfg.sparse = sparse;
    r.save_config(&cfg)?;
    let rm = r.remote("origin")?;
    let (tip, n) = r.fetch(&rm, &branch)?;
    r.materialize(&BTreeMap::new(), &r.change_flat(&tip)?)?;
    r.write_ref(&format!("heads/{branch}"), &tip)?;
    writeln!(out, "cloned {branch} into {} ({n} objects; file contents load on demand)", abs.display())?;
    Ok(())
}

fn cmd_commit(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let specs = with(&META_FLAGS, &[("m", Kind::Value), ("allow-empty", Kind::Bool), ("hunk", Kind::Multi)]);
    let f = parse(&specs, args)?;
    let msg = f.str("m", "");
    if msg.is_empty() {
        bail!("a message is required (-m)");
    }
    let meta = meta_of(&f)?;
    let res = r.record("commit", || {
        let mut o = CommitOpts { message: msg.clone(), meta, allow_empty: f.bool("allow-empty"), ..Default::default() };
        for p in &f.args {
            let p = p.replace('\\', "/").trim_matches('/').to_string();
            if !valid_path(&p) {
                bail!("bad path {p:?}");
            }
            o.paths.push(p);
        }
        for h in f.multi("hunk") {
            let (p, n) = match h.rsplit_once(':') {
                Some((p, n)) if !p.is_empty() => {
                    (p, n.parse::<usize>().map_err(|_| err!("bad --hunk {h:?} (want path:N)"))?)
                }
                _ => bail!("bad --hunk {h:?} (want path:N)"),
            };
            let p = p.trim_matches('/').to_string();
            if !valid_path(&p) {
                bail!("bad path {p:?}");
            }
            o.hunks.entry(p).or_default().push(n);
        }
        r.commit(&o)
    });
    let id = match res {
        Err(Error::NothingToCommit) => return Err(Error::Exit(1, "nothing to commit".into())),
        r => r?,
    };
    let (rf, _) = r.head();
    writeln!(out, "[{} {}] {}", rf.trim_start_matches("heads/"), short(&id), first_line(&msg))?;
    if let Ok(c) = r.get_change(&id)
        && !c.conflicts.is_empty()
    {
        writeln!(out, "warning: {} unresolved conflict(s) carried forward (see `sprig conflicts`)", c.conflicts.len())?;
    }
    Ok(())
}

fn cmd_status(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("json", Kind::Bool), ("M", Kind::Value)], args)?;
    let sim = f.int("M", RENAME_THRESHOLD as i64)?.max(0) as usize;
    let (rf, head) = r.head();
    let ds = r.working_deltas()?;
    let work: HashSet<String> = ds.iter().map(|d| d.path.clone()).collect();
    let deltas = r.with_renames_at(ds, sim, Some(&work));
    let conflicts = if head.is_empty() { vec![] } else { r.get_change(&head)?.conflicts };
    let branch = rf.trim_start_matches("heads/");
    if f.bool("json") {
        let changes: Vec<_> = deltas
            .iter()
            .map(|d| {
                let mut m = serde_json::json!({ "path": d.path, "kind": d.kind.to_string() });
                if !d.from.is_empty() {
                    m["from"] = d.from.clone().into();
                }
                m
            })
            .collect();
        let paths: Vec<&str> = conflicts.iter().map(|c| c.path.as_str()).collect();
        return print_json(
            out,
            &serde_json::json!({ "branch": branch, "head": head, "changes": changes, "conflicts": paths }),
        );
    }
    if rf.is_empty() {
        write!(out, "HEAD detached")?;
    } else {
        write!(out, "On branch {branch}")?;
    }
    if head.is_empty() {
        writeln!(out, " (no changes yet)")?;
    } else {
        writeln!(out, " ({})", short(&head))?;
    }
    if !conflicts.is_empty() {
        writeln!(out, "\nunresolved conflicts ({}):", conflicts.len())?;
        for c in &conflicts {
            writeln!(out, "  C {}", c.path)?;
        }
    }
    if deltas.is_empty() {
        writeln!(out, "\nworking tree clean")?;
        return Ok(());
    }
    writeln!(out, "\nchanges since last change:")?;
    for d in &deltas {
        if d.kind == 'R' {
            let edited = if d.old.as_ref().map(|e| &e.id) != d.new.as_ref().map(|e| &e.id) { " (edited)" } else { "" };
            writeln!(out, "  R {} -> {}{edited}", d.from, d.path)?;
        } else {
            writeln!(out, "  {} {}", d.kind, d.path)?;
        }
    }
    Ok(())
}

pub fn print_change_header(r: &Repo, out: Out, id: &str, c: &Change, sigs: bool) -> Result<()> {
    let tag = if c.parents.len() > 1 { " (merge)" } else { "" };
    writeln!(out, "change {id}{tag}")?;
    writeln!(out, "Author: {}", c.author)?;
    writeln!(out, "Date:   {} {}", util::fmt_date(c.time), c.tz)?;
    if !c.meta.agent.is_empty() || !c.meta.model.is_empty() {
        writeln!(out, "Agent:  {} {}", c.meta.agent, c.meta.model)?;
    }
    if !c.meta.intent.is_empty() {
        writeln!(out, "Intent: {}", c.meta.intent)?;
    }
    if !c.meta.checks.is_empty() {
        let parts: Vec<String> = c.meta.checks.iter().map(|(k, v)| format!("{k}={v}")).collect();
        writeln!(out, "Checks: {}", parts.join(" "))?;
    }
    if !c.conflicts.is_empty() {
        writeln!(out, "Conflicts: {} unresolved", c.conflicts.len())?;
    }
    if !c.projected.is_empty() {
        writeln!(out, "Projected from {} (a read-scoped view of that change)", short(&c.projected))?;
    }
    if c.sig.is_some() && sigs {
        let (key, res) = match r.verify_change(c) {
            Ok(k) => (k, Ok(())),
            Err((k, e)) => (k, Err(e)),
        };
        let who = match r.load_config().trusted.get(&key) {
            Some(name) => format!("{name} (trusted)"),
            None => key,
        };
        match res {
            Err(e) => writeln!(out, "Signed: INVALID ({e}) {who}")?,
            Ok(()) => writeln!(out, "Signed: {who}")?,
        }
    }
    for l in c.message.split('\n') {
        write!(out, "\n    {l}")?;
    }
    write!(out, "\n\n")?;
    Ok(())
}

fn cmd_log(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f =
        parse(&[("n", Kind::Value), ("agent", Kind::Value), ("json", Kind::Bool), ("signatures", Kind::Bool)], args)?;
    let n = f.int("n", 20)?.max(0) as usize;
    let agent = f.str("agent", "");
    let start = r.resolve_rev(if f.args.is_empty() { "HEAD" } else { f.arg(0) })?;
    let mut items = vec![];
    r.walk(&start, |id, c| {
        if agent.is_empty() || c.meta.agent == agent {
            items.push(crate::server::LogItem { id: id.into(), change: c.clone() });
        }
        items.len() < n
    })?;
    if f.bool("json") {
        return print_json(out, &items);
    }
    for it in &items {
        print_change_header(r, out, &it.id, &it.change, f.bool("signatures"))?;
    }
    Ok(())
}

/// Renders deltas as a diff. New sides of the paths in work are read from
/// the working tree, everything else from the store.
pub fn render_diff(r: &Repo, out: Out, ds: Vec<Delta>, sim: usize, work: Option<&HashSet<String>>) -> Result<()> {
    let read = |fe: &Option<FileEnt>, path: &str, new: bool| -> Result<Vec<u8>> {
        let Some(fe) = fe else { return Ok(vec![]) };
        if new && work.is_some_and(|w| w.contains(path)) {
            return Ok(r.read_work(path, fe.mode)?);
        }
        Ok(r.get_blob(&fe.id)?.to_vec())
    };
    for d in r.with_renames_at(ds, sim, work) {
        let (om, nm) = (d.old.as_ref().map(|e| e.mode.as_str()), d.new.as_ref().map(|e| e.mode.as_str()));
        let same_id = d.old.as_ref().map(|e| &e.id) == d.new.as_ref().map(|e| &e.id);
        if d.kind == 'R' {
            writeln!(out, "renamed: {} -> {}", d.from, d.path)?;
            if om != nm {
                writeln!(out, "mode change: {} {} -> {}", d.path, om.unwrap(), nm.unwrap())?;
            }
            if same_id {
                continue;
            }
        }
        let od = if d.kind != 'A' { read(&d.old, &d.from, false)? } else { vec![] };
        let nd = if d.kind != 'D' { read(&d.new, &d.path, true)? } else { vec![] };
        match d.kind {
            'A' => writeln!(out, "added: {} ({})", d.path, nm.unwrap())?,
            'D' => writeln!(out, "deleted: {}", d.path)?,
            'M' if om != nm => writeln!(out, "mode change: {} {} -> {}", d.path, om.unwrap(), nm.unwrap())?,
            _ => {}
        }
        if is_binary(&od) || is_binary(&nd) {
            if d.kind == 'M' && same_id {
                continue;
            }
            writeln!(out, "Binary files a/{0} and b/{0} differ", d.path)?;
            continue;
        }
        out.write_all(&unified_diff(&d.path, &split_lines(&od), &split_lines(&nd), 3))?;
    }
    Ok(())
}

fn cmd_show(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("M", Kind::Value)], args)?;
    let sim = f.int("M", RENAME_THRESHOLD as i64)?.max(0) as usize;
    let id = r.resolve_rev(if f.args.is_empty() { "HEAD" } else { f.arg(0) })?;
    let c = r.get_change(&id)?;
    print_change_header(r, out, &id, &c, true)?;
    let pt = r.tree_of(c.parents.first().map_or("", String::as_str))?;
    let ds = r.diff_trees(&pt, &c.tree)?;
    render_diff(r, out, ds, sim, None)
}

fn cmd_diff(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("M", Kind::Value)], args)?;
    let sim = f.int("M", RENAME_THRESHOLD as i64)?.max(0) as usize;
    match f.args.len() {
        0 => {
            // only the changed files are read, and nothing is stored
            let ds = r.working_deltas()?;
            let work: HashSet<String> = ds.iter().map(|d| d.path.clone()).collect();
            render_diff(r, out, ds, sim, Some(&work))
        }
        1 => {
            let a = r.change_flat(&r.resolve_rev(f.arg(0))?)?;
            let st = r.work_status(false)?;
            st.save(r);
            let work: HashSet<String> = st.deltas.iter().map(|d| d.path.clone()).collect();
            let b = apply_deltas(r.flatten(&st.tree)?, &st.deltas);
            render_diff(r, out, diff_flat(&a, &b), sim, Some(&work))
        }
        2 => {
            // two revisions: only changed directories are read
            let ta = r.tree_of(&r.resolve_rev(f.arg(0))?)?;
            let tb = r.tree_of(&r.resolve_rev(f.arg(1))?)?;
            render_diff(r, out, r.diff_trees(&ta, &tb)?, sim, None)
        }
        _ => bail!("usage: diff [revA [revB]]"),
    }
}

fn cmd_branch(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("d", Kind::Bool)], args)?;
    if f.args.is_empty() {
        let (cur, _) = r.head();
        let mut names: Vec<String> =
            r.list_refs().keys().filter_map(|n| n.strip_prefix("heads/")).map(String::from).collect();
        if let Some(c) = cur.strip_prefix("heads/")
            && !names.iter().any(|n| n == c)
        {
            names.push(c.to_string());
        }
        names.sort();
        for n in names {
            let mark = if format!("heads/{n}") == cur { "* " } else { "  " };
            let id = r.read_ref(&format!("heads/{n}")).unwrap_or_default();
            writeln!(out, "{mark}{n:<20} {}", short(&id))?;
        }
        return Ok(());
    }
    let name = f.arg(0);
    if !valid_ref_name(&format!("heads/{name}")) {
        bail!("invalid branch name {name:?}");
    }
    if f.bool("d") {
        if r.head().0 == format!("heads/{name}") {
            bail!("cannot delete the current branch");
        }
        return r.record("branch -d", || r.delete_ref(&format!("heads/{name}")));
    }
    if r.read_ref(&format!("heads/{name}")).is_some() {
        bail!("branch {name:?} already exists");
    }
    let id = r.resolve_rev(if f.args.len() > 1 { f.arg(1) } else { "HEAD" })?;
    r.record(&format!("branch {name}"), || r.write_ref(&format!("heads/{name}"), &id))
}

fn cmd_checkout(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("b", Kind::Bool), ("f", Kind::Bool)], args)?;
    let create = f.bool("b");
    if f.args.is_empty() || f.args.len() > 2 || (f.args.len() == 2 && !create) {
        bail!("usage: checkout [-f] <branch|rev>   or   checkout -b <name> [start]");
    }
    let name = f.arg(0);
    r.record(&format!("checkout {name}"), || {
        r.checkout(name, f.bool("f"), create, f.arg(1))?;
        writeln!(out, "switched to {name}")?;
        Ok(())
    })
}

fn cmd_merge(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let specs = with(&META_FLAGS, &[("no-ff", Kind::Bool), ("m", Kind::Value), ("json", Kind::Bool)]);
    let f = parse(&specs, args)?;
    if f.args.len() != 1 {
        bail!("usage: merge [--no-ff] [-m msg] <rev>");
    }
    let meta = meta_of(&f)?;
    let rev = f.arg(0);
    let res = r.record(&format!("merge {rev}"), || r.merge(rev, f.bool("no-ff"), &f.str("m", ""), meta))?;
    if f.bool("json") {
        let paths: Vec<&str> = res.conflicts.iter().map(|c| c.path.as_str()).collect();
        print_json(out, &serde_json::json!({ "result": res.kind, "change": res.change, "conflicts": paths }))?;
    } else {
        match res.kind {
            "up-to-date" => writeln!(out, "already up to date")?,
            "fast-forward" => writeln!(out, "fast-forward to {}", short(&res.change))?,
            _ => writeln!(out, "merged as {}", short(&res.change))?,
        }
        for c in &res.conflicts {
            writeln!(out, "CONFLICT {}", c.path)?;
        }
        if !res.conflicts.is_empty() {
            writeln!(
                out,
                "conflicts are recorded in the change; edit the files (or `sprig resolve --ours/--theirs`) and commit"
            )?;
        }
    }
    if !res.conflicts.is_empty() {
        return Err(Error::Exit(3, String::new()));
    }
    Ok(())
}

fn cmd_conflicts(r: &Repo, out: Out) -> Result<()> {
    let (_, head) = r.head();
    if head.is_empty() {
        return Ok(());
    }
    for cf in r.get_change(&head)?.conflicts {
        writeln!(out, "{}\tbase={} ours={} theirs={}", cf.path, short(&cf.base), short(&cf.ours), short(&cf.theirs))?;
    }
    Ok(())
}

fn cmd_resolve(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("ours", Kind::Bool), ("theirs", Kind::Bool), ("base", Kind::Bool)], args)?;
    let n = ["ours", "theirs", "base"].iter().filter(|k| f.bool(k)).count();
    if n != 1 || f.args.is_empty() {
        bail!("usage: resolve (--ours|--theirs|--base) <path>...");
    }
    let (_, head) = r.head();
    if head.is_empty() {
        bail!("no changes yet");
    }
    let c = r.get_change(&head)?;
    for p in &f.args {
        let p = clean_path(&p.replace('\\', "/"));
        let Some(cf) = c.conflicts.iter().rev().find(|cf| cf.path == p) else {
            bail!("{p} has no recorded conflict");
        };
        let pick = if f.bool("theirs") {
            &cf.theirs
        } else if f.bool("base") {
            &cf.base
        } else {
            &cf.ours
        };
        if pick.is_empty() {
            // that side has no file: resolving to it means deleting
            r.remove_work(&p)?;
            writeln!(out, "resolved {p} (removed)")?;
            continue;
        }
        let mode = r.lookup(&c.tree, &p)?.map_or(crate::tree::Mode::File, |e| e.mode);
        r.write_file(&p, &FileEnt::new(mode, pick.clone()))?;
        writeln!(out, "resolved {p}")?;
    }
    writeln!(out, "commit to record the resolution")?;
    Ok(())
}

fn cmd_ops(r: &Repo, out: Out) -> Result<()> {
    for o in r.ops()?.iter().rev() {
        writeln!(out, "{:4}  {}  {}", o.n, util::fmt_datetime(o.time), o.cmd)?;
    }
    Ok(())
}

fn cmd_undo(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("f", Kind::Bool)], args)?;
    let op = r.undo(f.bool("f"))?;
    writeln!(out, "undid operation {}: {}", op.n, op.cmd)?;
    Ok(())
}

fn cmd_audit(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("require", Kind::Value), ("signed", Kind::Bool), ("json", Kind::Bool)], args)?;
    let require = f.str("require", "review");
    let signed = f.bool("signed");
    let start = r.resolve_rev(if f.args.is_empty() { "HEAD" } else { f.arg(0) })?;
    let trusted = r.load_config().trusted;
    if signed && trusted.is_empty() {
        bail!("no trusted keys; add one with `sprig trust <key> <name>`");
    }
    #[derive(serde::Serialize)]
    struct Finding {
        id: String,
        agent: String,
        #[serde(skip_serializing_if = "String::is_empty")]
        model: String,
        message: String,
        problem: String,
    }
    let mut found = vec![];
    let mut total = 0;
    r.walk(&start, |id, c| {
        if c.meta.agent.is_empty() {
            return true;
        }
        total += 1;
        let check = c.meta.checks.get(&require).map_or(String::new(), |v| v.to_lowercase());
        let mut problem = String::new();
        if !matches!(check.as_str(), "pass" | "passed" | "ok" | "approved" | "true") {
            problem = format!("no passing {require} check");
        }
        if signed && problem.is_empty() {
            match r.verify_change(c) {
                Err((_, e)) => problem = format!("signature: {e}"),
                Ok(key) if !trusted.contains_key(&key) => problem = format!("signed by untrusted key {key}"),
                Ok(_) => {}
            }
        }
        if !problem.is_empty() {
            found.push(Finding {
                id: id.into(),
                agent: c.meta.agent.clone(),
                model: c.meta.model.clone(),
                message: first_line(&c.message).into(),
                problem,
            });
        }
        true
    })?;
    if f.bool("json") {
        print_json(out, &found)?;
    } else {
        let mut what = format!("a passing {require:?} check");
        if signed {
            what += " or a trusted signature";
        }
        writeln!(out, "{} of {total} agent-authored changes lack {what}", found.len())?;
        for x in &found {
            writeln!(out, "  {}  {}  {}  ({})", short(&x.id), x.agent, x.message, x.problem)?;
        }
    }
    if !found.is_empty() {
        return Err(Error::Exit(1, String::new()));
    }
    Ok(())
}

fn cmd_remote(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let mut cfg = r.load_config();
    match args {
        [] => {}
        [l] if l == "list" => {}
        [a, name, url] if a == "add" => {
            if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
                bail!("invalid remote name");
            }
            cfg.remotes.insert(name.clone(), url.clone());
            return r.save_config(&cfg);
        }
        _ => bail!("usage: remote add <name> <url> | remote list"),
    }
    for (n, u) in &cfg.remotes {
        writeln!(out, "{n}\t{u}")?;
    }
    Ok(())
}

fn remote_args(r: &Repo, f: &Flags) -> Result<(Remote, String)> {
    let name = if f.args.is_empty() { "origin" } else { f.arg(0) };
    let mut branch = f.arg(1).to_string();
    if branch.is_empty() {
        let (rf, _) = r.head();
        if rf.is_empty() {
            bail!("HEAD is detached; name a branch");
        }
        branch = rf.trim_start_matches("heads/").to_string();
    }
    Ok((r.remote(name)?, branch))
}

fn cmd_push(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("f", Kind::Bool)], args)?;
    let (rm, branch) = remote_args(r, &f)?;
    writeln!(out, "{}", r.push(&rm, &branch, f.bool("f"))?)?;
    Ok(())
}

fn cmd_land(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&with(&META_FLAGS, &[("m", Kind::Value)]), args)?;
    let msg = f.str("m", "");
    if msg.is_empty() {
        bail!("a message is required (-m)");
    }
    let meta = meta_of(&f)?;
    let (rm, branch) = remote_args(r, &f)?;
    let res = match r.record(&format!("land {}", rm.name), || r.land(&rm, &branch, &msg, meta)) {
        Err(Error::NothingToCommit) => return Err(Error::Exit(1, "nothing to land".into())),
        r => r?,
    };
    writeln!(out, "[{branch} {}] {} (landed on {})", short(&res.id), first_line(&msg), short(&res.parent))?;
    Ok(())
}

fn cmd_fetch(r: &Repo, args: &[String], out: Out, integrate: bool) -> Result<()> {
    let f = parse(&[], args)?;
    let (rm, branch) = remote_args(r, &f)?;
    let (tip, n) = r.record(&format!("fetch {}", rm.name), || r.fetch(&rm, &branch))?;
    writeln!(out, "fetched {}/{branch} -> {} ({n} objects)", rm.name, short(&tip))?;
    if !integrate {
        return Ok(());
    }
    if r.head().0 != format!("heads/{branch}") {
        writeln!(out, "not on {branch}; run `sprig checkout {branch}` then `sprig merge {}/{branch}`", rm.name)?;
        return Ok(());
    }
    cmd_merge(r, &[format!("{}/{branch}", rm.name)], out) // fast-forwards when possible
}

fn cmd_import_git(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("force", Kind::Bool)], args)?;
    if f.args.len() != 1 {
        bail!("usage: import-git [--force] <path-to-git-repo>");
    }
    if r.bare {
        bail!("run import-git in a repository with a working tree");
    }
    let res = r.import_git(f.arg(0), f.bool("force"))?;
    writeln!(
        out,
        "imported {} changes, {} file versions across {} branch(es)",
        res.changes,
        res.blobs,
        res.branches.len()
    )?;
    for (n, id) in &res.branches {
        writeln!(out, "  {n:<20} {}", short(id))?;
    }
    if res.skipped > 0 {
        writeln!(out, "note: skipped {} submodule entries", res.skipped)?;
    }
    for w in &res.warnings {
        writeln!(out, "warning: {w}")?;
    }
    Ok(())
}

fn cmd_export_git(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("force", Kind::Bool)], args)?;
    if f.args.len() != 1 {
        bail!("usage: export-git [--force] <path-to-git-repo>");
    }
    let res = r.export_git(f.arg(0), f.bool("force"))?;
    writeln!(out, "wrote {} commit(s) across {} branch(es)", res.commits, res.branches.len())?;
    for (n, sha) in &res.branches {
        writeln!(out, "  {n:<20} {}", short(sha))?;
    }
    for w in &res.warnings {
        writeln!(out, "warning: {w}")?;
    }
    Ok(())
}

/// Changes a working tree's view: files entering it are written (loading
/// content on demand in a lazy clone), files leaving it are removed.
fn cmd_sparse(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let cfg = r.load_config();
    if args.is_empty() {
        if cfg.sparse.is_empty() {
            writeln!(out, "(everything is checked out)")?;
        }
        for p in &cfg.sparse {
            writeln!(out, "{p}")?;
        }
        return Ok(());
    }
    if r.bare {
        bail!("bare repository has no working tree");
    }
    let mut next = cfg.clone();
    next.sparse.clear();
    for p in args {
        let p = p.trim_matches('/');
        if p == "." || p.is_empty() {
            next.sparse.clear(); // "." means everything
            break;
        }
        if !valid_path(p) {
            bail!("bad path {p:?}");
        }
        next.sparse.push(p.to_string());
    }
    let dirty = r.working_deltas()?;
    if !dirty.is_empty() {
        bail!("uncommitted changes ({} paths); commit them first", dirty.len());
    }
    let (_, head) = r.head();
    let (mut added, mut removed) = (0, 0);
    for (p, fe) in r.change_flat(&head)? {
        match (cfg.in_view(&p), next.in_view(&p)) {
            (false, true) => {
                r.write_file(&p, &fe)?;
                added += 1;
            }
            (true, false) => {
                r.remove_work(&p)?;
                removed += 1;
            }
            _ => {}
        }
    }
    r.save_config(&next)?;
    writeln!(out, "checked out {added} more file(s), removed {removed}")?;
    Ok(())
}

fn cmd_trust(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let mut cfg = r.load_config();
    match args {
        [] => {
            for (k, name) in &cfg.trusted {
                writeln!(out, "{name}\t{k}")?;
            }
            Ok(())
        }
        [key, name] => {
            if check_sig(&Signature { key: key.clone(), ..Default::default() }, &[])
                .is_err_and(|e| e.to_string() == "malformed key")
            {
                bail!("{key:?} is not a sprig public key (expected ed25519:...)");
            }
            cfg.trusted.insert(key.clone(), name.clone());
            r.save_config(&cfg)
        }
        _ => bail!("usage: trust [<key> <name>]"),
    }
}

/// Syncs with a git host through a mirror in .sprig/github.git; git does
/// the network part with the user's credentials.
fn cmd_github(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let url = r.load_config().github;
    match args {
        [set, u] if set == "set" => {
            let mut cfg = r.load_config();
            cfg.github = u.trim().to_string();
            r.save_config(&cfg)?;
            writeln!(out, "syncing with {}", cfg.github)?;
        }
        [k, f] if k == "key" => {
            let mut cfg = r.load_config();
            cfg.github_key = if f.is_empty() {
                String::new()
            } else {
                std::fs::canonicalize(f).map_err(|e| err!("{f}: {e}"))?.to_string_lossy().into_owned()
            };
            r.save_config(&cfg)?;
            if f.is_empty() {
                writeln!(out, "using ssh's default keys")?;
            } else {
                writeln!(out, "using ssh key {}", cfg.github_key)?;
            }
        }
        [] => writeln!(out, "{}", if url.is_empty() { "(not set; `sprig github set <url>`)" } else { &url })?,
        [p] if p == "pull" => {
            let res = r.github_pull(&url)?;
            writeln!(out, "{}", res.summary)?;
            for c in &res.conflicts {
                writeln!(out, "CONFLICT {c}")?;
            }
            if !res.conflicts.is_empty() {
                writeln!(out, "conflicts are recorded in the change; resolve them and commit")?;
                return Err(Error::Exit(3, String::new()));
            }
        }
        [p] if p == "push" => writeln!(out, "{}", r.github_push(&url)?)?,
        _ => bail!("usage: github set <url> | github key <file> | github pull | github push"),
    }
    Ok(())
}

fn cmd_cat(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let [id] = args else { bail!("usage: cat <id>") };
    let id = if crate::store::is_id(id) { id.clone() } else { r.resolve_rev(id)? };
    let (typ, data) = r.get_object(&id)?;
    eprintln!("({typ}, {} bytes)", data.len());
    out.write_all(&data)?;
    Ok(())
}

fn cmd_gc(r: &Repo, args: &[String], out: Out) -> Result<()> {
    let f = parse(&[("repack", Kind::Bool), ("auto", Kind::Bool)], args)?;
    let auto = f.bool("auto");
    if auto && !r.needs_gc() {
        return Ok(());
    }
    let res = r.gc(f.bool("repack"));
    if auto {
        return Ok(()); // background run: nobody is reading, and a busy lock is fine
    }
    let res = res?;
    if res.repacked > 0 {
        writeln!(out, "merged {} packs", res.repacked)?;
    }
    writeln!(
        out,
        "packed {} objects ({} as deltas): {} KB -> {} KB",
        res.packed,
        res.deltas,
        res.before / 1024,
        res.after / 1024
    )?;
    if res.deleted_loose_err > 0 {
        writeln!(out, "warning: {} loose objects could not be removed (they are packed too)", res.deleted_loose_err)?;
    }
    Ok(())
}

fn cmd_serve(args: &[String], out: Out) -> Result<()> {
    let f = parse(
        &[
            ("addr", Kind::Value),
            ("token", Kind::Value),
            ("tokens", Kind::Value),
            ("upstream", Kind::Value),
            ("follow", Kind::Value),
        ],
        args,
    )?;
    let mut r = Repo::open(std::path::Path::new(if f.args.is_empty() { "." } else { f.arg(0) }))?;
    let (upstream, follow) = (f.str("upstream", ""), f.str("follow", ""));
    if !upstream.is_empty() {
        if !r.bare {
            bail!("a cache needs its own bare repository (sprig init --bare cache.sprig)");
        }
        r.fallback = Some(Arc::new(Remote::new("upstream", &upstream)));
    } else {
        r.durable = true; // the authority: acknowledged writes must survive a crash
    }
    if !follow.is_empty() && (!r.bare || !upstream.is_empty()) {
        bail!("--follow needs its own bare repository and no --upstream");
    }
    let mut s = Server::new(r, &f.str("token", &env("SPRIG_TOKEN")));
    let tokens = f.str("tokens", "");
    if !tokens.is_empty() {
        s.scopes = parse_scopes(&std::fs::read(&tokens)?, &tokens)?;
    }
    s.upstream = s.repo.fallback.clone();
    if !follow.is_empty() {
        s.follow = Some(Arc::new(Remote::new("follow", &follow)));
    }
    serve(Arc::new(s), &f.str("addr", "127.0.0.1:7700"), out)
}
