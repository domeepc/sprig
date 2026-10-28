// sprig-ui's page. Rust (src/bin/sprig-ui.rs) reads the repository and runs
// sprig commands; this file only draws. Everything from the repository is
// untrusted text, so it goes through esc() before it reaches innerHTML.

const invoke = window.__TAURI__.core.invoke;

const $ = (id) => document.getElementById(id);
const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

// Stroke icons (lucide), 24-unit box.
const PATHS = {
  sprout: '<path d="M12 22V9"/><path d="M12 13c-3 0-6-2-6-6 3 0 6 2 6 6z"/><path d="M12 10c0-3 2-6 6-6 0 3-2 6-6 6z"/>',
  inbox: '<path d="M22 12h-6l-2 3h-4l-2-3H2"/><path d="M5.5 5h13L22 12v6a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-6z"/>',
  diff: '<path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9z"/><path d="M12 9v6M9 12h6M9 18h6"/>',
  clock: '<circle cx="12" cy="12" r="9"/><path d="M12 8v4l3 2"/>',
  branch: '<circle cx="6" cy="6" r="2"/><circle cx="6" cy="18" r="2"/><circle cx="18" cy="8" r="2"/><path d="M6 8v8M18 10c0 4-6 3-10 7"/>',
  warn: '<path d="M12 9v4M12 17h.01"/><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z"/>',
  undo: '<path d="M3 7v6h6"/><path d="M21 17a9 9 0 0 0-15-6.7L3 13"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  down: '<path d="m6 9 6 6 6-6"/>',
  left: '<path d="m15 18-6-6 6-6"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  cross: '<path d="M18 6 6 18M6 6l12 12"/>',
  merge: '<circle cx="6" cy="6" r="2"/><circle cx="18" cy="18" r="2"/><path d="M6 8v2a8 8 0 0 0 8 8h2"/>',
  gear: '<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>',
  shield: '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/><path d="m9 12 2 2 4-4"/>',
  up: '<path d="M12 19V5M5 12l7-7 7 7"/>',
  refresh: '<path d="M21 12a9 9 0 1 1-3-6.7L21 8"/><path d="M21 3v5h-5"/>',
};
const icon = (name, size = 15, color = "currentColor") =>
  `<svg aria-hidden="true" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="${color}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${PATHS[name]}</svg>`;

// ---------------------------------------------------------------------------
// State

let S = null; // the repository, from `state`
const V = {
  tab: "inbox", // inbox | changes | history | conflicts
  who: "all", // all | agents | people
  split: true,
  file: null, // the picked working-tree path (null: all)
  change: null, // the open change's id
  detail: null, // its provenance and diff, from `change`
  diff: [], // what the diff pane shows
  conflict: null, // the picked conflicted path
  sides: null, // its ours/base/theirs/working
  message: "",
  newBranch: "",
  githubEdit: null, // the GitHub URL being typed, or null
  menu: false, // the branch menu is open
  busy: "", // the running command's name
  status: { ok: true, text: "" },
  signing: null, // from `signing`: the key, whether it is trusted here, SSH keys
  closed: new Set(), // keys of the sections and diff files folded shut
};
const stats = new Map(); // change id -> [add, del]

const passing = (v) => ["pass", "passed", "ok", "approved", "true"].includes(String(v).toLowerCase());
const failing = (v) => ["fail", "failed", "false", "error", "rejected"].includes(String(v).toLowerCase());
const tone = (v) => (passing(v) ? "pass" : failing(v) ? "fail" : "pend");
const plural = (n, w) => `${n} ${w}${n === 1 ? "" : "s"}`;
const hash = (s) => [...s].reduce((a, c) => (a * 31 + c.charCodeAt(0)) >>> 0, 0);

function ago(t, long = false) {
  const s = Math.max(0, (S?.now ?? Date.now() / 1000) - t);
  const units = [[31536000, "y", "year"], [2592000, "mo", "month"], [86400, "d", "day"], [3600, "h", "hour"], [60, "m", "minute"]];
  for (const [n, a, b] of units) {
    if (s >= n) {
      const k = Math.floor(s / n);
      return long ? `${plural(k, b)} ago` : `${k}${a}`;
    }
  }
  return long ? "just now" : "now";
}

const isAgent = (c) => c.agent !== "";
const shown = (c) => V.who === "all" || (V.who === "agents") === isAgent(c);
const unreviewed = () => (S?.history ?? []).filter((c) => isAgent(c) && !passing(c.checks.review ?? ""));

/// Who made changes in the last week, busiest first.
function workers() {
  const week = (S?.now ?? 0) - 7 * 86400;
  const by = new Map();
  for (const c of S?.history ?? []) {
    if (c.time < week) break;
    const name = isAgent(c) ? c.agent : c.author;
    const key = `${isAgent(c)}:${name}`;
    if (!by.has(key)) by.set(key, { name, agent: isAgent(c), latest: c.title, count: 0, id: c.id });
    by.get(key).count++;
  }
  return [...by.values()].sort((a, b) => b.count - a.count);
}

// ---------------------------------------------------------------------------
// Talking to Rust

async function refresh() {
  try {
    S = await invoke("state");
  } catch (e) {
    S = null;
    V.status = { ok: false, text: String(e) };
  }
  if (S && V.conflict && !S.conflicts.includes(V.conflict)) V.conflict = null;
  if (V.tab === "changes") await loadWorking();
  if (V.tab === "conflicts") await loadConflict(V.conflict ?? S?.conflicts[0] ?? null);
  render();
}

async function act(name, args) {
  if (V.busy) return;
  V.busy = name;
  V.menu = false;
  render();
  try {
    const out = await invoke("run", { args });
    V.status = { ok: true, text: `${name}: ${out.split("\n").pop() || "done"}` };
    if (args[0] === "commit") V.message = "";
  } catch (e) {
    V.status = { ok: false, text: `${name}: ${e}` };
  }
  V.busy = "";
  await refresh();
}

async function loadWorking() {
  try {
    V.diff = await invoke("working_diff", { path: V.file });
  } catch (e) {
    V.diff = [];
    V.status = { ok: false, text: String(e) };
  }
}

async function openChange(id) {
  V.tab = "history";
  V.change = id;
  V.detail = null;
  render();
  try {
    V.detail = await invoke("change", { id });
  } catch (e) {
    V.status = { ok: false, text: String(e) };
  }
  render();
}

async function loadConflict(path) {
  V.conflict = path;
  V.sides = null;
  if (!path) return;
  try {
    V.sides = await invoke("conflict", { path });
  } catch (e) {
    V.status = { ok: false, text: String(e) };
  }
}

/// Fetches +N −N for the change rows on screen, then redraws them.
async function fillStats(ids) {
  const want = ids.filter((id) => !stats.has(id));
  if (!want.length) return;
  try {
    const got = await invoke("stats", { ids: want });
    for (const [id, s] of Object.entries(got)) stats.set(id, s);
    render();
  } catch {
    // counts are decoration; leave them blank
  }
}

// ---------------------------------------------------------------------------
// Pieces

const counts = (a, d, sep = " ") => `<span class="add">+${a}</span>${sep}<span class="del">−${d}</span>`;
const avatar = (name, agent) =>
  `<span class="avatar av-${hash(name) % 4}${agent ? "" : " person"}" aria-hidden="true">${esc((name[0] ?? "?").toUpperCase())}</span>`;
const pill = (k, v) => `<span class="pill ${tone(v)}">${esc(k)}=${esc(v)}</span>`;

/// A collapsible card; `key` remembers whether it was closed across redraws.
const fold = (key) => `data-key="${esc(key)}" ${V.closed.has(key) ? "" : "open"}`;

function section(n, title, aside, body) {
  const key = `s:${title.replace(/ on .*/, "")}`; // "Recent on main" keeps its state across branches
  return `<details class="section" ${fold(key)}><summary class="head">${icon("down", 12, "#8b8b93")}<span class="count">${n}</span>
    <h2>${esc(title)}</h2><div class="aside">${aside}</div></summary>${body}</details>`;
}

function changeRow(c) {
  const s = stats.get(c.id);
  const meta = isAgent(c)
    ? [c.model ? `${c.agent} · ${c.model}` : c.agent, c.intent && `intent: ${c.intent}`]
    : [c.author];
  if (c.signed) meta.push("signed");
  meta.push(c.short);
  const checks = Object.entries(c.checks).slice(0, 3).map(([k, v]) => pill(k, v));
  if (isAgent(c) && !("review" in c.checks)) checks.push('<span class="pill pend">review</span>');
  const tags = (c.parents.length > 1 ? ' <span class="pill merge">merge</span>' : "") +
    (c.conflicts ? ` <span class="pill pend">${plural(c.conflicts, "conflict")}</span>` : "");
  return `<button class="grid row" data-act="open" data-id="${esc(c.id)}">
    <span class="who-cell">${avatar(isAgent(c) ? c.agent : c.author, isAgent(c))}
      <span class="lines"><span>${esc(c.title)}${tags}</span><span>${esc(meta.filter(Boolean).join(" · "))}</span></span></span>
    <span class="checks">${checks.join("")}</span>
    <span class="right mono small">${s ? counts(s[0], s[1], " / ") : ""}</span>
    <span class="right small" style="color: var(--muted)">${ago(c.time)}</span></button>`;
}

const changeTable = (rows) =>
  `<div class="scroll-x"><div class="wide-table"><div class="grid cols"><span>Change</span><span>Checks</span>
    <span class="right">Changes</span><span class="right">Updated</span></div>${rows.map(changeRow).join("")}</div></div>`;

function fileRow(f, cls = "file") {
  const path = f.kind === "R" ? `${f.from} → ${f.path}` : f.path;
  return `<button class="${cls}${V.file === f.path && cls !== "file" ? " on" : ""}" data-act="file" data-path="${esc(f.path)}">
    <span class="badge k-${esc(f.kind)}">${esc(f.kind)}</span><span class="path">${esc(path)}</span>
    <span class="counts">${counts(f.add, f.del)}</span></button>`;
}

// ---------------------------------------------------------------------------
// Sidebar

function sidebar() {
  if (!S) return "";
  const nav = (tab, ic, name, n, warn = false) =>
    `<button class="nav${V.tab === tab ? " on" : ""}" data-act="tab" data-tab="${tab}">
      <span class="${warn ? "warn-ic" : ""}" style="display:flex">${icon(ic)}</span>${name}
      ${n ? `<span class="n${warn ? " warn" : ""}">${n}</span>` : ""}</button>`;
  const ws = workers();
  const palette = ["#3fb950", "#5b8def", "#8b8b93"];
  const people = ws
    .map((w) => {
      const color = w.agent ? palette[hash(w.name) % 3] : "#a371f7";
      return `<button class="worker" data-act="open" data-id="${esc(w.id)}" title="open the latest change">
        <span class="dot" style="background:${color}" aria-hidden="true"></span>
        <span class="who"><span>${esc(w.name)}</span><span>${esc(w.latest)}</span></span>
        <span class="small muted">${w.count}</span></button>`;
    })
    .join("");
  const mac = navigator.platform.includes("Mac");
  return `
    <div class="repo">
      <div class="logo">${icon("sprout", 14, "#5b8def")}</div>
      <div class="repo-name"><b>${esc(S.name)}</b><span title="${esc(S.root)}">${esc(S.root)}</span></div>
    </div>
    <div class="new">
      <button class="wide" data-act="tab" data-tab="changes" data-focus="msg">${icon("plus", 14)} New change<span class="kbd">${mac ? "⌘N" : "Ctrl+N"}</span></button>
    </div>
    ${nav("inbox", "inbox", "Inbox", unreviewed().length + S.conflicts.length)}
    ${nav("changes", "diff", "Changes", S.changes.length)}
    ${nav("history", "clock", "History", 0)}
    ${nav("conflicts", "warn", "Conflicts", S.conflicts.length, S.conflicts.length > 0)}
    ${nav("settings", "gear", "Settings", 0)}
    ${ws.length ? `<div class="label">This week</div>${people}` : ""}
    ${syncCard()}`;
}

function syncCard() {
  const busy = V.busy ? "disabled" : "";
  const remotes = S.remotes
    .map((rm) => `<div class="row"><b>${esc(rm)}</b><span style="margin-left:auto;display:flex;gap:6px">
      <button class="btn" data-act="run" data-args='${esc(JSON.stringify(["pull", rm]))}' ${busy}>Pull</button>
      <button class="btn soft" data-act="run" data-args='${esc(JSON.stringify(["push", rm]))}' ${busy}>Push</button></span></div>`)
    .join("");
  if (!S.github || V.githubEdit !== null) {
    return `<div class="sync"><div class="row"><b>GitHub</b></div>
      <input type="text" id="gh" placeholder="git@github.com:you/repo.git" value="${esc(V.githubEdit ?? S.github)}" aria-label="GitHub repository URL">
      <div class="pair">${S.github ? '<button class="btn" data-act="gh-cancel">Cancel</button>' : ""}
        <button class="btn primary" data-act="gh-set" ${busy}>Connect</button></div>${remotes}</div>`;
  }
  const name = S.github.replace(/\.git$/, "").split(/[/:]/).slice(-2).join("/");
  const ab = S.ahead_behind;
  const line = ab ? `${esc(S.branch)} is ${ab[0]} ahead, ${ab[1]} behind` : "not pulled yet";
  return `<div class="sync">
    <div class="row"><b title="${esc(S.github)}">${esc(name)}</b>
      <button class="ghost small" style="margin-left:auto;padding:2px 6px" data-act="gh-edit" title="change the repository">${S.github.startsWith("http") ? "https" : "ssh"}</button></div>
    <div class="small" style="color: var(--muted)">${line}</div>
    <div class="pair"><button class="btn soft" data-act="run" data-name="GitHub pull" data-args='["github","pull"]' title="fetch from GitHub and merge" ${busy}>Pull</button>
      <button class="btn primary" data-act="run" data-name="GitHub push" data-args='["github","push"]' title="push the current branch to GitHub" ${busy}>Push${ab && ab[0] ? ` ${ab[0]}` : ""}</button></div>
    ${remotes}</div>`;
}

// ---------------------------------------------------------------------------
// Header

function header() {
  if (!S) return "";
  if (V.tab === "history" && V.change) {
    const c = V.detail?.row ?? S.history.find((h) => h.id === V.change);
    return `<div class="crumb"><button class="back" data-act="back">${icon("left", 14)} History</button>
      <span style="color:#3a3a40">/</span><h1>${esc(c?.title ?? "")}</h1><span class="mono small muted">${esc(c?.short ?? "")}</span></div>`;
  }
  const title = { inbox: "Inbox", changes: "Changes", history: "History", conflicts: "Conflicts", settings: "Settings" }[V.tab];
  const seg = ["inbox", "history"].includes(V.tab)
    ? `<div class="seg" role="group" aria-label="Show">${["all", "agents", "people"]
        .map((w) => `<button class="${V.who === w ? "on" : ""}" aria-pressed="${V.who === w}" data-act="who" data-who="${w}">${w[0].toUpperCase() + w.slice(1)}</button>`)
        .join("")}</div>`
    : "";
  const commit = V.tab !== "changes" && S.changes.length
    ? `<button class="btn primary" data-act="tab" data-tab="changes" data-focus="msg">${icon("check", 13)} Commit ${plural(S.changes.length, "file")}</button>`
    : "";
  return `<h1>${title}</h1>${branchMenu()}${seg}
    <div class="end"><button class="btn" data-act="run" data-name="undo" data-args='["undo"]' title="undo the last operation (again = redo)" ${V.busy ? "disabled" : ""}>${icon("undo", 13)} Undo</button>${commit}</div>`;
}

function branchMenu() {
  const names = S.branches.includes(S.branch) ? S.branches : [S.branch, ...S.branches];
  const others = S.branches.filter((b) => b !== S.branch);
  const items = names
    .map((b) => `<button class="item${b === S.branch ? " on" : ""}" ${b === S.branch ? "" : `data-act="run" data-name="checkout ${esc(b)}" data-args='${esc(JSON.stringify(["checkout", b]))}'`}>
      ${icon("branch", 13, b === S.branch ? "#5b8def" : "#a1a1aa")}${esc(b)}${b === S.branch ? `<span class="check">${icon("check", 12)}</span>` : ""}</button>`)
    .join("");
  const create = S.head
    ? `<div class="create"><input type="text" id="newbranch" placeholder="new branch" value="${esc(V.newBranch)}" aria-label="New branch name">
        <button class="btn soft" data-act="branch-create">Create</button></div>`
    : '<div class="note">Commit first, then you can create branches</div>';
  const merges = others.length
    ? others.map((b) => `<button class="item" data-act="run" data-name="merge ${esc(b)}" data-args='${esc(JSON.stringify(["merge", b]))}'>${icon("merge", 13, "#a371f7")}${esc(b)}</button>`).join("")
    : '<div class="note">No other branches</div>';
  const menu = V.menu
    ? `<div class="menu" role="menu"><div class="label">Switch branch</div>${items}${create}<hr>
        <div class="label">Merge into ${esc(S.branch)}</div>${merges}<hr>
        <button class="item" data-act="refresh">${icon("refresh", 13, "#a1a1aa")}Refresh</button></div>`
    : "";
  return `<div class="menu-wrap"><button class="chip" data-act="menu" aria-expanded="${V.menu}">${icon("branch", 12)}${esc(S.branch)}${icon("down", 12, "#8b8b93")}</button>${menu}</div>`;
}

// ---------------------------------------------------------------------------
// Pages

function inbox() {
  const out = [];
  if (S.conflicts.length) {
    const head = S.history[0]?.title ?? "HEAD";
    const files = S.conflicts.slice(0, 3).map((p) => `<span class="mono">${esc(p)}</span>`).join(", ");
    out.push(`<section class="banner" aria-label="Conflicts">${icon("warn", 16, "#d29922")}
      <div class="text"><b>${esc(head)} recorded ${plural(S.conflicts.length, "conflict")}</b>
        <span>The merge completed. ${files}${S.conflicts.length > 3 ? ", …" : ""} carry markers until resolved.</span></div>
      <button class="btn outline-warn" data-act="theirs" ${V.busy ? "disabled" : ""} title="resolve --theirs every conflicted file">Take theirs</button>
      <button class="btn warn" data-act="tab" data-tab="conflicts">Resolve</button></section>`);
  }
  const [a, d] = S.changes.reduce(([a, d], f) => [a + f.add, d + f.del], [0, 0]);
  const base = S.head ? `uncommitted against HEAD ${esc(S.head.slice(0, 7))}` : "no commits yet";
  const files = S.changes.slice(0, 12).map((f) => fileRow(f)).join("");
  const more = S.changes.length > 12 ? `<button class="file" data-act="tab" data-tab="changes"><span class="muted">${S.changes.length - 12} more…</span></button>` : "";
  out.push(section(S.changes.length, "Working tree", `<span>${base}</span><span class="mono">${counts(a, d)}</span>`,
    S.changes.length ? files + more : '<div class="empty">Clean: nothing to commit</div>'));

  const review = unreviewed().filter(shown).slice(0, 10);
  out.push(section(review.length, "Agent changes without a passing review", '<span class="mono">audit --require review</span>',
    review.length ? changeTable(review) : '<div class="empty">Every agent change has a passing review</div>'));

  const recent = S.history.filter(shown).slice(0, 8);
  const landed = recent
    .map((c) => `<button class="landed" data-act="open" data-id="${esc(c.id)}">${icon("merge", 14, "#a371f7")}
      <span class="t">${esc(c.title)}</span><span class="m mono">${esc(c.short)}</span>
      <span class="m">${esc(isAgent(c) ? c.agent : c.author)}</span><span class="time">${ago(c.time)}</span></button>`)
    .join("");
  out.push(section(recent.length, `Recent on ${S.branch}`, '<a href="#" data-act="tab" data-tab="history">All history</a>',
    recent.length ? landed : '<div class="empty">No changes yet</div>'));
  fillStats(review.map((c) => c.id));
  return `<div class="content">${out.join("")}</div>`;
}

function changesPage() {
  const list = S.changes.length
    ? `<div class="filelist">${S.changes.map((f) => fileRow(f, "item")).join("")}</div>`
    : '<div class="empty">Working tree clean</div>';
  const what = V.file ? V.file : plural(S.changes.length, "file");
  const can = !V.busy && V.message.trim() && (S.changes.length || S.conflicts.length);
  return `<div class="split-page"><div class="left">${list}
      <div class="commit"><textarea id="msg" rows="3" placeholder="Commit message" aria-label="Commit message">${esc(V.message)}</textarea>
        <button class="btn primary" data-act="commit" ${can ? "" : "disabled"}>Commit ${esc(what)}</button></div></div>
    <div class="right">${diffView(V.diff, true)}</div></div>`;
}

function historyPage() {
  if (V.change) return changePage();
  const rows = S.history.filter(shown);
  fillStats(rows.slice(0, 60).map((c) => c.id));
  return `<div class="content">${section(rows.length, `Changes on ${S.branch}`, "",
    rows.length ? changeTable(rows) : '<div class="empty">No changes yet</div>')}</div>`;
}

function changePage() {
  const D = V.detail;
  if (!D) return '<div class="empty">Loading…</div>';
  const c = D.row;
  const agent = isAgent(c);
  const checks = Object.entries(c.checks);
  if (agent && !("review" in c.checks)) checks.push(["review", "required by policy"]);
  const checkRows = checks.length
    ? checks
        .map(([k, v]) => {
          const t = tone(v);
          const mark = t === "pass" ? icon("check", 11) : t === "fail" ? icon("cross", 11) : "!";
          const known = t !== "pend" || passing(v) || failing(v);
          return `<div class="c"><span class="ci ${t}" aria-hidden="true">${mark}</span>
            <span class="name">${esc(known ? `${k}=${v}` : k)}</span><span class="note">${known ? "" : esc(v)}</span></div>`;
        })
        .join("")
    : '<div class="c"><span class="note" style="margin-left:0">No checks recorded</span></div>';
  const sig = D.signature.Ok !== undefined
    ? D.trusted
      ? ["ok", "Signed by a trusted key", D.signature.Ok]
      : ["warn", "Signed, but not by a trusted key", D.signature.Ok]
    : D.signature.Err === "unsigned"
      ? ["", "Unsigned", "sign your changes with an SSH or sprig key"]
      : ["bad", "Bad signature", D.signature.Err];
  const sub = [c.model, ago(c.time, true), `on ${S.branch}`].filter(Boolean).join(" · ");
  const left = `
    <div class="byline">${avatar(agent ? c.agent : c.author, agent)}
      <div><span><b>${esc(agent ? c.agent : c.author)}</b>${agent ? ` <span class="muted">for ${esc(c.author)}</span>` : ""}</span>
        <small title="${esc(c.date)}">${esc(sub)}</small></div></div>
    <div><h2 class="title-lg">${esc(c.title)}</h2>${c.body ? `<p class="muted" style="white-space:pre-wrap;margin:6px 0 0">${esc(c.body)}</p>` : ""}</div>
    ${c.intent ? `<div class="group"><span class="heading">Intent</span><p class="intent">${esc(c.intent)}</p></div>` : ""}
    <div class="group"><span class="heading">Checks</span><div class="checklist">${checkRows}</div></div>
    <div class="group"><span class="heading">Signature</span>
      <div class="sig ${sig[0]}">${icon("shield", 15)}<div><b>${esc(sig[1])}</b><span title="${esc(sig[2])}">${esc(sig[2])}</span></div>
        ${V.signing && !V.signing.trusted ? '<button class="btn" style="margin-left:auto" data-act="keys">Set up signing</button>' : ""}</div></div>
    <div class="group"><span class="heading">Change</span><div class="facts">
      <span class="mono" style="color:var(--text)">${esc(c.short)}</span>
      <button class="btn" style="height:24px;padding:0 8px" data-act="copy" data-text="${esc(c.id)}">Copy id</button>
      ${c.parents.length > 1 ? '<span class="pill merge">merge</span>' : ""}
      ${c.parents.length ? `<span>parent ${esc(c.parents.join(", "))}</span>` : ""}
      ${c.conflicts ? `<span class="pill pend">${plural(c.conflicts, "conflict")}</span>` : ""}</div></div>`;
  return `<div class="split-page"><aside class="left" aria-label="Provenance">${left}</aside>
    <div class="right">${diffView(D.diff, false)}</div></div>`;
}

function settingsPage() {
  const k = V.signing;
  if (!k) return '<div class="content"><div class="empty">Loading…</div></div>';
  const env = k.env_author
    ? `<p class="note">SPRIG_AUTHOR is set (<span class="mono">${esc(k.env_author)}</span>) and wins over this.</p>`
    : "";
  const author = `<form class="form" onsubmit="return false">
    <label>Name<input type="text" id="a-name" value="${esc(k.name)}" autocomplete="name"></label>
    <label>Email<input type="text" id="a-email" value="${esc(k.email)}" autocomplete="email" inputmode="email"></label>
    ${env}<div class="end"><button class="btn primary" data-act="author">Save author</button></div></form>`;
  const current = k.key
    ? `<div class="sig ${k.trusted ? "ok" : "warn"}">${icon("shield", 15)}<div><b>${k.trusted ? "Signing; trusted in this repository" : "Signing; not trusted in this repository yet"}</b>
        <span title="${esc(k.key)}">${esc(k.key)}</span><span title="${esc(k.key_file)}">${esc(k.key_file)}</span></div>
        ${k.trusted ? "" : '<button class="btn" style="margin-left:auto" data-act="key-trust">Trust it</button>'}</div>`
    : '<div class="sig">' + icon("shield", 15) + "<div><b>Not signing</b><span>pick a key below</span></div></div>";
  const signing = `<div class="form">${current}
    <span class="heading">${k.key ? "Switch to" : "Sign with"}</span>${keyPicker(k)}
    <p class="note">An ed25519 SSH key without a passphrase works as is. Switching moves a sprig key aside to signing.key.old.</p>
    <div class="end"><button class="btn primary" data-act="key-replace">${k.key ? "Switch key" : "Use this key"}</button></div></div>`;
  return `<div class="content" style="max-width:720px">${section("", "Commit author", "<span>this repository</span>", author)}
    ${section("", "Signing key", "<span>all repositories</span>", signing)}</div>`;
}

function conflictsPage() {
  if (!S.conflicts.length) return '<div class="content"><div class="empty">No unresolved conflicts</div></div>';
  const chips = S.conflicts
    .map((p) => `<button class="fchip${p === V.conflict ? " on" : ""}" data-act="conflict" data-path="${esc(p)}"><span>${esc(p)}</span></button>`)
    .join("");
  const x = V.sides;
  if (!x) return `<div class="content"><div class="fchips">${chips}</div><div class="empty">Loading…</div></div>`;
  const side = (name, what, text, color, primary) => `<div class="side">
    <div class="box"><div class="fh"><b style="color:${color}">${name}</b><span class="small muted">${what}</span></div><pre>${esc(text)}</pre></div>
    <button class="btn ${primary ? "primary" : ""}" data-act="run" data-name="resolve --${name}" data-args='${esc(JSON.stringify(["resolve", `--${name}`, V.conflict]))}' ${V.busy ? "disabled" : ""}>Use ${name}</button></div>`;
  const markers = x.working.includes("<<<<<<<");
  return `<div class="content" style="max-width:none">
    <div class="banner">${icon("warn", 16, "#d29922")}<div class="text"><b>${plural(S.conflicts.length, "conflict")} to resolve</b>
      <span>Keep a whole side, or edit the merged result below and save it; then commit.</span></div></div>
    <div class="fchips">${chips}</div>
    <div class="sides">${side("ours", "current branch", x.ours, "#5b8def", true)}${side("base", "common ancestor", x.base, "#8b8b93", false)}${side("theirs", "incoming", x.theirs, "#3fb950", true)}</div>
    <div class="difftop"><b>Merged result</b><span class="muted">in the working tree</span>
      ${markers ? '<span class="pill fail">still has conflict markers</span>' : ""}
      <button class="btn primary" style="margin-left:auto" data-act="save" ${V.busy ? "disabled" : ""}>Save result</button></div>
    <textarea class="editor" id="editor" spellcheck="false" aria-label="Merged result">${esc(x.working)}</textarea></div>`;
}

// ---------------------------------------------------------------------------
// Diff

const splitSwitch = () => `<div class="seg" role="group" aria-label="Layout">
  <button class="${V.split ? "" : "on"}" aria-pressed="${!V.split}" data-act="split" data-split="0">Unified</button>
  <button class="${V.split ? "on" : ""}" aria-pressed="${V.split}" data-act="split" data-split="1">Split</button></div>`;

const MAX_LINES = 4000; // per view; a lockfile alone can be thousands

function diffView(files, working) {
  if (!files.length) return '<div class="empty">Nothing to show</div>';
  const [a, d] = files.reduce(([a, d], f) => [a + f.add, d + f.del], [0, 0]);
  const top = `<div class="difftop"><b>${plural(files.length, "file")} changed</b><span class="mono">${counts(a, d)}</span>${splitSwitch()}</div>`;
  const chips = `<div class="fchips">${files.slice(0, 12)
    .map((f) => `<button class="fchip${V.file === f.path ? " on" : ""}" ${working ? `data-act="file" data-path="${esc(f.path)}"` : `data-act="jump" data-path="${esc(f.path)}"`}>
      <span>${esc(f.path)}</span>${counts(f.add, f.del)}</button>`)
    .join("")}${files.length > 12 ? `<span class="small muted" style="align-self:center">+${files.length - 12} more</span>` : ""}</div>`;
  let budget = MAX_LINES;
  const body = files
    .map((f) => {
      const notes = f.notes.map((n) => `<div class="more">${esc(n)}</div>`).join("");
      let hunks = "";
      for (const h of f.hunks) {
        if (budget <= 0) {
          hunks += '<div class="more">Diff cut short here; pick the file to see it alone.</div>';
          break;
        }
        budget -= h.lines.length;
        hunks += `<div class="hh">${esc(h.header)}</div>${V.split ? splitHunk(h.lines) : unifiedHunk(h.lines)}`;
      }
      return `<details class="dfile" data-file="${esc(f.path)}" ${fold(`f:${f.path}`)}><summary class="fh">${icon("down", 12, "#8b8b93")}<span class="p">${esc(f.path)}</span>
        <span class="mono small">${counts(f.add, f.del)}</span></summary>${notes}${hunks}</details>`;
    })
    .join("");
  return top + chips + body;
}

const cls = (op) => (op === "+" ? "a" : op === "-" ? "d" : "");
const num = (n) => `<span class="num">${n ?? ""}</span>`;

function unifiedHunk(lines) {
  return `<div class="code">${lines
    .map(([o, n, op, t]) => `<div class="l ${cls(op)}">${num(o)}${num(n)}<span class="sign">${op === " " ? "" : op}</span><span class="t">${esc(t)}</span></div>`)
    .join("")}</div>`;
}

/// Side by side: each run of removals is paired with the additions after it.
function splitHunk(lines) {
  const L = [], R = [];
  let dels = [], adds = [];
  const flush = () => {
    const n = Math.max(dels.length, adds.length);
    for (let i = 0; i < n; i++) {
      L.push(dels[i] ?? null);
      R.push(adds[i] ?? null);
    }
    dels = [];
    adds = [];
  };
  for (const l of lines) {
    if (l[2] === "-") dels.push(l);
    else if (l[2] === "+") adds.push(l);
    else {
      flush();
      L.push(l);
      R.push(l);
    }
  }
  flush();
  const col = (xs, side) => `<div class="code">${xs
    .map((l) => (l ? `<div class="l ${cls(l[2])}">${num(l[side])}<span class="sign">${l[2] === " " ? "" : l[2]}</span><span class="t">${esc(l[3])}</span></div>`
      : '<div class="l pad"><span class="num"></span><span class="t"> </span></div>'))
    .join("")}</div>`;
  return `<div class="splitcode">${col(L, 0)}${col(R, 1)}</div>`;
}

// ---------------------------------------------------------------------------
// Drawing and events

function render() {
  if (!S) {
    $("side").innerHTML = "";
    $("top").innerHTML = "<h1>Not a sprig repository</h1>";
    $("page").innerHTML = `<div class="content"><p class="muted">${esc(V.status.text)}</p>
      <p>Run <span class="mono">sprig init</span> there (or <span class="mono">sprig init</span> + <span class="mono">sprig github set &lt;url&gt;</span> + Pull), or start sprig-ui with a repository path.</p></div>`;
    return;
  }
  const scroll = $("page").scrollTop;
  $("side").innerHTML = sidebar();
  $("top").innerHTML = header();
  $("page").innerHTML = { inbox, changes: changesPage, history: historyPage, conflicts: conflictsPage, settings: settingsPage }[V.tab]();
  $("page").scrollTop = scroll;
  const at = S.head ? `${S.branch} at ${S.head.slice(0, 7)}` : "no changes yet";
  $("status").innerHTML = V.busy
    ? `<span class="spin" aria-hidden="true"></span><span>${esc(V.busy)}…</span><span class="at">${esc(at)}</span>`
    : `<span class="${V.status.ok ? "" : "err"}" title="${esc(V.status.text)}">${esc(V.status.text.split("\n")[0])}</span><span class="at">${esc(at)}</span>`;
}

async function go(tab, focus) {
  V.tab = tab;
  V.change = null;
  V.menu = false;
  if (tab === "changes") await loadWorking();
  if (tab === "conflicts") await loadConflict(V.conflict ?? S.conflicts[0] ?? null);
  if (tab === "settings") await loadSigning();
  render();
  if (focus) $(focus)?.focus();
}

document.addEventListener("click", async (ev) => {
  const el = ev.target.closest("[data-act]");
  if (!el) {
    if (V.menu && !ev.target.closest(".menu")) {
      V.menu = false;
      render();
    }
    return;
  }
  ev.preventDefault();
  const d = el.dataset;
  switch (d.act) {
    case "tab": return go(d.tab, d.focus);
    case "who": V.who = d.who; return render();
    case "split": V.split = d.split === "1"; return render();
    case "menu": V.menu = !V.menu; return render();
    case "refresh": V.menu = false; return refresh();
    case "open": return openChange(d.id);
    case "back": V.change = null; return render();
    case "file":
      V.file = V.tab === "changes" && V.file === d.path ? null : d.path; // a second click goes back to all
      return go("changes");
    case "jump": return document.querySelector(`.dfile[data-file="${CSS.escape(d.path)}"]`)?.scrollIntoView({ block: "start" });
    case "run": return act(d.name ?? JSON.parse(d.args).join(" "), JSON.parse(d.args));
    case "commit": {
      const args = ["commit", "-m", V.message.trim()];
      if (!S.changes.length) args.push("--allow-empty"); // recording a resolution that matches HEAD
      if (V.file) args.push(V.file);
      return act("commit", args);
    }
    case "branch-create": {
      const b = V.newBranch.trim();
      if (!b) return;
      V.newBranch = "";
      return act(`checkout -b ${b}`, ["checkout", "-b", b]);
    }
    case "theirs": return act("resolve --theirs", ["resolve", "--theirs", ...S.conflicts]);
    case "conflict": await loadConflict(d.path); return render();
    case "save": {
      const text = $("editor").value;
      try {
        await invoke("save", { path: V.conflict, text });
        V.status = { ok: true, text: `saved ${V.conflict}` };
      } catch (e) {
        V.status = { ok: false, text: String(e) };
      }
      return refresh();
    }
    case "gh-edit": V.githubEdit = S.github; return render();
    case "gh-cancel": V.githubEdit = null; return render();
    case "gh-set": {
      const url = ($("gh")?.value ?? "").trim();
      if (!url) return;
      V.githubEdit = null;
      return act("github set", ["github", "set", url]);
    }
    case "keys": return openKeys();
    case "keys-use": return useKey();
    case "key-replace": return useKey(true);
    case "key-trust": return useKey(false);
    case "author": {
      try {
        await invoke("set_author", { name: $("a-name").value, email: $("a-email").value });
        V.status = { ok: true, text: "author saved" };
      } catch (e) {
        V.status = { ok: false, text: String(e) };
      }
      await loadSigning();
      return render();
    }
    case "keys-later":
      try {
        localStorage.setItem("sprig.keys-later", "1");
      } catch {}
      return $("keys").close();
    case "copy": navigator.clipboard?.writeText(d.text); V.status = { ok: true, text: "copied" }; return render();
  }
});

// Folding a section or a file; toggle doesn't bubble, so listen on the way down.
document.addEventListener(
  "toggle",
  (ev) => {
    const k = ev.target.dataset?.key;
    if (k) ev.target.open ? V.closed.delete(k) : V.closed.add(k);
  },
  true,
);

// Typing keeps its text in V without redrawing, so the field keeps focus.
document.addEventListener("input", (ev) => {
  const id = ev.target.id;
  if (id === "msg") {
    V.message = ev.target.value;
    const b = document.querySelector('[data-act="commit"]');
    if (b) b.disabled = !(!V.busy && V.message.trim() && (S.changes.length || S.conflicts.length));
  }
  if (id === "newbranch") V.newBranch = ev.target.value;
  if (id === "gh") V.githubEdit = ev.target.value;
});

document.addEventListener("keydown", (ev) => {
  if ((ev.metaKey || ev.ctrlKey) && ev.key.toLowerCase() === "n") {
    ev.preventDefault();
    go("changes", "msg");
  }
  if (ev.key === "Escape" && V.menu) {
    V.menu = false;
    render();
  }
  if (ev.key === "Enter" && ev.target.id === "newbranch") document.querySelector('[data-act="branch-create"]')?.click();
  if (ev.key === "Enter" && (ev.metaKey || ev.ctrlKey) && ev.target.id === "msg") document.querySelector('[data-act="commit"]')?.click();
});

// ---------------------------------------------------------------------------
// Signing: a dialog to sign with an SSH key or a new sprig key

async function loadSigning() {
  try {
    V.signing = await invoke("settings");
  } catch {
    V.signing = null;
  }
}

/// Radio options: each ed25519 SSH key, then a new sprig key.
function keyPicker(k) {
  const keys = k.ssh
    .map(([path, comment], i) => `<label class="opt"><input type="radio" name="key" value="${esc(path)}" ${i === 0 ? "checked" : ""}>
      <span><b class="mono">${esc(path.replace(/^.*\//, ""))}</b><small>${esc(comment || path)}</small></span></label>`)
    .join("");
  return `<div class="opts">${keys}<label class="opt"><input type="radio" name="key" value="" ${k.ssh.length ? "" : "checked"}>
    <span><b>Create a new sprig key</b><small>sprig keygen</small></span></label></div>`;
}

function openKeys(error = "") {
  const k = V.signing;
  if (!k) return;
  const mine = k.key
    ? `<p class="muted">You sign with <span class="mono">${esc(k.key.slice(0, 24))}…</span>; this repository doesn't trust it yet.</p>`
    : keyPicker(k);
  $("keys").innerHTML = `<form method="dialog">
    <h2>${icon("shield", 16, "#3fb950")} Sign your changes</h2>
    <p class="muted">Signed changes show who really made them, and <span class="mono">sprig audit --signed</span> checks them.
      ${k.key ? "" : "An ed25519 SSH key without a passphrase works as is."}</p>
    ${mine}${error ? `<p class="err">${esc(error)}</p>` : ""}
    <div class="end"><button class="btn" value="cancel" data-act="keys-later">Not now</button>
      <button class="btn primary" data-act="keys-use">${k.key ? "Trust my key here" : "Use this key"}</button></div></form>`;
  if (!$("keys").open) $("keys").showModal();
}

async function useKey(replace = false) {
  const root = replace ? $("page") : $("keys");
  const picked = root.querySelector('input[name="key"]:checked');
  try {
    const key = await invoke("setup_signing", { ssh: picked?.value || null, replace });
    if ($("keys").open) $("keys").close();
    V.status = { ok: true, text: `signing with ${key}` };
    await loadSigning();
    if (V.change) await openChange(V.change);
    render();
  } catch (e) {
    if (replace) {
      V.status = { ok: false, text: String(e) };
      render();
    } else openKeys(String(e));
  }
}

// Files may have changed while the window was in the back.
window.addEventListener("focus", () => V.busy || refresh());

refresh().then(loadSigning).then(() => {
  let later = false;
  try {
    later = localStorage.getItem("sprig.keys-later") === "1";
  } catch {}
  if (S && V.signing && !V.signing.key && !later) openKeys();
  render();
});
