//! Commands on one repository: commit, status, history, merges, renames,
//! packs, and git import/export.

mod common;

use common::*;
use sprig::pack::{AUTO_GC_LOOSE, REPACK_AT};
use sprig::store::{BLOB, Change, Conflict, Repo, TREE, children, hash_object};
use sprig::tree::{Entry, FileEnt, Flat, Mode, Tree};
use std::collections::BTreeMap;

#[test]
fn store_integrity() {
    let c = Ctx::new();
    let r = c.repo();
    let id = r.put_blob(b"hello").unwrap();
    assert_eq!(&*r.get_blob(&id).unwrap(), b"hello");
    // Overwrite the object with a different valid-looking one: the hash check must catch it.
    let other = r.put_blob(b"evil").unwrap();
    std::fs::copy(r.obj_path(&other), r.obj_path(&id)).unwrap();
    let fresh = c.repo(); // a cached copy is the verified original; this checks what is on disk
    let err = fresh.get_blob(&id).unwrap_err().to_string();
    assert!(err.contains("corrupt"), "tampering not detected: {err}");
}

#[test]
fn malicious_tree_rejected() {
    for name in ["..", ".", "a/b", "", "x\0y"] {
        let t = Tree { entries: vec![Entry { name: name.into(), mode: Mode::File, id: hash_object(BLOB, b"") }] };
        assert!(children(TREE, &sprig::util::to_json(&t)).is_err(), "tree entry name {name:?} accepted");
    }
}

#[derive(serde::Deserialize)]
struct Item {
    #[allow(dead_code)]
    id: String,
    change: Change,
}

#[test]
fn commit_log_status() {
    let c = Ctx::new();
    c.write("a.txt", "hello\n");
    c.write("sub/b.txt", "world\n");
    let out = c.must(&["status"]);
    assert!(out.contains("A a.txt") && out.contains("A sub/b.txt"), "{out}");
    c.must(&[
        "commit",
        "-m",
        "first",
        "--intent",
        "bootstrap",
        "--agent",
        "claude",
        "--model",
        "m1",
        "--check",
        "tests=pass",
    ]);
    assert!(c.must(&["status"]).contains("working tree clean"));
    assert!(c.sp(&["commit", "-m", "again"]).is_err(), "empty commit should fail");
    c.write("a.txt", "hello there\n");
    c.must(&["commit", "-m", "second"]);

    let items: Vec<Item> = serde_json::from_str(&c.must(&["log", "--json"])).unwrap();
    assert_eq!(items.len(), 2);
    let m = &items[1].change.meta;
    assert!(m.intent == "bootstrap" && m.checks["tests"] == "pass" && m.agent == "claude", "metadata lost: {m:?}");
    let out = c.must(&["log", "--agent", "claude"]);
    assert!(out.contains("first") && !out.contains("second"), "agent filter: {out}");
    let out = c.must(&["show", "HEAD"]);
    assert!(out.contains("-hello\n") && out.contains("+hello there\n"), "show: {out}");
    assert!(c.must(&["diff", "HEAD~1", "HEAD"]).contains("+hello there"));
}

#[test]
fn ignore() {
    let c = Ctx::new();
    c.write(".sprigignore", "*.log\nbuild/\n");
    c.write("x.log", "noise");
    c.write("build/out.bin", "noise");
    c.write("keep.txt", "keep");
    c.must(&["commit", "-m", "x"]);
    let out = c.must(&["show"]);
    assert!(
        !out.contains("added: x.log") && !out.contains("build/out.bin") && out.contains("added: keep.txt"),
        "{out}"
    );
}

#[test]
fn branch_checkout_and_exec_bit() {
    let c = Ctx::new();
    c.write("f.txt", "base\n");
    c.write("run.sh", "#!/bin/sh\n");
    chmod(&c.path("run.sh"), 0o755);
    c.must(&["commit", "-m", "base"]);
    c.must(&["checkout", "-b", "feature"]);
    c.write("f.txt", "feature\n");
    c.write("new.txt", "new\n");
    std::fs::remove_file(c.path("run.sh")).unwrap();
    c.must(&["commit", "-m", "feature work"]);
    c.must(&["checkout", "main"]);
    assert_eq!(c.read("f.txt"), "base\n");
    assert!(!c.exists("new.txt"), "new.txt should be gone on main");
    assert!(is_exec(&c.path("run.sh")), "exec bit lost");
    // dirty tree blocks checkout; -f discards
    c.write("f.txt", "dirty\n");
    assert!(c.sp(&["checkout", "feature"]).is_err(), "checkout should refuse a dirty tree");
    c.must(&["checkout", "-f", "feature"]);
    assert_eq!(c.read("f.txt"), "feature\n");
    assert!(c.must(&["status"]).contains("clean"));
}

#[test]
fn merge_conflict_resolve_undo() {
    let c = Ctx::new();
    c.write("poem.txt", "line1\nline2\nline3\n");
    c.write("other.txt", "same\n");
    c.must(&["commit", "-m", "base"]);
    c.must(&["checkout", "-b", "side"]);
    c.write("poem.txt", "line1\nSIDE\nline3\n");
    c.write("other.txt", "side only\n");
    c.must(&["commit", "-m", "side edit"]);
    c.must(&["checkout", "main"]);
    c.write("poem.txt", "line1\nMAIN\nline3\n");
    c.must(&["commit", "-m", "main edit"]);

    let f = c.sp(&["merge", "side"]).unwrap_err();
    assert_eq!(f.code, 3, "expected exit 3 for a conflicted merge: {f}");
    assert!(f.out.contains("CONFLICT poem.txt"), "{f}");
    let got = c.read("poem.txt");
    assert!(got.contains("<<<<<<< main") && got.contains(">>>>>>> side"), "no markers: {got:?}");
    assert_eq!(c.read("other.txt"), "side only\n");
    // the conflict is data: it is part of HEAD and visible without touching files
    assert!(c.must(&["conflicts"]).starts_with("poem.txt"));
    assert!(c.must(&["status"]).contains("C poem.txt"));
    // committing without touching the file keeps the conflict
    c.must(&["commit", "--allow-empty", "-m", "wip"]);
    assert!(c.must(&["conflicts"]).starts_with("poem.txt"), "conflict should carry forward");
    c.must(&["resolve", "--theirs", "poem.txt"]);
    assert_eq!(c.read("poem.txt"), "line1\nSIDE\nline3\n");
    c.must(&["commit", "-m", "resolved"]);
    assert_eq!(c.must(&["conflicts"]), "");
    // undo the resolve commit, then redo it by undoing the undo
    c.must(&["undo"]);
    assert!(c.must(&["conflicts"]).starts_with("poem.txt"), "undo should bring the conflict back");
    assert!(c.read("poem.txt").contains("SIDE"), "working tree should be restored by undo");
    c.must(&["undo"]);
    assert_eq!(c.must(&["conflicts"]), "", "redo should clear conflicts again");
    let ops = c.must(&["ops"]);
    assert!(ops.contains("merge side") && ops.contains("undo"), "{ops}");
}

#[test]
fn fast_forward_and_up_to_date() {
    let c = Ctx::new();
    c.write("a", "1\n");
    c.must(&["commit", "-m", "one"]);
    c.must(&["checkout", "-b", "f"]);
    c.write("a", "2\n");
    c.must(&["commit", "-m", "two"]);
    c.must(&["checkout", "main"]);
    assert!(c.must(&["merge", "f"]).contains("fast-forward"));
    assert_eq!(c.read("a"), "2\n");
    assert!(c.must(&["merge", "f"]).contains("up to date"));
}

#[test]
fn delete_vs_modify_conflict() {
    let c = Ctx::new();
    c.write("a", "1\n");
    c.write("b", "keep\n");
    c.must(&["commit", "-m", "base"]);
    c.must(&["checkout", "-b", "del"]);
    std::fs::remove_file(c.path("a")).unwrap();
    c.must(&["commit", "-m", "delete a"]);
    c.must(&["checkout", "main"]);
    c.write("a", "modified\n");
    c.must(&["commit", "-m", "modify a"]);
    assert!(c.sp(&["merge", "del"]).is_err(), "expected conflict");
    assert_eq!(c.read("a"), "modified\n");
    c.must(&["resolve", "--theirs", "a"]); // theirs deleted it
    assert!(!c.exists("a"));
}

#[test]
fn audit() {
    let c = Ctx::new();
    c.write("a", "1\n");
    c.must(&["commit", "-m", "human change"]);
    c.write("a", "2\n");
    c.must(&["commit", "-m", "agent unreviewed", "--agent", "bot"]);
    c.write("a", "3\n");
    c.must(&["commit", "-m", "agent reviewed", "--agent", "bot", "--check", "review=approved"]);
    let f = c.sp(&["audit", "--json"]).unwrap_err();
    assert_eq!(f.code, 1);
    let found: Vec<BTreeMap<String, String>> = serde_json::from_str(&f.out).unwrap();
    assert!(found.len() == 1 && found[0]["message"] == "agent unreviewed", "{}", f.out);
}

#[test]
fn renames() {
    let c = Ctx::new();
    c.write("old.txt", "same content\n");
    c.must(&["commit", "-m", "one"]);
    std::fs::rename(c.path("old.txt"), c.path("new.txt")).unwrap();
    assert!(c.must(&["status"]).contains("R old.txt -> new.txt"));
    c.must(&["commit", "-m", "rename"]);
    assert!(c.must(&["show"]).contains("renamed: old.txt -> new.txt"));
    c.write("big.go", "package x\n\nfunc A() {}\nfunc B() {}\nfunc C() {}\nfunc D() {}\n");
    c.must(&["commit", "-m", "add big"]);
    std::fs::remove_file(c.path("big.go")).unwrap();
    c.write("moved.go", "package x\n\nfunc A() {}\nfunc B() {}\nfunc C() {}\nfunc E() {}\n");
    let out = c.must(&["status"]);
    assert!(out.contains("R big.go -> moved.go (edited)"), "rename with edit not detected: {out}");
    assert!(c.must(&["diff"]).contains("+func E() {}"));
    c.must(&["commit", "-m", "move and edit"]);
    let out = c.must(&["show"]);
    assert!(out.contains("renamed: big.go -> moved.go") && out.contains("+func E() {}"), "{out}");
}

#[test]
fn criss_cross_merge() {
    let c = Ctx::new();
    c.write("f", "1\n2\n3\n");
    c.must(&["commit", "-m", "base"]);
    c.must(&["checkout", "-b", "y"]);
    c.write("f", "1\n2\ny\n");
    c.must(&["commit", "-m", "y1"]);
    c.must(&["checkout", "main"]);
    c.write("f", "x\n2\n3\n");
    c.must(&["commit", "-m", "x1"]);
    let x1 = c.head();
    c.must(&["merge", "--no-ff", "y"]); // main: x,2,y
    c.must(&["checkout", "y"]);
    c.must(&["merge", "--no-ff", &x1]); // y: x,2,y, via the other parent order
    c.write("f", "x\n2\nyy\n");
    c.must(&["commit", "-m", "y3"]);
    c.must(&["checkout", "main"]);
    c.write("f", "xx\n2\ny\n");
    c.must(&["commit", "-m", "x3"]);
    // either single base conflicts here; the merged virtual base does not
    c.must(&["merge", "y"]);
    assert_eq!(c.read("f"), "xx\n2\nyy\n");
}

#[test]
fn commit_paths() {
    let c = Ctx::new();
    c.write("a.txt", "a\n");
    c.write("d/b.txt", "b\n");
    c.must(&["commit", "-m", "only d", "d"]);
    let r = c.repo();
    let flat = r.change_flat(&c.head()).unwrap();
    assert!(flat.len() == 1 && flat.contains_key("d/b.txt"), "path commit took {flat:?}");
    std::fs::remove_file(c.path("d/b.txt")).unwrap();
    c.write("a.txt", "a2\n");
    c.must(&["commit", "-m", "drop b", "d/b.txt"]);
    assert!(r.change_flat(&c.head()).unwrap().is_empty(), "deletion not committed alone");
    assert!(c.must(&["status"]).contains("A a.txt"), "a.txt should still be uncommitted");
}

#[test]
fn checkout_new_branch_keeps_work() {
    let c = Ctx::new();
    c.write("a.txt", "a\n");
    c.must(&["commit", "-m", "one"]);
    c.write("a.txt", "edited\n");
    c.must(&["checkout", "-b", "topic"]);
    assert_eq!(c.read("a.txt"), "edited\n");
    let out = c.must(&["status"]);
    assert!(out.contains("On branch topic") && out.contains("M a.txt"), "{out}");
}

#[test]
fn commit_hunks() {
    let c = Ctx::new();
    let orig: String = (0..20).map(|i| format!("{i}\n")).collect();
    c.write("f.txt", &orig);
    c.must(&["commit", "-m", "base"]);
    let edited = orig.replacen("1\n", "ONE\n", 1).replacen("18\n", "EIGHTEEN\n", 1);
    c.write("f.txt", &edited);
    let out = c.must(&["diff"]);
    assert!(out.contains("hunk 1") && out.contains("hunk 2"), "{out}");
    c.must(&["commit", "-m", "second hunk only", "--hunk", "f.txt:2"]);
    let r = c.repo();
    let flat = r.change_flat(&c.head()).unwrap();
    assert_eq!(&*r.get_blob(&flat["f.txt"].id).unwrap(), orig.replacen("18\n", "EIGHTEEN\n", 1).as_bytes());
    assert_eq!(c.read("f.txt"), edited, "working file changed by a hunk commit");
    assert!(c.must(&["status"]).contains("M f.txt"));
    assert!(c.sp(&["commit", "-m", "x", "--hunk", "f.txt:5"]).is_err(), "missing hunk accepted");
}

#[test]
fn go_aware_merge_end_to_end() {
    let base = "package x\n\nimport \"strings\"\n\n// A says hi.\nfunc A() string { return strings.ToUpper(\"a\") }\n\nfunc B() int { return 1 }\n";
    let ours = "package x\n\nimport (\n\t\"fmt\"\n\t\"strings\"\n)\n\n// A says hi.\nfunc A() string { return fmt.Sprint(strings.ToUpper(\"a\")) }\n\nfunc B() int { return 1 }\n\nfunc C() {}\n";
    let theirs = "package x\n\nimport (\n\t\"os\"\n\t\"strings\"\n)\n\n// A says hi.\nfunc A() string { return strings.ToUpper(\"a\") }\n\nfunc B() int { return len(os.Args) }\n\nfunc D() {}\n";
    let c = Ctx::new();
    c.write("x.go", base);
    c.must(&["commit", "-m", "base"]);
    c.must(&["checkout", "-b", "other"]);
    c.write("x.go", theirs);
    c.must(&["commit", "-m", "theirs"]);
    c.must(&["checkout", "main"]);
    c.write("x.go", ours);
    c.must(&["commit", "-m", "ours"]);
    c.must(&["merge", "other"]);
    let got = c.read("x.go");
    assert!(got.contains("func D()") && got.contains("func C()") && got.contains("\"os\""), "{got}");
}

#[test]
fn rename_aware_merge() {
    let c = Ctx::new();
    let body: String = (0..30).map(|i| format!("line {i}\n")).collect();
    c.write("lib/util.txt", &body);
    c.write("lib/other.txt", "other\n");
    c.must(&["commit", "-m", "base"]);
    c.must(&["checkout", "-b", "edit"]);
    c.write("lib/util.txt", &body.replacen("line 5\n", "line FIVE\n", 1));
    c.write("lib/new.txt", "added on the edit branch\n");
    c.must(&["commit", "-m", "edit util, add to lib"]);
    c.must(&["checkout", "main"]);
    std::fs::create_dir_all(c.path("pkg")).unwrap();
    std::fs::rename(c.path("lib/util.txt"), c.path("pkg/util.txt")).unwrap();
    std::fs::rename(c.path("lib/other.txt"), c.path("pkg/other.txt")).unwrap();
    c.write("pkg/util.txt", &body.replacen("line 25\n", "line 25!\n", 1));
    c.must(&["commit", "-m", "move lib to pkg, tweak"]);
    c.must(&["merge", "edit"]);
    let got = c.read("pkg/util.txt");
    assert!(got.contains("line FIVE") && got.contains("line 25!"), "edit not carried into the renamed file:\n{got}");
    assert!(!c.exists("lib/util.txt"), "old path came back");
    assert_eq!(c.read("pkg/new.txt"), "added on the edit branch\n");
    assert!(c.must(&["status", "-M", "0"]).contains("clean"));
}

#[test]
fn rename_detection_scales() {
    let c = Ctx::new();
    for i in 0..1000 {
        c.write(&format!("old/f{i}.txt"), &format!("file {i}\nshared header\n{}\n", format!("{i} ").repeat(20)));
    }
    c.must(&["commit", "-m", "many"]);
    let r = c.repo();
    let a = r.change_flat(&c.head()).unwrap();
    let mut b: Flat = a.iter().map(|(p, fe)| (format!("new/{}", &p[4..]), fe.clone())).collect();
    for i in (0..1000).step_by(2) {
        // half of them edited as well
        let p = format!("new/f{i}.txt");
        let mut data = r.get_blob(&b[&p].id).unwrap().to_vec();
        data.extend_from_slice(b"edited\n");
        b.insert(p, FileEnt::new(Mode::File, r.put_blob(&data).unwrap()));
    }
    let start = std::time::Instant::now();
    let ds = r.with_renames(sprig::tree::diff_flat(&a, &b));
    let took = start.elapsed();
    let renamed = ds.iter().filter(|d| d.kind == 'R' && d.from[4..] == d.path[4..]).count();
    assert_eq!(renamed, 1000);
    assert!(took.as_millis() < 300, "rename detection took {took:?}");
}

#[test]
fn diff_trees_matches_flat() {
    let c = Ctx::new();
    let r = c.repo();
    let mk = |files: &[(&str, &str)]| -> (String, Flat) {
        let f: Flat = files
            .iter()
            .map(|(p, d)| (p.to_string(), FileEnt::new(Mode::File, r.put_blob(d.as_bytes()).unwrap())))
            .collect();
        (r.build_tree(&f).unwrap(), f)
    };
    let (ta, fa) =
        mk(&[("a", "1"), ("d/x", "2"), ("d/y", "3"), ("e/f/g", "4"), ("same/s", "5"), ("file-then-dir", "6")]);
    let (tb, fb) =
        mk(&[("a", "1!"), ("d/x", "2"), ("e/new", "7"), ("same/s", "5"), ("file-then-dir/inside", "8"), ("n/m", "9")]);
    assert_eq!(r.diff_trees(&ta, &tb).unwrap(), sprig::tree::diff_flat(&fa, &fb));
    assert_eq!(r.diff_trees("", &tb).unwrap().len(), fb.len());
}

/// Commits build trees incrementally; the result must equal building the
/// whole tree from scratch, for edits, adds, deletes and files replacing
/// directories and the other way round.
#[test]
fn incremental_tree_matches_full_build() {
    let c = Ctx::new();
    let r = c.repo();
    let mut flat = Flat::new();
    let mut tree = r.build_tree(&flat).unwrap();
    let mut seed = 7u64;
    let mut rnd = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let names = ["a", "b", "c", "d/e", "d/f", "d/g/h", "x/y/z", "x/y/w", "q"];
    for round in 0..200 {
        let mut edits = BTreeMap::new();
        for _ in 0..1 + rnd(4) {
            let p = names[rnd(names.len() as u64) as usize].to_string();
            if rnd(3) == 0 {
                edits.insert(p, None);
            } else {
                let fe = FileEnt::new(Mode::File, r.put_blob(format!("{round} {}", rnd(5)).as_bytes()).unwrap());
                edits.insert(p, Some(fe));
            }
        }
        // a file and a directory at one path in one edit is not a tree
        let mut next = flat.clone();
        for (p, e) in &edits {
            match e {
                Some(fe) => next.insert(p.clone(), fe.clone()),
                None => next.remove(p),
            };
        }
        let clash = next.keys().any(|p| next.keys().any(|q| q.starts_with(&format!("{p}/"))));
        if clash {
            continue;
        }
        // removing what a new file or directory replaces
        for (p, e) in edits.clone() {
            if e.is_some() {
                for q in flat.keys().filter(|q| q.starts_with(&format!("{p}/")) || p.starts_with(&format!("{q}/"))) {
                    edits.entry(q.clone()).or_insert(None);
                }
            }
        }
        tree = r.update_tree(&tree, &edits).unwrap();
        flat = next;
        assert_eq!(tree, r.build_tree(&flat).unwrap(), "round {round}: {edits:?}");
    }
}

#[test]
fn gc_packs() {
    let c = Ctx::new();
    let body: String = (0..400).map(|i| format!("{i} {}\n", hash_object("x", i.to_string().as_bytes()))).collect(); // poorly compressible
    for i in 0..15 {
        // many versions of one file, plus a revert
        c.write("big.txt", &format!("{body}{}", "edit\n".repeat(i % 7)));
        c.must(&["commit", "-m", &format!("v{i}")]);
    }
    let before = c.must(&["log", "--json", "-n", "100"]);
    let out = c.must(&["gc"]);
    let nums: Vec<u64> = out[out.find(": ").unwrap() + 2..]
        .split(" KB")
        .take(2)
        .map(|s| s.trim_start_matches(" -> ").trim().parse().unwrap())
        .collect();
    assert!(out.contains("as deltas") && nums[1] * 3 <= nums[0], "gc did not shrink storage much: {out}");
    let r = c.repo();
    assert_eq!(std::fs::read_dir(r.dir.join("objects")).unwrap().count(), 0, "loose objects left");
    assert_eq!(c.must(&["log", "--json", "-n", "100"]), before, "history reads differently after gc");
    assert!(c.must(&["status"]).contains("clean"));
    let head = c.head();
    assert_eq!(r.resolve_rev(&head[..10]).unwrap(), head, "short id of a packed change");
    c.write("big.txt", "rewritten\n");
    c.must(&["commit", "-m", "after gc"]);
    c.must(&["gc"]);
    assert_eq!(c.must(&["show"]).matches("+rewritten").count(), 1);
}

#[test]
fn pack_formats() {
    // a v1 (zlib) pack written by older sprig stays readable, and repack converts it
    let c = Ctx::new();
    c.write("a.txt", "v1 pack content\n");
    c.must(&["commit", "-m", "one"]);
    let r = c.repo();
    let head = c.head();
    let c1 = r.get_change(&head).unwrap();
    let mut ids = vec![];
    for d in std::fs::read_dir(r.dir.join("objects")).unwrap() {
        let d = d.unwrap();
        for f in std::fs::read_dir(d.path()).unwrap() {
            ids.push(format!("{}{}", d.file_name().to_str().unwrap(), f.unwrap().file_name().to_str().unwrap()));
        }
    }
    ids.sort();
    let (mut pk, mut idx) = (vec![], vec![]);
    for id in &ids {
        let (typ, data) = r.get_local(id).unwrap();
        idx.extend_from_slice(&sprig::util::raw_id(id).unwrap());
        idx.extend_from_slice(&(pk.len() as u64).to_be_bytes());
        let mut z = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
        std::io::Write::write_all(&mut z, format!("{typ} - {}\0", data.len()).as_bytes()).unwrap();
        std::io::Write::write_all(&mut z, &data).unwrap();
        pk.extend(z.finish().unwrap());
    }
    std::fs::create_dir_all(r.pack_dir()).unwrap();
    std::fs::write(r.pack_dir().join("pack-old.pack"), pk).unwrap();
    std::fs::write(r.pack_dir().join("pack-old.idx"), idx).unwrap();
    std::fs::remove_dir_all(r.dir.join("objects")).unwrap();
    std::fs::create_dir_all(r.dir.join("objects")).unwrap();
    assert_eq!(c.repo().get_change(&head).unwrap().tree, c1.tree, "v1 pack unreadable");
    for i in 0..10 {
        // more packs than REPACK_AT: the next gc merges them
        c.write("a.txt", &format!("version {i}\n"));
        c.must(&["commit", "-m", &i.to_string()]);
        c.must(&["gc"]);
    }
    assert!(c.repo().pack_count() <= 2, "gc should have merged the packs (REPACK_AT {REPACK_AT})");
    c.must(&["gc", "--repack"]);
    assert_eq!(c.repo().pack_count(), 1);
    assert_eq!(c.must(&["log", "-n", "20"]).matches("change ").count(), 11, "history lost in repack");
}

#[test]
fn auto_gc() {
    let c = Ctx::new();
    c.setenv("SPRIG_NO_AUTOGC", "");
    c.write("a.txt", "a");
    c.must(&["commit", "-m", "small"]);
    let r = c.repo();
    assert_eq!(r.pack_count(), 0, "auto gc ran for a tiny repo");
    let fan = r.dir.join("objects").join("17");
    std::fs::create_dir_all(&fan).unwrap();
    for i in 0..AUTO_GC_LOOSE / 256 + 1 {
        // fake a full fan-out dir; they are not valid ids, so gc skips them
        std::fs::write(fan.join(format!("x{i}")), "").unwrap();
    }
    c.write("a.txt", "b");
    c.must(&["commit", "-m", "trigger"]);
    wait_for("background gc", 10, || r.pack_count() == 1 && !r.dir.join("gc.lock").exists());
}

#[test]
fn import_git() {
    if !has_git() {
        return;
    }
    let mut c = Ctx::new();
    let g = c.temp();
    git(&g, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(g.join("src")).unwrap();
    std::fs::write(g.join("README.md"), "# hi\n").unwrap();
    std::fs::write(g.join("src/main.go"), "package main\n").unwrap();
    std::fs::write(g.join("run.sh"), "#!/bin/sh\n").unwrap();
    chmod(&g.join("run.sh"), 0o755);
    std::os::unix::fs::symlink("README.md", g.join("link")).unwrap();
    git(&g, &["add", "."]);
    git(&g, &["commit", "-q", "-m", "initial\n\nbody text"]);
    git(&g, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(g.join("feature.txt"), "feat\n").unwrap();
    git(&g, &["add", "."]);
    git(&g, &["commit", "-q", "-m", "feature"]);
    git(&g, &["checkout", "-q", "main"]);
    std::fs::write(g.join("src/main.go"), "package main\n// main\n").unwrap();
    git(&g, &["commit", "-q", "-am", "main change"]);
    git(&g, &["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
    git(&g, &["rm", "-q", "README.md"]);
    git(&g, &["commit", "-q", "-m", "drop readme"]);

    let gs = g.to_str().unwrap();
    assert!(c.must(&["import-git", gs]).contains("imported 5 changes"));
    // HEAD adopted and checked out
    assert_eq!(c.read("feature.txt"), "feat\n");
    assert_eq!(c.read("src/main.go"), "package main\n// main\n");
    assert!(!c.exists("README.md"), "README.md should be gone at tip");
    assert!(is_exec(&c.path("run.sh")), "exec bit not preserved");
    assert_eq!(std::fs::read_link(c.path("link")).unwrap().to_str(), Some("README.md"));
    // history shape: the merge change has two parents; metadata kept
    let log = c.must(&["log", "--json", "-n", "10"]);
    let items: Vec<Item> = serde_json::from_str(&log).unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(items.iter().filter(|it| it.change.parents.len() == 2).count(), 1);
    for it in &items {
        let ch = &it.change;
        assert!(
            ch.author == "Gina <gina@example.com>" && !ch.meta.git_sha.is_empty() && ch.time == 1709290800,
            "{ch:?}"
        );
    }
    assert_eq!(items[4].change.message, "initial\n\nbody text");
    assert!(c.must(&["branch"]).contains("feature"));
    c.must(&["import-git", gs]);
    assert_eq!(c.must(&["log", "--json", "-n", "10"]), log, "re-import changed history (should be deterministic)");
    let files = git(&g, &["ls-files"]);
    assert_eq!(c.repo().change_flat(&c.head()).unwrap().len(), files.split_whitespace().count());
    assert!(c.must(&["status"]).contains("clean"));
}

#[test]
fn export_git_round_trip() {
    if !has_git() {
        return;
    }
    let mut c = Ctx::new();
    // "github" is a bare repo with one commit; clone is the local transport.
    let (hub, seed, tmp) = (c.temp(), c.temp(), c.temp());
    git(&hub, &["init", "-q", "--bare", "-b", "main"]);
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("a.txt"), "one\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    git(&seed, &["push", "-q", hub.to_str().unwrap(), "main"]);
    let seed_sha = git(&seed, &["rev-parse", "HEAD"]);
    let clone = tmp.join("clone.git");
    git(&tmp, &["clone", "-q", "--bare", hub.to_str().unwrap(), clone.to_str().unwrap()]);
    let cs = clone.to_str().unwrap();

    c.must(&["import-git", cs]);
    c.write("a.txt", "one\ntwo\n");
    c.write("dir/b.sh", "#!/bin/sh\n");
    chmod(&c.path("dir/b.sh"), 0o755);
    c.must(&["commit", "-m", "from sprig", "--intent", "add two", "--agent", "claude", "--check", "tests=pass"]);
    assert!(c.must(&["export-git", cs]).contains("wrote 1 commit(s)"));
    let tip = git(&clone, &["rev-parse", "main"]);
    assert_eq!(git(&clone, &["rev-parse", "main^"]), seed_sha, "imported history not reused");
    assert_eq!(git(&clone, &["show", "main:a.txt"]), "one\ntwo");
    assert!(git(&clone, &["ls-tree", "-r", "main", "dir/b.sh"]).starts_with("100755"));
    let msg = git(&clone, &["log", "-1", "--format=%B", "main"]);
    for want in ["Sprig-Intent: add two", "Sprig-Agent: claude", "Sprig-Check: tests=pass", "Sprig-Change: "] {
        assert!(msg.contains(want), "message lacks {want:?}:\n{msg}");
    }
    assert!(c.must(&["export-git", cs]).contains("wrote 0 commit(s)"), "re-export rewrote known commits");
    assert_eq!(git(&clone, &["rev-parse", "main"]), tip, "re-export changed the commit");
    assert_eq!(git(&clone, &["for-each-ref", sprig::git::EXPORT_REF]), "", "scratch ref left behind");
    git(&clone, &["push", "-q", "origin", "main"]);

    // someone else builds on it in git; importing again fast-forwards
    let other = c.temp();
    git(&other, &["clone", "-q", hub.to_str().unwrap(), "."]);
    std::fs::write(other.join("c.txt"), "c\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "from git"]);
    git(&other, &["push", "-q", "origin", "main"]);
    git(&clone, &["fetch", "-q", "origin", "+refs/heads/*:refs/heads/*"]);
    let mine = c.head();
    let out = c.must(&["import-git", cs]);
    assert!(!out.contains("warning"), "import did not fast-forward: {out}");
    let r = c.repo();
    let new_tip = r.read_ref("heads/main").unwrap();
    let ch = r.get_change(&new_tip).unwrap();
    assert_eq!(ch.parents, vec![mine], "exported commit not mapped back to its change");
    assert_eq!(c.read("c.txt"), "c\n", "working tree not moved with the fast-forwarded branch");

    // a conflicted tip is not exported
    c.must(&["checkout", "-b", "x"]);
    let bad = Change {
        parents: vec![new_tip],
        tree: ch.tree.clone(),
        author: "T <t@x>".into(),
        message: "bad".into(),
        conflicts: vec![Conflict { path: "a.txt".into(), ..Default::default() }],
        ..Default::default()
    };
    r.write_ref("heads/x", &r.put_change(&bad).unwrap()).unwrap();
    assert!(c.must(&["export-git", cs]).contains("branch \"x\" has 1 unresolved"));
}

#[test]
fn import_git_diverged() {
    if !has_git() {
        return;
    }
    let mut c = Ctx::new();
    let g = c.temp();
    let gs = g.to_str().unwrap().to_string();
    git(&g, &["init", "-q", "-b", "main"]);
    std::fs::write(g.join("a.txt"), "1\n2\n").unwrap();
    git(&g, &["add", "."]);
    git(&g, &["commit", "-q", "-m", "seed"]);
    c.must(&["import-git", &gs]);
    c.write("b.txt", "sprig side\n");
    c.must(&["commit", "-m", "sprig work"]);
    std::fs::write(g.join("a.txt"), "1\n2\ngit side\n").unwrap();
    git(&g, &["commit", "-q", "-am", "git work"]);
    assert!(c.must(&["import-git", &gs]).contains("git/main"), "diverged branch not kept");
    c.must(&["merge", "git/main"]);
    assert_eq!(c.read("a.txt"), "1\n2\ngit side\n");
    assert_eq!(c.read("b.txt"), "sprig side\n");
    c.write("c.txt", "more\n");
    c.must(&["commit", "-m", "ahead of git"]);
    assert!(!c.must(&["import-git", &gs]).contains("warning"), "branch only ahead of git reported as diverged");
}

#[test]
fn bare_repo_has_no_working_tree() {
    let mut c = Ctx::bare();
    let d = c.temp();
    let r = Repo::init(&d, true, "main").unwrap();
    assert!(r.work_status(false).is_err());
}

#[test]
fn signs_with_an_ssh_key() {
    if std::process::Command::new("ssh-keygen").arg("-?").output().is_err() {
        return;
    }
    let mut c = Ctx::new();
    let dir = c.temp();
    let ssh = dir.join("id_ed25519");
    let st = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "me@example.com", "-f"])
        .arg(&ssh)
        .status()
        .unwrap();
    assert!(st.success());
    c.setenv("SPRIG_KEY", dir.join("signing.key").to_str().unwrap());
    let out = c.must(&["keygen", "--ssh", ssh.to_str().unwrap()]);
    let public = std::fs::read_to_string(dir.join("id_ed25519.pub")).unwrap();
    c.must(&["trust", public.trim(), "me"]); // the .pub line, converted
    assert!(out.contains(&c.must(&["trust"]).split('\t').nth(1).unwrap().trim().to_string()), "{out}");
    c.write("a.txt", "x\n");
    c.must(&["commit", "-m", "signed by ssh", "--agent", "bot", "--check", "review=pass"]);
    let audit = c.must(&["audit", "--signed"]);
    assert!(audit.starts_with("0 of 1"), "{audit}");
    assert!(c.must(&["log", "--signatures"]).contains("me"));
}
