//! Line diff (LCS), unified diffs with numbered hunks, and three-way merge.

use crate::bail;
use crate::util::Result;
use std::collections::{HashMap, HashSet};
use std::io::Write;

/// A line with its "\n" (the last line of a file may lack one). Lines are
/// bytes: text in any encoding diffs and merges unchanged.
pub type Line = Vec<u8>;

pub fn split_lines(b: &[u8]) -> Vec<Line> {
    b.split_inclusive(|&c| c == b'\n').map(<[u8]>::to_vec).collect()
}

pub fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8000)].contains(&0)
}

#[derive(Clone, Copy, Debug)]
struct Match {
    a: usize,
    b: usize,
    n: usize,
}

/// Bounds the quadratic LCS table; larger middles are treated as a wholesale
/// replacement (correct, just not minimal).
const MAX_LCS: usize = 4_000_000;

/// The maximal runs of identical lines shared by a and b, in order. It is
/// the primitive both diff and merge are built on.
fn matching_blocks(a: &[Line], b: &[Line]) -> Vec<Match> {
    fn intern<'a>(ids: &mut HashMap<&'a [u8], u32>, ss: &'a [Line]) -> Vec<u32> {
        ss.iter()
            .map(|s| {
                let n = ids.len() as u32;
                *ids.entry(s.as_slice()).or_insert(n)
            })
            .collect()
    }
    let mut ids = HashMap::new();
    let (ai, bi) = (intern(&mut ids, a), intern(&mut ids, b));
    let mut pre = 0;
    while pre < ai.len() && pre < bi.len() && ai[pre] == bi[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < ai.len() - pre && suf < bi.len() - pre && ai[ai.len() - 1 - suf] == bi[bi.len() - 1 - suf] {
        suf += 1;
    }
    let mut pairs: Vec<(usize, usize)> = (0..pre).map(|i| (i, i)).collect();
    let (ma, mb) = (&ai[pre..ai.len() - suf], &bi[pre..bi.len() - suf]);
    if !ma.is_empty() && !mb.is_empty() && ma.len() * mb.len() <= MAX_LCS {
        let w = mb.len() + 1;
        let mut l = vec![0u32; (ma.len() + 1) * w];
        for i in (0..ma.len()).rev() {
            for j in (0..mb.len()).rev() {
                l[i * w + j] = if ma[i] == mb[j] {
                    l[(i + 1) * w + j + 1] + 1
                } else {
                    l[(i + 1) * w + j].max(l[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < ma.len() && j < mb.len() {
            if ma[i] == mb[j] {
                pairs.push((pre + i, pre + j));
                i += 1;
                j += 1;
            } else if l[(i + 1) * w + j] >= l[i * w + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
    for k in (1..=suf).rev() {
        pairs.push((ai.len() - k, bi.len() - k));
    }
    let mut blocks: Vec<Match> = vec![];
    for (pa, pb) in pairs {
        match blocks.last_mut() {
            Some(m) if m.a + m.n == pa && m.b + m.n == pb => m.n += 1,
            _ => blocks.push(Match { a: pa, b: pb, n: 1 }),
        }
    }
    blocks
}

#[derive(Clone, Copy)]
struct Edit<'a> {
    op: u8, // b' ', b'-', b'+'
    line: &'a [u8],
}

fn diff_edits<'a>(a: &'a [Line], b: &'a [Line]) -> Vec<Edit<'a>> {
    let mut out = vec![];
    let (mut ai, mut bi) = (0, 0);
    let mut blocks = matching_blocks(a, b);
    blocks.push(Match { a: a.len(), b: b.len(), n: 0 });
    for m in blocks {
        out.extend(a[ai..m.a].iter().map(|l| Edit { op: b'-', line: l }));
        out.extend(b[bi..m.b].iter().map(|l| Edit { op: b'+', line: l }));
        out.extend(a[m.a..m.a + m.n].iter().map(|l| Edit { op: b' ', line: l }));
        ai = m.a + m.n;
        bi = m.b + m.n;
    }
    out
}

/// Groups the non-equal edits into hunks with ctx lines of context,
/// returning each hunk's [start, stop) range in edits.
fn hunks(edits: &[Edit], ctx: usize) -> Vec<(usize, usize)> {
    let mut out = vec![];
    let n = edits.len();
    let mut i = 0;
    while i < n {
        if edits[i].op == b' ' {
            i += 1;
            continue;
        }
        let start = i.saturating_sub(ctx);
        let (mut end, mut j) = (i, i);
        while j < n {
            if edits[j].op != b' ' {
                end = j;
                j += 1;
                continue;
            }
            let mut k = j;
            while k < n && edits[k].op == b' ' {
                k += 1;
            }
            if k == n || k - j > 2 * ctx {
                break;
            }
            j = k;
        }
        let stop = (end + 1 + ctx).min(n);
        out.push((start, stop));
        i = stop;
    }
    out
}

/// Renders a unified diff with ctx lines of context, or "" when there is no
/// difference. Hunks are numbered from 1 so that `commit --hunk path:N` can
/// pick them.
pub fn unified_diff(name: &str, a: &[Line], b: &[Line], ctx: usize) -> Vec<u8> {
    let edits = diff_edits(a, b);
    let mut a_pos = vec![0; edits.len() + 1];
    let mut b_pos = vec![0; edits.len() + 1];
    for (k, e) in edits.iter().enumerate() {
        a_pos[k + 1] = a_pos[k] + (e.op != b'+') as usize;
        b_pos[k + 1] = b_pos[k] + (e.op != b'-') as usize;
    }
    let mut sb = Vec::new();
    for (h, &(start, stop)) in hunks(&edits, ctx).iter().enumerate() {
        if sb.is_empty() {
            let _ = write!(sb, "--- a/{name}\n+++ b/{name}\n");
        }
        let (ac, bc) = (a_pos[stop] - a_pos[start], b_pos[stop] - b_pos[start]);
        let a_start = a_pos[start] + (ac != 0) as usize;
        let b_start = b_pos[start] + (bc != 0) as usize;
        let _ = writeln!(sb, "@@ -{a_start},{ac} +{b_start},{bc} @@ hunk {}", h + 1);
        for e in &edits[start..stop] {
            sb.push(e.op);
            sb.extend_from_slice(e.line);
            if !e.line.ends_with(b"\n") {
                sb.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
    }
    sb
}

/// a with only the chosen hunks (numbered from 1, as unified_diff prints
/// them with 3 lines of context) of the change to b applied.
pub fn apply_hunks(a: &[Line], b: &[Line], keep: &HashSet<usize>) -> Result<Vec<Line>> {
    let edits = diff_edits(a, b);
    let hs = hunks(&edits, 3);
    for &n in keep {
        if n < 1 || n > hs.len() {
            bail!("no hunk {n} (there are {})", hs.len());
        }
    }
    let mut chosen = vec![false; edits.len()];
    for (h, &(start, stop)) in hs.iter().enumerate() {
        if keep.contains(&(h + 1)) {
            chosen[start..stop].iter_mut().for_each(|c| *c = true);
        }
    }
    Ok(edits
        .iter()
        .zip(chosen)
        .filter(|(e, c)| e.op == b' ' || (e.op == b'-') != *c)
        .map(|(e, _)| e.line.to_vec())
        .collect())
}

// ---------------------------------------------------------------------------
// Three-way merge

pub struct MergeResult {
    pub lines: Vec<Line>,
    pub conflicts: usize,
}

impl MergeResult {
    pub fn text(&self) -> Vec<u8> {
        self.lines.concat()
    }
}

fn ensure_nl(lines: &[Line]) -> impl Iterator<Item = Line> + '_ {
    let n = lines.len();
    lines.iter().enumerate().map(move |(i, l)| {
        let mut l = l.clone();
        if i + 1 == n && !l.ends_with(b"\n") {
            l.push(b'\n');
        }
        l
    })
}

/// Merges `ours` and `theirs`, both derived from `base`. Regions that only
/// one side touched are taken from that side; regions both sides changed
/// differently are emitted between conflict markers.
pub fn merge3(base: &[Line], ours: &[Line], theirs: &[Line], ours_label: &str, theirs_label: &str) -> MergeResult {
    let ma = matching_blocks(base, ours);
    let mb = matching_blocks(base, theirs);
    let mut regions = vec![]; // (base, ours, theirs, len)
    let (mut i, mut j) = (0, 0);
    while i < ma.len() && j < mb.len() {
        let (x, y) = (ma[i], mb[j]);
        let (lo, hi) = (x.a.max(y.a), (x.a + x.n).min(y.a + y.n));
        if lo < hi {
            regions.push((lo, x.b + (lo - x.a), y.b + (lo - y.a), hi - lo));
        }
        if x.a + x.n < y.a + y.n {
            i += 1;
        } else {
            j += 1;
        }
    }
    regions.push((base.len(), ours.len(), theirs.len(), 0));
    let mut res = MergeResult { lines: vec![], conflicts: 0 };
    let (mut bp, mut op, mut tp) = (0, 0, 0);
    for (rb, ro, rt, n) in regions {
        let (bc, oc, tc) = (&base[bp..rb], &ours[op..ro], &theirs[tp..rt]);
        if bc.is_empty() && oc.is_empty() && tc.is_empty() {
        } else if oc == bc {
            res.lines.extend_from_slice(tc);
        } else if tc == bc || oc == tc {
            res.lines.extend_from_slice(oc);
        } else {
            res.conflicts += 1;
            res.lines.push(format!("<<<<<<< {ours_label}\n").into_bytes());
            res.lines.extend(ensure_nl(oc));
            res.lines.push(b"=======\n".to_vec());
            res.lines.extend(ensure_nl(tc));
            res.lines.push(format!(">>>>>>> {theirs_label}\n").into_bytes());
        }
        res.lines.extend_from_slice(&ours[ro..ro + n]);
        (bp, op, tp) = (rb + n, ro + n, rt + n);
    }
    res
}

/// merge3 over whole texts, giving the result if it has no conflicts.
pub fn merge3_text(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let l = |s: &str| split_lines(s.as_bytes());
    let res = merge3(&l(base), &l(ours), &l(theirs), "ours", "theirs");
    (res.conflicts == 0).then(|| String::from_utf8(res.text()).ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<Line> {
        split_lines(s.as_bytes())
    }

    #[test]
    fn merge3_cases() {
        for (name, base, ours, theirs, want, conflicts) in [
            ("disjoint edits", "a\nb\nc\nd\ne\n", "A\nb\nc\nd\ne\n", "a\nb\nc\nd\nE\n", "A\nb\nc\nd\nE\n", 0),
            ("same edit both sides", "a\nb\n", "a\nX\n", "a\nX\n", "a\nX\n", 0),
            ("only theirs changed", "a\nb\n", "a\nb\n", "a\nb\nc\n", "a\nb\nc\n", 0),
            ("conflict", "a\nb\nc\n", "a\nX\nc\n", "a\nY\nc\n", "a\n<<<<<<< ours\nX\n=======\nY\n>>>>>>> theirs\nc\n", 1),
            ("both insert different", "a\n", "a\nx\n", "a\ny\n", "a\n<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\n", 1),
            ("one deletes other untouched", "a\nb\nc\n", "a\nc\n", "a\nb\nc\n", "a\nc\n", 0),
            ("add vs delete elsewhere", "a\nb\nc\nd\ne\nf\n", "a\nb\nc\nd\ne\nf\ng\n", "b\nc\nd\ne\nf\n", "b\nc\nd\ne\nf\ng\n", 0),
            ("empty base both add same", "", "x\n", "x\n", "x\n", 0),
            ("no trailing newline conflict", "a", "b", "c", "<<<<<<< ours\nb\n=======\nc\n>>>>>>> theirs\n", 1),
        ] {
            let res = merge3(&lines(base), &lines(ours), &lines(theirs), "ours", "theirs");
            assert_eq!((String::from_utf8(res.text()).unwrap().as_str(), res.conflicts), (want, conflicts), "{name}");
        }
    }

    #[test]
    fn diff_round_trip() {
        let a = lines("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n");
        let b = lines("one\n2\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\n");
        let d = String::from_utf8(unified_diff("f", &a, &b, 1)).unwrap();
        for want in ["--- a/f", "+++ b/f", "-two\n", "+2\n", "+eleven\n", "@@ -1,3 +1,3 @@", "@@ -10,1 +10,2 @@"] {
            assert!(d.contains(want), "diff missing {want:?}:\n{d}");
        }
        assert!(unified_diff("f", &a, &a, 3).is_empty());
    }
}
