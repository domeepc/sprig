//! Servers and clients: push, pull, landing, scoped tokens, caches,
//! standbys, projections, signatures and lazy clones.

mod common;

use common::*;
use sprig::remote::{LandFile, LandRequest, RefUpdate, Remote};
use sprig::server::{Scope, Server};
use sprig::sign::{LandProof, land_payload, sign};
use sprig::store::{BLOB, hash_object, short};
use std::collections::BTreeMap;
use std::sync::Arc;

#[test]
fn push_pull_and_server_policy() {
    let mut c = Ctx::new();
    let srv = bare_repo(&c.temp());
    let (s, url) = start(Server::new(srv, "secret"));
    let repo = s.repo.clone();
    c.setenv("SPRIG_TOKEN", "secret");

    // client A
    let a = c.dir();
    c.must(&["remote", "add", "origin", &url]);
    c.write("a.txt", "from A\n");
    c.write("d/x.txt", "nested\n");
    c.must(&["commit", "-m", "A1"]);
    assert!(c.must(&["push"]).contains("pushed main"));
    assert!(c.must(&["push"]).contains("up to date"));
    assert!(repo.read_ref("heads/main").is_some(), "server ref missing");

    // server-side agent API: read a file without any clone
    let rm = c.remote(&url);
    assert_eq!(rm.call("GET", "/v1/file?ref=main&path=d/x.txt", &[], &[]).unwrap().data, b"nested\n");
    let log = rm.call("GET", "/v1/log?ref=main&limit=5", &[], &[]).unwrap().data;
    assert!(String::from_utf8_lossy(&log).contains("A1"));
    // a wrong token is rejected
    let mut bad = Remote::new("t", &url);
    bad.token = "nope".into();
    assert!(bad.refs().is_err(), "bad token accepted");

    // client B pulls into an unborn branch: adopts the remote history
    let b = c.temp();
    c.cd(&b);
    c.must(&["init"]);
    c.must(&["remote", "add", "origin", &url]);
    c.must(&["pull"]);
    assert_eq!(c.read("a.txt"), "from A\n");
    assert_eq!(c.read("d/x.txt"), "nested\n");
    c.write("a.txt", "from B\n");
    c.must(&["commit", "-m", "B1"]);
    c.must(&["push"]);

    // A is behind and edits independently: push rejected (non-fast-forward)
    c.cd(&a);
    c.write("other.txt", "A side\n");
    c.must(&["commit", "-m", "A2"]);
    let f = c.sp(&["push"]).unwrap_err();
    assert!(f.err.contains("non-fast-forward"), "{f}");
    // pull merges B's work, then push succeeds
    c.must(&["pull"]);
    assert_eq!(c.read("a.txt"), "from B\n");
    assert_eq!(c.read("other.txt"), "A side\n");
    c.must(&["push"]);

    // a conflicted tip is refused by the server's policy gate
    c.cd(&b);
    c.must(&["pull"]);
    c.write("a.txt", "B conflicting\n");
    c.must(&["commit", "-m", "B2"]);
    c.cd(&a);
    c.write("a.txt", "A conflicting\n");
    c.must(&["commit", "-m", "A3"]);
    c.must(&["push"]); // A wins the race
    c.cd(&b);
    c.must(&["fetch"]);
    assert!(c.sp(&["merge", "origin/main"]).is_err(), "expected conflict");
    let f = c.sp(&["push"]).unwrap_err();
    assert!(f.err.contains("unresolved conflict"), "server should refuse a conflicted tip: {f}");
    c.must(&["resolve", "--ours", "a.txt"]);
    c.must(&["commit", "-m", "resolved"]);
    c.must(&["push"]);
}

#[test]
fn server_rejects_bad_objects() {
    let mut c = Ctx::bare();
    let (_s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    let rm = Remote::new("t", &url);
    let good = hash_object(BLOB, b"data");
    assert!(rm.put_object(&good, BLOB, b"different").is_err(), "hash mismatch accepted");
    rm.put_object(&good, BLOB, b"data").unwrap();
    // a tree referencing a blob the server does not have
    let tree = format!(r#"{{"entries":[{{"name":"f","mode":"file","id":"{}"}}]}}"#, hash_object(BLOB, b"absent"));
    let e = rm.put_object(&hash_object("tree", tree.as_bytes()), "tree", tree.as_bytes()).unwrap_err();
    assert!(e.to_string().contains("missing dependency"), "{e}");
    // ref names that try to escape
    for name in ["../../etc/passwd", "heads/.hidden", "a//b"] {
        let body = sprig::util::to_json(&RefUpdate::default());
        assert!(rm.call("POST", &format!("/v1/refs/{name}"), &body, &[]).is_err(), "ref name {name:?} accepted");
    }
    assert!(rm.update_ref("heads/x", "", &good, false).is_err(), "ref pointing at a blob accepted");
}

#[test]
fn land_rebases_and_retries() {
    let mut c = Ctx::new();
    let (s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    let repo = s.repo.clone();
    let a = c.dir();
    c.must(&["remote", "add", "origin", &url]);
    c.write("a.txt", "1\n2\n3\n");
    c.write("b.txt", "b\n");
    c.must(&["commit", "-m", "seed"]);
    c.must(&["push"]);
    let b = c.temp();
    c.cd(&b);
    c.must(&["init"]);
    c.must(&["remote", "add", "origin", &url]);
    c.must(&["pull"]);

    // A lands first; B, still on the old base, lands an unrelated edit and is rebased
    c.cd(&a);
    c.write("a.txt", "1\nA\n3\n");
    c.must(&["land", "-m", "A edit", "--agent", "claude", "--check", "tests=pass"]);
    let a_tip = repo.read_ref("heads/main").unwrap();
    c.cd(&b);
    c.write("b.txt", "B\n");
    let out = c.must(&["land", "-m", "B edit"]);
    assert!(out.contains(&format!("landed on {}", short(&a_tip))), "B not rebased onto A: {out}");
    assert_eq!(c.read("a.txt"), "1\nA\n3\n");
    assert_eq!(c.read("b.txt"), "B\n");
    let st = c.must(&["status"]);
    assert!(!st.contains("a.txt") && !st.contains("b.txt"), "working tree dirty after land: {st}");
    let tip = repo.read_ref("heads/main").unwrap();
    assert_eq!(c.repo().read_ref("heads/main").unwrap(), tip, "local branch not at landed change");
    let ch = repo.get_change(&a_tip).unwrap();
    assert!(ch.meta.agent == "claude" && ch.meta.checks["tests"] == "pass", "provenance lost: {:?}", ch.meta);

    // A is stale and edits b.txt, which B changed since A's base: refused
    c.cd(&a);
    c.write("b.txt", "A wants b\n");
    let f = c.sp(&["land", "-m", "clash"]).unwrap_err();
    assert!(f.err.contains("b.txt"), "expected conflict on b.txt: {f}");
    assert_eq!(repo.read_ref("heads/main").unwrap(), tip, "conflicting land moved the branch");

    // clone-free agents: 20 concurrent landings from the same stale base all land
    let rm = Arc::new(Remote::new("t", &url));
    let hs: Vec<_> = (0..20)
        .map(|i| {
            let (rm, base) = (rm.clone(), a_tip.clone());
            std::thread::spawn(move || {
                rm.land(&LandRequest {
                    branch: "main".into(),
                    base,
                    message: format!("agent {i}"),
                    files: BTreeMap::from([(format!("agents/{i}.txt"), file(b"hi\n"))]),
                    ..Default::default()
                })
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap().unwrap();
    }
    let end = repo.read_ref("heads/main").unwrap();
    assert_eq!(repo.change_flat(&end).unwrap().len(), 22, "files after concurrent landings");
    let mut n = 0;
    repo.walk(&end, |_, c| {
        n += 1;
        c.parents.len() <= 1
    })
    .unwrap();
    assert_eq!(n, 23, "seed, A, B, 20 agents; all linear");
}

#[test]
fn land_paths() {
    let mut c = Ctx::bare();
    let (s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    seed(&c, &url);
    let f = || file(b"y\n");
    assert!(land(&url, "", vec![("a.txt/y", f())]).is_err(), "file used as a directory accepted");
    let e = land(&url, "", vec![("d", f())]).unwrap_err();
    assert!(e.to_string().contains("directory"), "directory overwritten by a file: {e}");
    land(&url, "", vec![("d/x.txt", None), ("e/f/g.txt", f())]).unwrap();
    let tip = s.repo.read_ref("heads/main").unwrap();
    let flat = s.repo.change_flat(&tip).unwrap();
    assert!(!flat.contains_key("d/x.txt") && flat.len() == 2 && flat.contains_key("e/f/g.txt"), "{flat:?}");
    let root = s.repo.get_tree(&s.repo.get_change(&tip).unwrap().tree).unwrap();
    assert_eq!(root.entries.len(), 2, "empty d/ should be dropped");
}

#[test]
fn scoped_tokens() {
    let mut c = Ctx::bare();
    let mut srv = Server::new(bare_repo(&c.temp()), "full-access-token");
    srv.scopes = BTreeMap::from([
        ("docs-only-token-123".to_string(), Scope { write: vec!["d".into()], read: None }),
        ("read-d-only-token-12".to_string(), Scope { write: vec![], read: Some(vec!["d".into()]) }),
    ]);
    c.setenv("SPRIG_TOKEN", "full-access-token");
    let (s, url) = start(srv);
    seed(&c, &url);
    let f = || file(b"new\n");
    land(&url, "docs-only-token-123", vec![("d/x.txt", f())]).unwrap();
    let e = land(&url, "docs-only-token-123", vec![("a.txt", f())]).unwrap_err();
    assert!(e.to_string().contains("403"), "out-of-scope land: {e}");
    assert!(land(&url, "docs-only-token-123", vec![("dx.txt", f())]).is_err(), "prefix matched a sibling name");
    let mut rm = Remote::new("t", &url);
    rm.token = "docs-only-token-123".into();
    let e = rm.update_ref("heads/main", "", "", true).unwrap_err();
    assert!(e.to_string().contains("403"), "scoped token moved a ref: {e}");
    let e = land(&url, "wrong", vec![("d/x.txt", f())]).unwrap_err();
    assert!(e.to_string().contains("401"), "bad token: {e}");
    // read scope: only d/ is visible, and raw objects are off limits
    let mut ro = Remote::new("t", &url);
    ro.token = "read-d-only-token-12".into();
    assert_eq!(ro.call("GET", "/v1/file?path=d/x.txt", &[], &[]).unwrap().data, b"new\n");
    let e = ro.call("GET", "/v1/file?path=a.txt", &[], &[]).unwrap_err();
    assert!(e.to_string().contains("403"), "out-of-scope read: {e}");
    let ls = String::from_utf8(ro.call("GET", "/v1/ls", &[], &[]).unwrap().data).unwrap();
    assert!(!ls.contains("a.txt") && ls.contains("\"d\""), "ls not filtered: {ls}");
    let refs = ro.refs().unwrap();
    ro.get_object(&refs["heads/main"]).expect("projected tip not fetchable");
    let real_tip = s.repo.read_ref("heads/main").unwrap();
    for id in [real_tip, hash_object(BLOB, b"a\n")] {
        let e = ro.get_object(&id).unwrap_err();
        assert!(e.to_string().contains("403"), "read-limited token fetched out-of-scope object: {e}");
        assert!(ro.get_objects(&[id], |_, _, _| Ok(())).is_err(), "batch-fetched an out-of-scope object");
    }
    assert!(land(&url, "read-d-only-token-12", vec![("d/x.txt", f())]).is_err(), "read-only token landed");
}

#[test]
fn two_servers_one_repo() {
    let mut c = Ctx::bare();
    let dir = c.temp();
    let (s1, url1) = start(Server::new(bare_repo(&dir), ""));
    let (_s2, url2) = start(Server::new(sprig::store::Repo::bare_at(&dir), ""));
    seed(&c, &url1);
    let hs: Vec<_> = (0..20u8)
        .map(|i| {
            let url = if i % 2 == 1 { url2.clone() } else { url1.clone() };
            std::thread::spawn(move || land(&url, "", vec![(format!("n/{i}").as_str(), file(&[i]))]))
        })
        .collect();
    for h in hs {
        h.join().unwrap().unwrap();
    }
    let tip = s1.repo.read_ref("heads/main").unwrap();
    assert_eq!(s1.repo.change_flat(&tip).unwrap().len(), 22, "lost landings across servers");
}

fn cache_of(c: &mut Ctx, upstream: &str) -> (Arc<Server>, String) {
    let mut r = bare_repo(&c.temp());
    let up = Arc::new(Remote::new("upstream", upstream));
    r.fallback = Some(up.clone());
    let mut s = Server::new(r, "");
    s.upstream = Some(up);
    start(s)
}

#[test]
fn cache_server() {
    let mut c = Ctx::bare();
    let (s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    seed(&c, &url);
    let (cache, curl) = cache_of(&mut c, &url);
    let t = c.temp();
    c.cd(&t);
    c.must(&["clone", &curl, "w"]);
    c.cd("w");
    assert_eq!(c.read("d/x.txt"), "x\n", "clone through cache failed");
    let tip = s.repo.read_ref("heads/main").unwrap();
    assert!(cache.repo.has_object(&tip), "cache did not keep the change");
    c.write("a.txt", "via cache\n");
    c.must(&["land", "-m", "through cache"]);
    let new_tip = s.repo.read_ref("heads/main").unwrap();
    assert_eq!(s.repo.change_flat(&new_tip).unwrap()["a.txt"].id, hash_object(BLOB, b"via cache\n"));
    cache.stop();
}

#[test]
fn caches_land_on_separate_disks() {
    let mut c = Ctx::bare();
    let (s, inner) = start(Server::new(bare_repo(&c.temp()), ""));
    let (main, seen) = counting_proxy(&inner);
    seed(&c, &main);
    seen.lock().unwrap().clear();
    let caches: Vec<_> = (0..2).map(|_| cache_of(&mut c, &main)).collect(); // each with its own, initially empty, store
    let urls: Vec<String> = caches.iter().map(|(_, u)| u.clone()).collect();
    let hs: Vec<_> = (0..20u8)
        .map(|i| {
            let url = urls[i as usize % 2].clone();
            std::thread::spawn(move || land(&url, "", vec![(format!("n/{i}").as_str(), file(&[i]))]))
        })
        .collect();
    for h in hs {
        h.join().unwrap().unwrap();
    }
    let tip = s.repo.read_ref("heads/main").unwrap();
    assert_eq!(s.repo.change_flat(&tip).unwrap().len(), 22, "2 seeded files and 20 landed");
    let seen = seen.lock().unwrap();
    assert!(
        !seen.iter().any(|r| r.contains("/v1/land")),
        "a landing reached the main server; the cache should merge it"
    );
    let swaps = seen.iter().filter(|r| r.starts_with("POST /v1/refs/")).count();
    assert!(swaps >= 20, "expected one ref swap per landing, saw {swaps}");
    for (cs, _) in &caches {
        cs.stop();
    }
}

#[test]
fn lazy_sparse_clone() {
    let mut c = Ctx::bare();
    let (s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    seed(&c, &url);
    let t = c.temp();
    c.cd(&t);
    c.must(&["clone", "--path", "d", &url, "w"]);
    c.cd("w");
    assert!(!c.exists("a.txt"), "file outside --path was checked out");
    assert!(!c.repo().has_object(&hash_object(BLOB, b"a\n")), "blob outside the view was downloaded");
    assert!(!c.must(&["status"]).contains("a.txt"), "file outside the view shows as changed");
    // reading history outside the view fetches on demand
    assert!(c.must(&["show", "HEAD"]).contains("+a"), "show did not load a.txt lazily");
    c.write("d/x.txt", "x2\n");
    c.must(&["land", "-m", "sparse edit"]);
    let tip = s.repo.read_ref("heads/main").unwrap();
    let flat = s.repo.change_flat(&tip).unwrap();
    assert!(flat.len() == 2 && flat["d/x.txt"].id == hash_object(BLOB, b"x2\n"), "{flat:?}");
    // commit + push from a lazy clone works without downloading everything
    c.write("d/y.txt", "y\n");
    c.must(&["commit", "-m", "local"]);
    c.must(&["push"]);
    let tip = s.repo.read_ref("heads/main").unwrap();
    let flat = s.repo.change_flat(&tip).unwrap();
    assert!(
        flat.contains_key("a.txt") && flat.contains_key("d/y.txt"),
        "push from a sparse clone lost files: {flat:?}"
    );
}

#[test]
fn sparse_widen() {
    let mut c = Ctx::bare();
    let (_s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    seed(&c, &url);
    let t = c.temp();
    c.cd(&t);
    c.must(&["clone", "--path", "d", &url, "w"]);
    c.cd("w");
    c.must(&["sparse", "."]);
    assert_eq!(c.read("a.txt"), "a\n", "widening did not check out a.txt");
    c.must(&["sparse", "d"]);
    assert!(!c.exists("a.txt"), "narrowing did not remove a.txt");
    assert!(c.must(&["status"]).contains("clean"));
}

#[test]
fn batched_transfer() {
    let mut c = Ctx::new();
    let (_s, inner) = start(Server::new(bare_repo(&c.temp()), ""));
    let (url, seen) = counting_proxy(&inner);
    c.must(&["remote", "add", "origin", &url]);
    for i in 0..60 {
        c.write(&format!("d{}/f{i}.txt", i % 5), &i.to_string());
    }
    c.must(&["commit", "-m", "many"]);
    c.must(&["push"]);
    let b = c.temp();
    c.cd(&b);
    c.must(&["init"]);
    c.must(&["remote", "add", "origin", &url]);
    c.must(&["pull"]);
    assert_eq!(c.read("d3/f8.txt"), "8");
    let seen = seen.lock().unwrap();
    let single = seen.iter().filter(|r| r.contains("/v1/object/")).count();
    let batch = seen.iter().filter(|r| r.contains("/v1/objects/get") || r.contains("/v1/objects/put")).count();
    assert!(single == 0 && batch <= 5, "requests: {single} single, {batch} batched"); // 1 put + one get per tree level
}

#[test]
fn signed_changes() {
    let mut c = Ctx::bare();
    let (s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    seed(&c, &url);
    let key = c.path("key");
    c.setenv("SPRIG_KEY", key.to_str().unwrap());
    let out = c.must(&["keygen"]);
    let public = out.split("public key: ").nth(1).unwrap().split_whitespace().next().unwrap().to_string();
    c.must(&["trust", &public, "me"]);
    c.write("a.txt", "signed\n");
    c.setenv("SPRIG_AGENT", "claude");
    c.must(&["commit", "-m", "signed commit", "--check", "review=approved"]);
    assert!(c.must(&["show"]).contains("Signed: me (trusted)"));
    c.must(&["audit", "--signed"]);

    // a signed landing verifies on the server's change
    c.write("d/x.txt", "landed\n");
    c.must(&["push"]);
    c.write("d/x.txt", "landed again\n");
    c.must(&["land", "-m", "signed land", "--check", "review=approved"]);
    let repo = &s.repo;
    let tip = repo.read_ref("heads/main").unwrap();
    let ch = repo.get_change(&tip).unwrap();
    assert_eq!(repo.verify_change(&ch).map_err(|e| e.1.to_string()), Ok(public.clone()));
    c.must(&["audit", "--signed"]);

    // tampering is caught: same signature, different message or extra path
    let mut forged = ch.clone();
    forged.message = "not what was signed".into();
    assert!(repo.verify_change(&forged).is_err(), "altered message verified");
    let mut flat = repo.flatten(&ch.tree).unwrap();
    flat.insert("sneaky.txt".into(), flat["a.txt"].clone());
    let mut forged = ch.clone();
    forged.tree = repo.build_tree(&flat).unwrap();
    let e = repo.verify_change(&forged).unwrap_err().1.to_string();
    assert!(e.contains("unsigned path"), "extra path verified: {e}");
    // an unsigned agent change fails audit --signed
    c.setenv("SPRIG_KEY", c.path("none").to_str().unwrap());
    c.write("a.txt", "unsigned\n");
    c.must(&["commit", "-m", "unsigned", "--check", "review=approved"]);
    let f = c.sp(&["audit", "--signed"]).unwrap_err();
    assert!(f.out.contains("unsigned"), "unsigned change passed audit: {f}");
}

#[test]
fn land_follows_rename() {
    let mut c = Ctx::bare();
    let (s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    seed(&c, &url);
    let body: String = (0..30).map(|i| format!("row {i}\n")).collect();
    c.write("a.txt", &body);
    c.must(&["commit", "-m", "long a"]);
    c.must(&["push"]);
    let stale = Remote::new("t", &url).refs().unwrap()["heads/main"].clone();
    // someone renames a.txt
    std::fs::rename(c.path("a.txt"), c.path("b.txt")).unwrap();
    c.must(&["commit", "-m", "rename"]);
    c.must(&["push"]);
    // a signed landing edits a.txt against the old base
    let key = ed25519_dalek::SigningKey::from_bytes(&[0; 32]);
    let edited = body.replacen("row 3\n", "row three\n", 1);
    let proof = LandProof {
        branch: "main".into(),
        base: stale.clone(),
        files: BTreeMap::from([("a.txt".to_string(), hash_object(BLOB, edited.as_bytes()))]),
    };
    let mut lr = LandRequest {
        branch: "main".into(),
        base: stale,
        message: "edit a".into(),
        author: "x <x@y>".into(),
        files: BTreeMap::from([(
            "a.txt".to_string(),
            Some(LandFile { mode: "file".into(), data: edited.into_bytes() }),
        )]),
        ..Default::default()
    };
    let key = sprig::sign::Key::Local(key);
    lr.sig = Some(sign(&key, &land_payload(&proof, &lr.message, &lr.author, &lr.meta)).unwrap());
    let res = Remote::new("t", &url).land(&lr).expect("landing on a renamed file");
    let repo = &s.repo;
    let flat = repo.change_flat(&res.id).unwrap();
    assert!(!flat.contains_key("a.txt"), "landing resurrected the old path");
    assert!(String::from_utf8_lossy(&repo.get_blob(&flat["b.txt"].id).unwrap()).contains("row three"));
    let ch = repo.get_change(&res.id).unwrap();
    assert!(repo.verify_change(&ch).is_ok());
    assert_eq!(ch.sig.as_ref().unwrap().moved["a.txt"], "b.txt");
    let mut forged = ch.clone();
    forged.sig.as_mut().unwrap().moved = BTreeMap::from([("a.txt".into(), "x.txt".into())]);
    assert!(repo.verify_change(&forged).is_err(), "made-up move claim verified");
}

#[test]
fn standby_long_poll_reflog_failover() {
    let mut c = Ctx::bare();
    let mut srv = bare_repo(&c.temp());
    srv.durable = true;
    let (s, url) = start(Server::new(srv, ""));
    seed(&c, &url);
    let repo = s.repo.clone();
    let rm = Remote::new("t", &url);
    let (refs, etag) = rm.wait_refs("").unwrap();
    assert!(refs.contains_key("heads/main"));

    // a long-poll returns as soon as a push lands
    let (url2, etag2) = (url.clone(), etag.clone());
    let waiter = std::thread::spawn(move || Remote::new("t", &url2).wait_refs(&etag2).unwrap().0["heads/main"].clone());
    std::thread::sleep(std::time::Duration::from_millis(50));
    c.write("a.txt", "pushed\n");
    c.must(&["commit", "-m", "wake up"]);
    let start_t = std::time::Instant::now();
    c.must(&["push"]);
    let tip = waiter.join().unwrap();
    assert_eq!(tip, repo.read_ref("heads/main").unwrap(), "long-poll returned stale refs");
    assert!(start_t.elapsed().as_secs() < 2, "long-poll was slow to wake");

    // the reflog has the push and a landing
    land(&url, "", vec![("d/y.txt", file(b"y"))]).unwrap();
    let log = repo.reflog().unwrap();
    assert!(log.len() >= 2 && log[log.len() - 2].via == "push" && log[log.len() - 1].via == "land", "{log:?}");

    // failover: a dead first server is skipped
    let refs = Remote::new("t", &format!("http://127.0.0.1:1,{url}")).refs().expect("failover");
    assert!(refs.contains_key("heads/main"));

    // a standby mirrors the main server and refuses writes
    let mut sb = Server::new(bare_repo(&c.temp()), "");
    sb.follow = Some(Arc::new(Remote::new("follow", &url)));
    let (sb, sb_url) = start(sb);
    let want = repo.read_ref("heads/main").unwrap();
    wait_for("standby to catch up", 5, || sb.repo.read_ref("heads/main").as_ref() == Some(&want));
    c.must(&["pull"]); // the landing above moved main on the server
    c.write("a.txt", "after standby\n");
    c.must(&["commit", "-m", "followed"]);
    c.must(&["push"]);
    let want = repo.read_ref("heads/main").unwrap();
    wait_for("standby to follow a push", 5, || sb.repo.read_ref("heads/main").as_ref() == Some(&want));
    assert!(sb.repo.change_flat(&want).unwrap().contains_key("a.txt"), "standby lacks objects");
    let e = land(&sb_url, "", vec![("z", file(b"z"))]).unwrap_err();
    assert!(e.to_string().contains("503"), "standby accepted a landing: {e}");
    let t = c.temp();
    c.cd(&t);
    c.must(&["clone", &sb_url, "w"]);
    assert_eq!(c.read("w/a.txt"), "after standby\n", "clone from standby is stale");
    sb.stop();
    s.stop();
}

#[test]
fn projected_clone() {
    let mut c = Ctx::bare();
    let dir = c.temp();
    let scopes = BTreeMap::from([(
        "api-team-token-1234".to_string(),
        Scope { write: vec!["api".into()], read: Some(vec!["api".into()]) },
    )]);
    let mut srv = Server::new(bare_repo(&dir), "full-access-token");
    srv.scopes = scopes.clone();
    c.setenv("SPRIG_TOKEN", "full-access-token");
    let (s, url) = start(srv);
    seed(&c, &url);
    c.write("api/h.go", "package api\n");
    c.must(&["commit", "-m", "add api"]);
    c.write("secret/plan.txt", "do not leak\n");
    c.must(&["commit", "-m", "secret project codename"]);
    c.write("api/h.go", "package api\n// v2\n");
    c.must(&["commit", "-m", "api v2"]);
    c.must(&["push"]);

    c.setenv("SPRIG_TOKEN", "api-team-token-1234");
    let t = c.temp();
    c.cd(&t);
    c.must(&["clone", &url, "w"]);
    c.cd("w");
    assert_eq!(c.read("api/h.go"), "package api\n// v2\n");
    for p in ["a.txt", "secret/plan.txt", "d/x.txt"] {
        assert!(!c.exists(p), "out-of-scope {p} was cloned");
    }
    let log = c.must(&["log", "-n", "20"]);
    assert!(!log.contains("secret project") && !log.contains("seed") && log.matches("change ").count() == 2, "{log}");
    assert!(c.must(&["show"]).contains("Projected from"));
    // land from the projected clone; the real repo gets it on top of everything
    c.write("api/h.go", "package api\n// v3\n");
    c.must(&["land", "-m", "api v3"]);
    let repo = &s.repo;
    let real = repo.change_flat(&repo.read_ref("heads/main").unwrap()).unwrap();
    assert_eq!(&*repo.get_blob(&real["api/h.go"].id).unwrap(), b"package api\n// v3\n");
    assert!(real.contains_key("secret/plan.txt"), "landing from a projection broke the real tree");
    assert!(c.must(&["status"]).contains("clean"));
    // the projection is persisted: a fresh server process gives the same ids
    let mut rm = Remote::new("t", &url);
    rm.token = "api-team-token-1234".into();
    let refs1 = rm.refs().unwrap();
    let mut fresh = Server::new(sprig::store::Repo::bare_at(&dir), "full-access-token");
    fresh.scopes = scopes;
    let (_f, furl) = start(fresh);
    let mut rm2 = Remote::new("t", &furl);
    rm2.token = "api-team-token-1234".into();
    let refs2 = rm2.refs().unwrap();
    assert!(!refs1["heads/main"].is_empty());
    assert_eq!(refs1, refs2, "projection changed after reload");
}

#[test]
fn gc_and_serving_packed() {
    let mut c = Ctx::bare();
    let (s, url) = start(Server::new(bare_repo(&c.temp()), ""));
    seed(&c, &url);
    c.write("big.txt", &"line\n".repeat(500));
    c.must(&["commit", "-m", "big"]);
    c.must(&["gc"]);
    c.write("big.txt", "rewritten\n");
    c.must(&["commit", "-m", "after gc"]);
    c.must(&["gc"]);
    c.must(&["push"]);
    s.repo.gc(false).unwrap();
    let t = c.temp();
    c.cd(&t);
    c.must(&["clone", &url, "w"]);
    assert_eq!(c.read("w/big.txt"), "rewritten\n", "clone from a packed server failed");
}
