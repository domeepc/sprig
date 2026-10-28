//! Projected history for read-scoped tokens. Such a token sees the
//! repository as if it only ever contained its paths: every change is
//! rebuilt with a tree holding just those paths, changes that touched
//! nothing there are dropped, and the results are stored as ordinary
//! objects. Its refs, log, changes and object requests are all answered from
//! that projection, so it can clone and fetch normally, and nothing outside
//! its paths, not even commit messages, reaches it. A landing from a
//! projected clone names a projected base, which is mapped back to the real
//! change before merging.
//!
//! The mapping (real change -> projected change) and the set of objects the
//! scope may fetch are kept per scope under projections/ and extended
//! incrementally.

use crate::server::in_scope;
use crate::store::{Change, Repo, hash_object, write_file_atomic};
use crate::tree::{Entry, Mode, Tree};
use crate::util::{self, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default)]
pub struct Projection {
    #[serde(skip)]
    read: Vec<String>,
    #[serde(skip)]
    path: PathBuf,
    /// real change -> projected ("" = nothing in scope yet)
    #[serde(default, deserialize_with = "util::nullable")]
    map: BTreeMap<String, String>,
    /// objects this scope may fetch
    #[serde(default, deserialize_with = "util::nullable")]
    allowed: BTreeMap<String, bool>,
    /// projected change -> real change
    #[serde(skip)]
    back: HashMap<String, String>,
    /// "tree dir" -> projected tree (memo)
    #[serde(skip)]
    trees: HashMap<String, String>,
    #[serde(skip)]
    dirty: bool,
}

/// The key a scope's projection is stored under.
pub fn scope_key(read: &[String]) -> String {
    let mut sorted = read.to_vec();
    sorted.sort();
    hash_object("scope", sorted.join("\n").as_bytes())[..16].to_string()
}

impl Projection {
    pub fn load(r: &Repo, read: &[String]) -> Projection {
        let mut sorted = read.to_vec();
        sorted.sort();
        let path = r.dir.join("projections").join(format!("{}.json", scope_key(read)));
        let mut pj: Projection =
            std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        pj.read = sorted;
        pj.path = path;
        pj.back = pj.map.iter().filter(|(_, p)| !p.is_empty()).map(|(real, p)| (p.clone(), real.clone())).collect();
        pj
    }

    fn save(&mut self) {
        if self.dirty && write_file_atomic(&self.path, &util::to_json(self), false).is_ok() {
            self.dirty = false;
        }
    }

    /// Maps a real change to its projection ("" when nothing in scope exists
    /// yet at that point in history). A projected id maps to itself.
    pub fn project(&mut self, r: &Repo, id: &str) -> Result<String> {
        let res = self.project_all(r, id);
        self.save();
        res
    }

    fn project_all(&mut self, r: &Repo, id: &str) -> Result<String> {
        // parents before children, without recursing down long histories
        let mut stack = vec![(id.to_string(), false)];
        while let Some((cur, ready)) = stack.pop() {
            if self.map.contains_key(&cur) || self.back.contains_key(&cur) {
                continue;
            }
            let c = r.get_change(&cur)?;
            if ready {
                self.project_one(r, &cur, c)?;
                continue;
            }
            stack.push((cur, true));
            for p in c.parents.iter().rev() {
                stack.push((p.clone(), false));
            }
        }
        Ok(self.map.get(id).cloned().unwrap_or_else(|| id.to_string()))
    }

    /// Projects one change whose parents are projected already.
    fn project_one(&mut self, r: &Repo, id: &str, c: Change) -> Result<()> {
        let mut parents: Vec<String> = vec![];
        for p in &c.parents {
            let pp = self.map.get(p).cloned().unwrap_or_else(|| p.clone());
            if !pp.is_empty() && !parents.contains(&pp) {
                parents.push(pp);
            }
        }
        let tree = self.tree(r, &c.tree, "")?;
        // Nothing in scope changed: this change disappears into its parent.
        if parents.len() <= 1 {
            let parent_tree = match parents.first() {
                Some(p) => r.get_change(p)?.tree,
                None => String::new(),
            };
            let empty = r.put_tree(&mut Tree::default())?;
            if parent_tree == tree || (parents.is_empty() && tree == empty) {
                self.map.insert(id.to_string(), parents.first().cloned().unwrap_or_default());
                self.dirty = true;
                return Ok(());
            }
        }
        let conflicts = c.conflicts.iter().filter(|cf| in_scope(&self.read, &cf.path)).cloned().collect();
        let pc = Change { parents, tree, conflicts, sig: None, projected: id.to_string(), ..c };
        let pid = r.put_change(&pc)?;
        self.map.insert(id.to_string(), pid.clone());
        self.back.insert(pid.clone(), id.to_string());
        self.allowed.insert(pid, true);
        self.dirty = true;
        Ok(())
    }

    /// The tree (the directory at dir) cut down to the scope's paths.
    fn tree(&mut self, r: &Repo, tree: &str, dir: &str) -> Result<String> {
        let memo = format!("{tree} {dir}");
        if let Some(id) = self.trees.get(&memo) {
            return Ok(id.clone());
        }
        let t = r.get_tree(tree)?;
        let mut out = Tree::default();
        for e in &t.entries {
            let full = if dir.is_empty() { e.name.clone() } else { format!("{dir}/{}", e.name) };
            if in_scope(&self.read, &full) {
                out.entries.push(e.clone());
                self.allow(r, &e.id, e.mode)?;
            } else if e.mode == Mode::Dir && self.leads_in(&full) {
                let sub = self.tree(r, &e.id, &full)?;
                if r.get_tree(&sub).is_ok_and(|st| !st.entries.is_empty()) {
                    out.entries.push(Entry { name: e.name.clone(), mode: Mode::Dir, id: sub });
                }
            }
        }
        let id = r.put_tree(&mut out)?;
        self.trees.insert(memo, id.clone());
        self.allowed.insert(id.clone(), true);
        self.dirty = true;
        Ok(id)
    }

    /// Whether a directory contains part of the scope.
    fn leads_in(&self, dir: &str) -> bool {
        self.read.iter().any(|pre| pre.starts_with(&format!("{dir}/")))
    }

    /// Marks an in-scope object, and everything under it, fetchable.
    fn allow(&mut self, r: &Repo, id: &str, mode: Mode) -> Result<()> {
        if self.allowed.contains_key(id) {
            return Ok(());
        }
        self.allowed.insert(id.to_string(), true);
        self.dirty = true;
        if mode == Mode::Dir {
            for e in &r.get_tree(id)?.entries {
                self.allow(r, &e.id, e.mode)?;
            }
        }
        Ok(())
    }

    pub fn may(&self, id: &str) -> bool {
        self.allowed.contains_key(id)
    }

    /// Maps a projected change back to the real one (None if unknown).
    pub fn real(&self, id: &str) -> Option<String> {
        self.back.get(id).cloned()
    }

    /// What a read-scoped token sees as refs.
    pub fn project_refs(&mut self, r: &Repo, refs: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for (name, id) in refs {
            let p = self.project(r, id)?;
            if !p.is_empty() {
                out.insert(name.clone(), p);
            }
        }
        Ok(out)
    }
}
