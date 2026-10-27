//! History walking, merge bases, tree merges, and the commands that move
//! HEAD: commit, merge, checkout and undo.

use crate::diff::{apply_hunks, is_binary, merge3, split_lines};
use crate::index::work_of;
use crate::sign;
use crate::store::{under, Change, Conflict, Meta, Op, Repo};
use crate::tree::{apply_deltas, has_markers, FileEnt, Flat, Mode};
use crate::util::{self, dir_name, Error, Result};
use crate::{bail, err};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

impl Repo {
    // -----------------------------------------------------------------------
    // History walking

    pub fn ancestors(&self, id: &str) -> Result<HashMap<String, usize>> {
        let mut dist = HashMap::from([(id.to_string(), 0)]);
        let mut queue = VecDeque::from([id.to_string()]);
        while let Some(cur) = queue.pop_front() {
            let d = dist[&cur];
            for p in self.get_change(&cur)?.parents {
                if !dist.contains_key(&p) {
                    dist.insert(p.clone(), d + 1);
                    queue.push_back(p);
                }
            }
        }
        Ok(dist)
    }

    pub fn is_ancestor(&self, anc: &str, desc: &str) -> Result<bool> {
        if anc == desc {
            return Ok(true);
        }
        // the common case for pushes and landings: anc is a direct parent
        if self.get_change(desc).is_ok_and(|c| c.parents.iter().any(|p| p == anc)) {
            return Ok(true);
        }
        Ok(self.ancestors(desc)?.contains_key(anc))
    }

    /// Every best common ancestor of a and b: common ancestors that are not
    /// themselves behind another one. Criss-cross histories have more than
    /// one.
    pub fn merge_bases(&self, a: &str, b: &str) -> Result<Vec<String>> {
        let aset = self.ancestors(a)?;
        let mut seen = HashSet::from([b.to_string()]);
        let mut queue = VecDeque::from([b.to_string()]);
        let mut frontier = vec![];
        while let Some(cur) = queue.pop_front() {
            if aset.contains_key(&cur) {
                frontier.push(cur);
                continue;
            }
            for p in self.get_change(&cur)?.parents {
                if seen.insert(p.clone()) {
                    queue.push_back(p);
                }
            }
        }
        let mut best = vec![];
        for x in &frontier {
            let mut behind = false;
            for y in frontier.iter().filter(|y| *y != x) {
                if self.is_ancestor(x, y)? {
                    behind = true;
                    break;
                }
            }
            if !behind {
                best.push(x.clone());
            }
        }
        best.sort();
        Ok(best)
    }

    /// The tree to three-way merge a and b against. With several best common
    /// ancestors they are merged into a virtual base first, recursively, like
    /// git's recursive strategy. Conflicts inside the virtual base stay as
    /// markers, which then usually resolve against the two real sides.
    fn merge_base_flat(&self, a: &str, b: &str, depth: usize) -> Result<Flat> {
        let bases = self.merge_bases(a, b)?;
        let Some(first) = bases.first() else { return Ok(Flat::new()) };
        let mut virt = self.change_flat(first)?;
        for other in &bases[1..] {
            if depth >= 8 {
                break; // shortcut: very deep criss-cross nesting falls back to one base
            }
            let ob = self.change_flat(other)?;
            let below = self.merge_base_flat(first, other, depth + 1)?;
            virt = self.merge_flat(&below, &virt, &ob, "base", "base")?.0;
        }
        Ok(virt)
    }

    /// Visits every ancestor of start, newest first, until f returns false.
    pub fn walk(&self, start: &str, mut f: impl FnMut(&str, &Change) -> bool) -> Result<()> {
        // newest first; among equal times, the one queued first
        let mut heap = BinaryHeap::new();
        let mut seq = 0usize;
        let mut changes = HashMap::new();
        heap.push((self.get_change(start)?.time, std::cmp::Reverse(seq), start.to_string()));
        changes.insert(start.to_string(), self.get_change(start)?);
        let mut seen = HashSet::from([start.to_string()]);
        while let Some((_, _, id)) = heap.pop() {
            let c = changes.remove(&id).unwrap();
            if !f(&id, &c) {
                return Ok(());
            }
            for p in &c.parents {
                if seen.insert(p.clone()) {
                    let pc = self.get_change(p)?;
                    seq += 1;
                    heap.push((pc.time, std::cmp::Reverse(seq), p.clone()));
                    changes.insert(p.clone(), pc);
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Merging trees

    /// Prepares a three-way merge for renames: when one side renamed a file,
    /// the base and the other side's version move to the new path too, so an
    /// edit on one side and a rename on the other merge into the renamed file
    /// instead of conflicting as delete-vs-modify. Files the other side added
    /// to a directory that this side moved wholesale follow it. A file renamed
    /// on both sides is left alone.
    fn align_renames(&self, base: &Flat, ours: &Flat, theirs: &Flat) -> (Flat, Flat, Flat) {
        let (ro, rt) = (self.renames(base, ours), self.renames(base, theirs));
        let (mut b, mut o, mut t) = (base.clone(), ours.clone(), theirs.clone());
        if ro.is_empty() && rt.is_empty() {
            return (b, o, t);
        }
        fn mv(side: &mut Flat, from: &str, to: &str) {
            if side.contains_key(from) && !side.contains_key(to) {
                let fe = side.remove(from).unwrap();
                side.insert(to.to_string(), fe);
            }
        }
        for (old, new) in &ro {
            if !rt.contains_key(old) {
                mv(&mut t, old, new);
                mv(&mut b, old, new);
            }
        }
        for (old, new) in &rt {
            if !ro.contains_key(old) {
                mv(&mut o, old, new);
                mv(&mut b, old, new);
            }
        }
        let follow = |ren: &HashMap<String, String>, side: &mut Flat| {
            let dirs = dir_renames(base, ren);
            let add: Vec<String> = side
                .keys()
                .filter(|p| !base.contains_key(*p) && dirs.contains_key(dir_name(p)))
                .cloned()
                .collect();
            for p in add {
                let to = format!("{}/{}", dirs[dir_name(&p)], util::base_name(&p));
                mv(side, &p, &to);
            }
        };
        follow(&ro, &mut t);
        follow(&rt, &mut o);
        (b, o, t)
    }

    /// Three-way merges two flat trees against their base. Files that
    /// cannot be merged cleanly are written with conflict markers (text) or
    /// keep our side (binary/symlink/delete-vs-modify) and are reported as
    /// conflicts.
    pub fn merge_flat(&self, base: &Flat, ours: &Flat, theirs: &Flat, ol: &str, tl: &str) -> Result<(Flat, Vec<Conflict>)> {
        let paths: BTreeSet<&String> = base.keys().chain(ours.keys()).chain(theirs.keys()).collect();
        let mut out = Flat::new();
        let mut conflicts = vec![];
        for p in paths {
            let (b, o, t) = (base.get(p), ours.get(p), theirs.get(p));
            if o == t {
                if let Some(o) = o {
                    out.insert(p.clone(), o.clone());
                }
            } else if o == b {
                if let Some(t) = t {
                    out.insert(p.clone(), t.clone());
                }
            } else if t == b {
                if let Some(o) = o {
                    out.insert(p.clone(), o.clone());
                }
            } else if o.is_none() || t.is_none() {
                // one side deleted, the other changed it
                let keep = o.or(t).unwrap().clone();
                conflicts.push(Conflict {
                    path: p.clone(),
                    base: b.map(|e| e.id.clone()).unwrap_or_default(),
                    ours: o.map(|e| e.id.clone()).unwrap_or_default(),
                    theirs: t.map(|e| e.id.clone()).unwrap_or_default(),
                    result: keep.id.clone(),
                });
                out.insert(p.clone(), keep);
            } else {
                // both sides changed
                let (o, t) = (o.unwrap(), t.unwrap());
                let mode = match b {
                    None => o.mode,
                    Some(b) if o.mode == b.mode => t.mode,
                    Some(_) => o.mode,
                };
                let id = |e: &FileEnt| e.id.clone();
                if b.is_some_and(|b| o.id == b.id) {
                    out.insert(p.clone(), FileEnt::new(mode, id(t)));
                } else if b.is_some_and(|b| t.id == b.id) || o.id == t.id {
                    out.insert(p.clone(), FileEnt::new(mode, id(o)));
                } else {
                    let (fe, cf) = self.merge_content(p, b, o, t, mode, ol, tl)?;
                    out.insert(p.clone(), fe);
                    conflicts.extend(cf);
                }
            }
        }
        Ok((out, conflicts))
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_content(
        &self,
        p: &str,
        b: Option<&FileEnt>,
        o: &FileEnt,
        t: &FileEnt,
        mode: Mode,
        ol: &str,
        tl: &str,
    ) -> Result<(FileEnt, Option<Conflict>)> {
        let od = self.get_blob(&o.id)?;
        let td = self.get_blob(&t.id)?;
        let bd = match b {
            Some(b) => self.get_blob(&b.id)?,
            None => Vec::new().into(),
        };
        let mut cf = Conflict {
            path: p.to_string(),
            base: b.map(|b| b.id.clone()).unwrap_or_default(),
            ours: o.id.clone(),
            theirs: t.id.clone(),
            result: String::new(),
        };
        if o.mode == Mode::Link || t.mode == Mode::Link || is_binary(&od) || is_binary(&td) || is_binary(&bd) {
            cf.result = o.id.clone();
            return Ok((FileEnt::new(o.mode, o.id.clone()), Some(cf)));
        }
        let res = merge3(&split_lines(&bd), &split_lines(&od), &split_lines(&td), ol, tl);
        if res.conflicts > 0 {
            if let Some(merged) = crate::codemerge::code_merge(p, &bd, &od, &td) {
                return Ok((FileEnt::new(mode, self.put_blob(&merged)?), None));
            }
        }
        let id = self.put_blob(&res.text())?;
        if res.conflicts == 0 {
            return Ok((FileEnt::new(mode, id), None));
        }
        cf.result = id.clone();
        Ok((FileEnt::new(mode, id), Some(cf)))
    }

    // -----------------------------------------------------------------------
    // Commands on history

    pub fn new_change(&self, parents: Vec<String>, tree: String, msg: &str, meta: Meta, conflicts: Vec<Conflict>) -> Change {
        let mut c = Change {
            parents,
            tree,
            author: self.author(),
            time: util::now_unix(),
            tz: util::tz_now(),
            message: msg.to_string(),
            meta,
            conflicts,
            ..Default::default()
        };
        match sign::signing_key() {
            Err(e) => eprintln!("warning: change not signed: {e}"),
            Ok(Some(key)) => c.sig = Some(sign::sign(&key, &sign::change_payload(&c))),
            Ok(None) => {}
        }
        c
    }

    /// Snapshots the working tree as a new change on the current branch.
    /// Only the changed files are read and stored, and only the directories
    /// along their paths are rewritten.
    pub fn commit(&self, o: &CommitOpts) -> Result<String> {
        let (_, head) = self.head();
        let parent = if head.is_empty() { None } else { Some(self.get_change(&head)?) };
        let st = self.work_status(true)?;
        let picked = |p: &str| {
            (o.paths.is_empty() && o.hunks.is_empty()) || o.paths.iter().any(|pre| under(p, pre))
        };
        let mut edits: BTreeMap<String, Option<FileEnt>> =
            st.deltas.iter().filter(|d| picked(&d.path)).map(|d| (d.path.clone(), d.new.clone())).collect();
        for (p, nums) in &o.hunks {
            let in_head = self.lookup(&st.tree, p)?.filter(|e| e.mode != Mode::Dir);
            let in_work = match st.deltas.iter().find(|d| d.path == *p) {
                Some(d) => d.new.clone(),
                None => in_head.clone(),
            };
            let Some(mode) = in_head.as_ref().or(in_work.as_ref()).map(|e| e.mode) else {
                bail!("{p}: no such file");
            };
            let blob = |e: &Option<FileEnt>| -> Result<Vec<u8>> {
                Ok(match e {
                    Some(e) => self.get_blob(&e.id)?.to_vec(),
                    None => vec![],
                })
            };
            let keep: HashSet<usize> = nums.iter().copied().collect();
            let lines = apply_hunks(&split_lines(&blob(&in_head)?), &split_lines(&blob(&in_work)?), &keep)
                .map_err(|e| err!("{p}: {e}"))?;
            edits.insert(p.clone(), Some(FileEnt::new(mode, self.put_blob(&lines.concat())?)));
        }
        let tree = self.update_tree(&st.tree, &edits)?;
        if parent.as_ref().is_some_and(|p| p.tree == tree) && !o.allow_empty {
            st.save(self);
            return Err(Error::NothingToCommit);
        }
        // Carry forward conflicts that are still unresolved: the file is
        // untouched since the merge, or still contains conflict markers.
        let mut carried = vec![];
        for mut cf in parent.map(|p| p.conflicts).unwrap_or_default() {
            let Some(fe) = self.lookup(&tree, &cf.path)? else { continue };
            if fe.id == cf.result {
                carried.push(cf);
            } else if self.get_blob(&fe.id).is_ok_and(|d| has_markers(&d)) {
                cf.result = fe.id;
                carried.push(cf);
            }
        }
        let parents = if head.is_empty() { vec![] } else { vec![head] };
        let id = self.put_change(&self.new_change(parents, tree.clone(), &o.message, o.meta.clone(), carried))?;
        self.advance_head(&id)?;
        st.save_with(self, &tree, &edits, &BTreeMap::new());
        Ok(id)
    }

    /// Integrates `target` into the current branch. A conflicted merge still
    /// completes: the conflicts are recorded in the new change and the
    /// working tree holds the marked-up files, so work can continue.
    pub fn merge(&self, target_rev: &str, no_ff: bool, msg: &str, meta: Meta) -> Result<MergeOutcome> {
        let (r, cur) = self.head();
        let target = self.resolve_rev(target_rev)?;
        let cur_flat = self.change_flat(&cur)?; // empty for an unborn branch
        let dirty = self.working_deltas()?;
        if !dirty.is_empty() {
            bail!("uncommitted changes ({} paths); commit or discard them first", dirty.len());
        }
        if !cur.is_empty() && self.is_ancestor(&target, &cur)? {
            return Ok(MergeOutcome { kind: "up-to-date", change: cur, conflicts: vec![] });
        }
        // an unborn branch simply adopts the target
        let ff = cur.is_empty() || (self.is_ancestor(&cur, &target)? && !no_ff);
        if ff {
            self.materialize(&cur_flat, &self.change_flat(&target)?)?;
            self.advance_head(&target)?;
            return Ok(MergeOutcome { kind: "fast-forward", change: target, conflicts: vec![] });
        }
        let base = self.merge_base_flat(&cur, &target, 0)?;
        let theirs = self.change_flat(&target)?;
        let ours_label = r.strip_prefix("heads/").map_or("ours".to_string(), String::from);
        let (mb, mo, mt) = self.align_renames(&base, &cur_flat, &theirs);
        let (merged, conflicts) = self.merge_flat(&mb, &mo, &mt, &ours_label, target_rev)?;
        let tree = self.build_tree(&merged)?;
        let msg = if msg.is_empty() { format!("Merge {target_rev} into {ours_label}") } else { msg.to_string() };
        let id = self.put_change(&self.new_change(vec![cur, target], tree, &msg, meta, conflicts.clone()))?;
        // In a sparse checkout, conflicted files must be on disk to be
        // resolved, so they join the view.
        let mut cfg = self.load_config();
        if !cfg.sparse.is_empty() {
            for cf in &conflicts {
                if !cfg.in_view(&cf.path) {
                    cfg.sparse.push(cf.path.clone());
                }
            }
            self.save_config(&cfg)?;
        }
        self.materialize(&cur_flat, &merged)?;
        self.advance_head(&id)?;
        Ok(MergeOutcome { kind: "merged", change: id, conflicts })
    }

    /// Moves HEAD to a branch (or a detached change) and updates the working
    /// tree. Without force it refuses to discard uncommitted work. With
    /// create, it makes branch `rev` at `start` (default: HEAD) and switches
    /// to it. Only the paths that differ between the two trees are touched.
    pub fn checkout(&self, rev: &str, force: bool, create: bool, start: &str) -> Result<()> {
        let (_, cur) = self.head();
        let (target, head_ref) = if create {
            let name = format!("heads/{rev}");
            if !crate::store::valid_ref_name(&name) {
                bail!("invalid branch name {rev:?}");
            }
            if self.read_ref(&name).is_some() {
                bail!("branch {rev:?} already exists");
            }
            if start.is_empty() && cur.is_empty() {
                bail!("no changes yet; commit first or give a start point");
            }
            (self.resolve_rev(start)?, name)
        } else if let Some(id) = self.read_ref(&format!("heads/{rev}")) {
            (id, format!("heads/{rev}"))
        } else {
            (self.resolve_rev(rev)?, String::new())
        };
        if create && target == cur {
            // a new branch at the current change: nothing on disk moves, so
            // uncommitted work simply comes along
            self.write_ref(&head_ref, &target)?;
            return self.set_head_ref(&head_ref);
        }
        let st = self.work_status(false)?;
        if !st.deltas.is_empty() && !force {
            bail!("uncommitted changes ({} paths); commit them or use -f to discard", st.deltas.len());
        }
        let to_tree = self.tree_of(&target)?;
        // HEAD's state is what is on disk, except where the tree is dirty
        let mut have: BTreeMap<String, Option<FileEnt>> = BTreeMap::new();
        let mut want: BTreeMap<String, Option<FileEnt>> = BTreeMap::new();
        for d in self.diff_trees(&st.tree, &to_tree)? {
            have.insert(d.path.clone(), d.old);
            want.insert(d.path, d.new);
        }
        for d in &st.deltas {
            have.insert(d.path.clone(), d.new.clone());
            if !want.contains_key(&d.path) {
                want.insert(d.path.clone(), self.lookup(&to_tree, &d.path)?.filter(|e| e.mode != Mode::Dir));
            }
        }
        let cfg = self.load_config();
        let mut work = BTreeMap::new();
        for (p, w) in &want {
            if w.is_none() && have[p].is_some() && cfg.in_view(p) {
                self.remove_work(p)?;
                work.insert(p.clone(), None);
            }
        }
        let mut dirs = HashSet::new();
        for (p, w) in &want {
            let (Some(w), h) = (w, &have[p]) else { continue };
            if h.as_ref() == Some(w) || !cfg.in_view(p) {
                continue;
            }
            let same_mode = h.as_ref().is_some_and(|h| h.mode == w.mode && w.mode != Mode::Link);
            self.write_file_in(p, w, Some(&mut dirs), same_mode)?;
            if let Ok(md) = std::fs::symlink_metadata(self.abs(p)) {
                work.insert(p.clone(), work_of(&md, w.id.clone()));
            }
        }
        st.save_with(self, &to_tree, &want, &work);
        if create {
            self.write_ref(&head_ref, &target)?;
        }
        if head_ref.is_empty() {
            self.set_head_detached(&target)
        } else {
            self.set_head_ref(&head_ref)
        }
    }

    /// Reverts the most recent recorded operation. Because undo is itself
    /// recorded, undoing it again redoes the original operation.
    pub fn undo(&self, force: bool) -> Result<Op> {
        let ops = self.ops()?;
        let Some(last) = ops.last().cloned() else { bail!("nothing to undo") };
        let (_, cur) = self.head();
        let cur_flat = self.change_flat(&cur)?;
        let dirty = self.working_deltas()?;
        if !dirty.is_empty() && !force {
            bail!("uncommitted changes ({} paths); commit them or use -f to discard", dirty.len());
        }
        let from = apply_deltas(cur_flat, &dirty);
        // the tree HEAD will have after restoring
        let new_head = match last.before.head.strip_prefix("ref: ") {
            Some(r) => last.before.refs.get(r).cloned().unwrap_or_default(),
            None => last.before.head.clone(),
        };
        let to = self.change_flat(&new_head)?;
        self.record("undo", || {
            self.restore_state(&last.before)?;
            self.materialize(&from, &to)
        })?;
        Ok(last)
    }
}

/// Directories whose files all moved, under the same names, into one other
/// directory.
fn dir_renames(base: &Flat, ren: &HashMap<String, String>) -> HashMap<String, String> {
    let mut files: HashMap<&str, usize> = HashMap::new();
    for p in base.keys() {
        *files.entry(dir_name(p)).or_default() += 1;
    }
    let mut moves: HashMap<(&str, &str), usize> = HashMap::new();
    for (old, new) in ren {
        if util::base_name(old) == util::base_name(new) && dir_name(old) != dir_name(new) {
            *moves.entry((dir_name(old), dir_name(new))).or_default() += 1;
        }
    }
    moves
        .into_iter()
        .filter(|((from, _), n)| files.get(from) == Some(n))
        .map(|((from, to), _)| (from.to_string(), to.to_string()))
        .collect()
}

#[derive(Default)]
pub struct CommitOpts {
    pub message: String,
    pub meta: Meta,
    pub allow_empty: bool,
    /// if set, only these files/directories are committed; the rest stays as in HEAD
    pub paths: Vec<String>,
    /// path -> hunk numbers (as `diff` prints them) to commit from that file
    pub hunks: BTreeMap<String, Vec<usize>>,
}

pub struct MergeOutcome {
    /// up-to-date | fast-forward | merged
    pub kind: &'static str,
    pub change: String,
    pub conflicts: Vec<Conflict>,
}
