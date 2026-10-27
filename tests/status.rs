//! The index and the fast paths built on it: status, diff, commit and
//! checkout must give the same answers as looking at everything.

mod common;

use common::*;
use std::time::{Duration, SystemTime};

/// status --json with the index thrown away first: every file is read.
fn full_status(c: &Ctx) -> String {
    let saved = c.repo().dir.join("index");
    let backup = std::fs::read(&saved).ok();
    let _ = std::fs::remove_file(&saved);
    c.setenv("SPRIG_NO_WATCH", "1");
    let out = c.must(&["status", "--json"]);
    c.setenv("SPRIG_NO_WATCH", "");
    if let Some(b) = backup {
        std::fs::write(&saved, b).unwrap(); // the fast path must not benefit from this run
    }
    out
}

#[track_caller]
fn same_as_full(c: &Ctx, step: &str) {
    let full = full_status(c);
    let fast = c.must(&["status", "--json"]);
    assert_eq!(fast, full, "{step}: indexed status differs from a full re-read");
}

/// Files whose mtime is well in the past, so the index trusts them.
fn age(c: &Ctx, paths: &[&str]) {
    let past = SystemTime::now() - Duration::from_secs(60);
    for p in paths {
        set_mtime(&c.path(p), past);
    }
}

#[test]
fn index_matches_full_read() {
    let c = Ctx::new();
    for i in 0..20 {
        c.write(&format!("d{}/f{i}.txt", i % 3), &i.to_string());
    }
    c.write("run.sh", "#!/bin/sh\n");
    c.must(&["commit", "-m", "base"]);
    age(&c, &["d0/f0.txt", "d1/f1.txt", "d2/f2.txt", "run.sh"]);
    c.must(&["status"]); // saves an index that trusts the aged files
    same_as_full(&c, "clean");

    c.write("d1/f1.txt", "edited");
    same_as_full(&c, "edit");
    c.write("d0/new.txt", "new");
    same_as_full(&c, "add");
    std::fs::remove_file(c.path("d2/f2.txt")).unwrap();
    same_as_full(&c, "delete");
    chmod(&c.path("run.sh"), 0o755);
    same_as_full(&c, "chmod");
    std::os::unix::fs::symlink("d0/f0.txt", c.path("link")).unwrap();
    same_as_full(&c, "symlink");
    std::fs::remove_dir_all(c.path("d0")).unwrap();
    same_as_full(&c, "delete directory");
    c.write("d0", "a file where a directory was");
    same_as_full(&c, "file replaces directory");
    c.write(".sprigignore", "d1\n");
    same_as_full(&c, "ignore file changed");
    // a torn index is ignored, not trusted
    let idx = c.repo().dir.join("index");
    let b = std::fs::read(&idx).unwrap();
    std::fs::write(&idx, &b[..b.len() / 2]).unwrap();
    same_as_full(&c, "torn index");
    std::fs::write(&idx, b"garbage").unwrap();
    same_as_full(&c, "garbage index");
    c.must(&["commit", "-m", "all of it"]);
    assert!(c.must(&["status"]).contains("clean"));
    same_as_full(&c, "after commit");
}

#[test]
fn racy_edit() {
    let c = Ctx::new();
    c.write("a.txt", "one\n");
    c.must(&["commit", "-m", "one"]);
    // same size, same mtime, within the racy window: must still be seen
    let mtime = std::fs::metadata(c.path("a.txt")).unwrap().modified().unwrap();
    c.write("a.txt", "two\n");
    set_mtime(&c.path("a.txt"), mtime);
    assert!(c.must(&["status"]).contains("M a.txt"), "racy edit missed");
}

#[test]
fn status_after_other_command_saves_index() {
    let c = Ctx::new();
    c.setenv("SPRIG_NO_WATCH", "1");
    c.write("f.txt", "one\n");
    c.write("g.txt", "g\n");
    c.must(&["commit", "-m", "base"]);
    let past = SystemTime::now() - Duration::from_secs(60);
    age(&c, &["f.txt", "g.txt"]);
    c.must(&["status"]);
    c.must(&["status"]);
    c.write("f.txt", "two\n");
    set_mtime(&c.path("f.txt"), past + Duration::from_secs(1));
    c.must(&["diff"]); // saves the index with f's new state
    assert!(c.must(&["status"]).contains("M f.txt"), "edit hidden by an index another command saved");
    assert!(c.must(&["status"]).contains("M f.txt"), "edit lost on the second status");
    std::fs::remove_file(c.path("g.txt")).unwrap();
    let out = c.must(&["status"]);
    assert!(out.contains("D g.txt") && out.contains("M f.txt"), "{out}");
}

#[test]
fn diff_reads_working_files_without_storing_them() {
    let c = Ctx::new();
    c.write("a.txt", "a\n");
    c.must(&["commit", "-m", "a"]);
    c.write("a.txt", "a\nmore\n");
    c.write("b.txt", "new file\n");
    let out = c.must(&["diff"]);
    assert!(out.contains("+more") && out.contains("added: b.txt") && out.contains("+new file"), "{out}");
    let r = c.repo();
    assert!(!r.has_object(&sprig::store::hash_object("blob", b"a\nmore\n")), "diff stored a working file");
    assert!(c.must(&["diff", "HEAD"]).contains("+more"));
}

#[test]
fn commit_after_gc_stores_every_blob() {
    let c = Ctx::new();
    c.write("a.txt", "1\n");
    c.write("b.txt", "b\n");
    c.must(&["commit", "-m", "one"]);
    c.write("a.txt", "2\n");
    c.must(&["status"]); // the index learns a.txt's new content without storing it
    c.must(&["gc"]);
    c.must(&["commit", "-m", "two"]);
    let r = c.repo();
    for (p, fe) in r.change_flat(&c.head()).unwrap() {
        assert!(r.get_blob(&fe.id).is_ok(), "{p}: blob missing after commit");
    }
    assert_eq!(c.must(&["show"]).matches("+2").count(), 1);
}

#[test]
fn checkout_touches_only_what_differs() {
    let c = Ctx::new();
    for i in 0..30 {
        c.write(&format!("d{}/f{i}.txt", i % 3), &format!("{i}\n"));
    }
    c.must(&["commit", "-m", "base"]);
    c.must(&["checkout", "-b", "other"]);
    c.write("d0/f0.txt", "changed\n");
    c.write("d1/new.txt", "new\n");
    std::fs::remove_file(c.path("d2/f2.txt")).unwrap();
    std::fs::remove_dir_all(c.path("d1")).unwrap();
    c.write("d1", "now a file\n");
    c.must(&["commit", "-m", "other"]);
    age(&c, &["d0/f3.txt"]);
    let untouched = std::fs::metadata(c.path("d0/f3.txt")).unwrap().modified().unwrap();
    for (branch, want_d1_file) in [("main", false), ("other", true), ("main", false)] {
        c.must(&["checkout", branch]);
        assert!(c.must(&["status"]).contains("clean"), "after checkout {branch}");
        same_as_full(&c, &format!("checkout {branch}"));
        assert_eq!(std::fs::metadata(c.path("d1")).unwrap().is_file(), want_d1_file);
        assert_eq!(
            c.repo().change_flat(&c.head()).unwrap().len(),
            std::fs::read_dir(c.path("d0")).unwrap().count()
                + std::fs::read_dir(c.path("d2")).unwrap().count()
                + if want_d1_file { 1 } else { 10 }
        );
    }
    assert_eq!(
        std::fs::metadata(c.path("d0/f3.txt")).unwrap().modified().unwrap(),
        untouched,
        "an unchanged file was rewritten"
    );
    // uncommitted work is never overwritten without -f; with -f it is, and untracked files go
    c.write("d0/f0.txt", "dirty\n");
    c.write("untracked.txt", "u\n");
    assert!(c.sp(&["checkout", "other"]).is_err(), "checkout over uncommitted work");
    assert_eq!(c.read("d0/f0.txt"), "dirty\n");
    c.must(&["checkout", "-f", "other"]);
    assert_eq!(c.read("d0/f0.txt"), "changed\n");
    assert!(!c.exists("untracked.txt"));
    assert!(c.must(&["status"]).contains("clean"));
}

/// Kills the child when dropped.
struct Kill(std::process::Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn watcher_fast_path_matches_full() {
    let c = Ctx::new();
    for i in 0..40 {
        c.write(&format!("d{}/f{i}.txt", i % 4), &i.to_string());
    }
    c.write("keep.txt", "k");
    c.must(&["commit", "-m", "base"]);
    let w = std::process::Command::new(env!("CARGO_BIN_EXE_sprig"))
        .arg("watch")
        .current_dir(c.dir())
        .envs(c.env.borrow().iter())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _w = Kill(w);
    let sock = c.repo().dir.join("watch.sock");
    wait_for("the watcher to start", 5, || sock.exists());
    std::thread::sleep(Duration::from_millis(1100)); // the index's save time must fall inside what the watcher vouches for
    c.must(&["status"]); // saves an index the watcher can vouch for
    let r = c.repo();
    let check = |step: &str| {
        std::thread::sleep(Duration::from_millis(150)); // let the watcher see the events
        let since = sprig_index_written(&r);
        let (_, ok, running) = sprig::watch::ask_watcher_since(&r, since);
        assert!(ok && running, "{step}: the watcher should vouch for the index");
        same_as_full(&c, step);
    };
    c.write("d1/f1.txt", "edited");
    check("edit");
    std::fs::remove_file(c.path("d2/f2.txt")).unwrap();
    check("delete");
    c.write("newdir/a/b.txt", "new");
    check("new directory");
    std::fs::rename(c.path("d0/f0.txt"), c.path("d0/g0.txt")).unwrap();
    check("rename");
    c.write("d1/f1.txt", "1"); // back to HEAD's content
    check("revert");
    std::fs::remove_dir_all(c.path("d3")).unwrap();
    check("delete directory");
    let out = c.must(&["status"]);
    assert!(
        out.contains("D d2/f2.txt") && out.contains("A newdir/a/b.txt") && out.contains("R d0/f0.txt -> d0/g0.txt"),
        "{out}"
    );
    // what counts as a file changed: the watcher can't vouch, everything is looked at
    c.write(".sprigignore", "newdir\n");
    std::thread::sleep(Duration::from_millis(150));
    assert!(!sprig::watch::ask_watcher_since(&r, sprig_index_written(&r)).1);
    same_as_full(&c, "ignore file changed");
}

/// The save time recorded in the index.
fn sprig_index_written(r: &sprig::store::Repo) -> i64 {
    let b = std::fs::read(r.dir.join("index")).unwrap();
    i64::from_le_bytes(b[8..16].try_into().unwrap())
}
