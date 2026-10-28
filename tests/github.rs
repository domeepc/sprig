//! Syncing with a git host. A bare git repository on disk plays GitHub.

mod common;

use common::*;

fn host(c: &mut Ctx) -> (std::path::PathBuf, String) {
    let hub = c.temp();
    let seed = c.temp();
    git(&hub, &["init", "-q", "--bare", "-b", "main"]);
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    git(&seed, &["push", "-q", hub.to_str().unwrap(), "main"]);
    let url = hub.to_str().unwrap().to_string();
    (hub, url)
}

/// Commits in a git clone of the host and pushes them.
fn git_side(c: &mut Ctx, url: &str, file: &str, content: &str) {
    let w = c.temp();
    git(&w, &["clone", "-q", url, "."]);
    std::fs::write(w.join(file), content).unwrap();
    git(&w, &["add", "."]);
    git(&w, &["commit", "-q", "-m", &format!("git edits {file}")]);
    git(&w, &["push", "-q", "origin", "main"]);
}

#[test]
fn pull_push_and_merge() {
    if !has_git() {
        return;
    }
    let mut c = Ctx::new();
    let (hub, url) = host(&mut c);
    let r = c.repo();
    // first pull adopts the host's history and checks it out
    let out = r.github_pull(&url).unwrap();
    assert!(out.summary.contains("pulled 1 new change"), "{out:?}");
    assert_eq!(c.read("a.txt"), "one\n");

    // a local commit goes up
    c.write("b.txt", "from sprig\n");
    c.must(&["commit", "-m", "sprig work", "--agent", "claude"]);
    let r = c.repo();
    assert!(r.github_push(&url).unwrap().contains("1 new commit"));
    assert_eq!(git(&hub, &["show", "main:b.txt"]), "from sprig");
    assert!(git(&hub, &["log", "-1", "--format=%B", "main"]).contains("Sprig-Agent: claude"));

    // the host moved: pull fast-forwards
    git_side(&mut c, &url, "c.txt", "from git\n");
    let out = c.repo().github_pull(&url).unwrap();
    assert!(out.conflicts.is_empty(), "{out:?}");
    assert_eq!(c.read("c.txt"), "from git\n");

    // both moved: push is refused until a pull merges them
    git_side(&mut c, &url, "d.txt", "git again\n");
    c.write("e.txt", "sprig again\n");
    c.must(&["commit", "-m", "sprig again"]);
    let e = c.repo().github_push(&url).unwrap_err().to_string();
    assert!(e.contains("pull first"), "{e}");
    let out = c.repo().github_pull(&url).unwrap();
    assert!(out.summary.contains("merged") && out.conflicts.is_empty(), "{out:?}");
    assert_eq!(c.read("d.txt"), "git again\n");
    assert_eq!(c.read("e.txt"), "sprig again\n");
    c.repo().github_push(&url).unwrap();
    assert_eq!(git(&hub, &["show", "main:e.txt"]), "sprig again");
    assert_eq!(git(&hub, &["show", "main:d.txt"]), "git again");
    assert!(c.must(&["status"]).contains("clean"));
}

#[test]
fn pull_reports_conflicts() {
    if !has_git() {
        return;
    }
    let mut c = Ctx::new();
    let (_hub, url) = host(&mut c);
    c.repo().github_pull(&url).unwrap();
    git_side(&mut c, &url, "a.txt", "git side\n");
    c.write("a.txt", "sprig side\n");
    c.must(&["commit", "-m", "mine"]);
    let out = c.repo().github_pull(&url).unwrap();
    assert_eq!(out.conflicts, vec!["a.txt".to_string()], "{out:?}");
    assert!(c.read("a.txt").contains("<<<<<<< main"));
    // a conflicted tip is not pushed
    let e = c.repo().github_push(&url).unwrap_err().to_string();
    assert!(e.contains("unresolved"), "{e}");
    c.must(&["resolve", "--ours", "a.txt"]);
    c.must(&["commit", "-m", "resolved"]);
    c.repo().github_push(&url).unwrap();
}

#[test]
fn ssh_key_goes_into_the_mirror() {
    if !has_git() {
        return;
    }
    let mut c = Ctx::new();
    let (_hub, url) = host(&mut c);
    let dir = c.temp();
    let key = dir.join("it's a key");
    std::fs::write(&key, "").unwrap();
    c.must(&["github", "set", &url]);
    c.must(&["github", "key", key.to_str().unwrap()]);
    c.must(&["github", "pull"]);
    let m = c.repo().github_mirror();
    let key = std::fs::canonicalize(&key).unwrap();
    let want = format!("ssh -i '{}' -o IdentitiesOnly=yes", key.to_str().unwrap().replace('\'', "'\\''"));
    assert_eq!(git(&m, &["config", "core.sshCommand"]), want);
    // clearing it goes back to ssh's defaults on the next sync
    c.must(&["github", "key", ""]);
    c.must(&["github", "pull"]);
    let st = std::process::Command::new("git").arg("-C").arg(&m).args(["config", "core.sshCommand"]).status().unwrap();
    assert!(!st.success());
}

/// A working tree that also has a .git: every sprig command shows up there.
#[test]
fn local_git_follows_sprig() {
    if !has_git() {
        return;
    }
    let c = Ctx::new();
    let d = c.dir();
    git(&d, &["init", "-q", "-b", "main"]);
    c.write("a.txt", "one\n");
    c.must(&["commit", "-m", "first"]);
    assert_eq!(git(&d, &["log", "-1", "--format=%s"]), "first");
    assert_eq!(git(&d, &["status", "--porcelain"]), "", "index follows HEAD, .sprig is excluded");
    let first = git(&d, &["rev-parse", "HEAD"]);

    c.write("a.txt", "two\n");
    c.must(&["commit", "-m", "second"]);
    assert_eq!(git(&d, &["show", "HEAD:a.txt"]), "two");
    c.must(&["undo"]);
    assert_eq!(git(&d, &["rev-parse", "main"]), first, "undo moves git's branch back too");

    c.must(&["branch", "side"]);
    assert_eq!(git(&d, &["rev-parse", "side"]), first);
    c.must(&["checkout", "side"]);
    assert_eq!(git(&d, &["symbolic-ref", "--short", "HEAD"]), "side");
    c.must(&["checkout", "main"]);
    c.must(&["branch", "-d", "side"]);
    assert_eq!(git(&d, &["branch", "--list", "side"]), "");

    // a commit made with git alone is never overwritten
    git(&d, &["commit", "-q", "--allow-empty", "-m", "git only"]);
    let mine = git(&d, &["rev-parse", "main"]);
    c.write("a.txt", "three\n");
    c.must(&["commit", "-m", "third"]);
    assert_eq!(git(&d, &["rev-parse", "main"]), mine);
}

#[test]
fn fetch_prunes_deleted_branches() {
    if !has_git() {
        return;
    }
    let mut c = Ctx::new();
    let (hub, url) = host(&mut c);
    git(&hub, &["branch", "old", "main"]);
    c.must(&["github", "set", &url]);
    c.must(&["github", "pull"]);
    assert!(!c.must(&["branch"]).contains("old"), "fetch leaves local branches alone");
    c.must(&["branch", "old", "git/old"]);

    git(&hub, &["branch", "-D", "old"]);
    let out = c.must(&["github", "fetch"]);
    assert!(out.contains("removed git/old"), "{out}");
    assert!(out.contains("GitHub branch is gone: old"), "{out}");
    assert!(c.sp(&["show", "git/old"]).is_err(), "git/old is gone");
    assert!(c.must(&["branch"]).contains("old"), "the local branch stays until deleted");
}

/// Commits are written as changes against their parent; the trees must
/// still come out whole.
#[test]
fn export_writes_only_changes_but_whole_trees() {
    if !has_git() {
        return;
    }
    let mut c = Ctx::new();
    let out = c.temp();
    git(&out, &["init", "-q", "--bare"]);
    c.write("a.txt", "one\n");
    c.write("b.txt", "bee\n");
    c.write("dir/x", "x\n");
    c.must(&["commit", "-m", "first"]);
    c.write("a.txt", "two\n");
    std::fs::remove_file(c.path("b.txt")).unwrap();
    std::fs::remove_dir_all(c.path("dir")).unwrap();
    c.write("dir", "now a file\n");
    c.write("c/d.txt", "new\n");
    c.must(&["commit", "-m", "second"]);
    c.must(&["export-git", out.to_str().unwrap()]);
    assert_eq!(git(&out, &["ls-tree", "-r", "--name-only", "main"]), "a.txt\nc/d.txt\ndir");
    assert_eq!(git(&out, &["show", "main:a.txt"]), "two");
    assert_eq!(git(&out, &["show", "main:dir"]), "now a file");
    assert_eq!(git(&out, &["ls-tree", "-r", "--name-only", "main~1"]), "a.txt\nb.txt\ndir/x");
}
