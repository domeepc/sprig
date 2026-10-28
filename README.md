# sprig

A small version control system built for a world where most commits come
from agents. Storage is content-addressed and distributed like git; a server
exposes the repository as an API, so always-online tools can ask questions
without cloning; and it lives happily next to git: a `.git` in the same
folder is kept in step, and GitHub is one `sprig github push` away.

Prototype: about 10,300 lines of Rust, 2,100 of tests and a 1,600-line
desktop app page. It was first written in Go; the Rust version reads and
writes the same repositories, packs and wire protocol (importing the same
git history gives the same change ids, byte for byte).

- [Install](#install) · [Desktop app](#desktop-app) · [What's different from git](#whats-different-from-git) · [Quick tour](#quick-tour)
- [Working with git and GitHub](#working-with-git-and-github) · [Server and remotes](#server-and-remotes) · [Environment](#environment) · [Layout](#layout) · [Measured](#measured)

## Install

You need **Rust 1.89 or newer** (1.90 for the desktop app), **a C compiler**
(the tree-sitter grammars and zstd are C) and **git** (for GitHub sync,
`import-git`/`export-git` and fetching the source).

| System | Command |
|---|---|
| Fedora / RHEL | `sudo dnf install rust cargo gcc git` |
| Debian / Ubuntu | `sudo apt install gcc git`, then Rust from rustup.rs (Debian's is often too old) |
| Arch | `sudo pacman -S rust gcc git` |
| macOS | `xcode-select --install` (clang and git), then `brew install rust` |
| Windows | Rust from rustup.rs with the MSVC build tools. Untested; `sprig watch` is Unix only. |

```sh
git clone https://github.com/domeepc/sprig.git && cd sprig
cargo install --locked --path .                 # sprig, into ~/.cargo/bin
cargo install --locked --path . --features ui   # sprig and the desktop app, sprig-ui
cargo test --release                            # check the build
```

Tab completion of commands, flags, branches, remotes, conflicted paths and
trusted keys (the scripts ask `sprig __complete`, so they never go stale):

```sh
echo 'eval "$(sprig completion bash)"' >> ~/.bashrc
echo 'source <(sprig completion zsh)' >> ~/.zshrc      # after compinit
sprig completion fish > ~/.config/fish/completions/sprig.fish
```

Then, once:

- **Who you are:** set it in the app's Settings, or put `Name <email>` in
  `~/.config/sprig/author` (all repositories; a repository's own setting and
  `SPRIG_AUTHOR` win over it).
- **Signing:** `sprig keygen --ssh ~/.ssh/id_ed25519` signs your changes with
  your SSH key (ed25519; one with a passphrase signs through ssh-agent once
  you `ssh-add` it), or `sprig keygen` makes a sprig key.
- `sprig watch` in a repo you work in a lot keeps `status` fast.

Update with `git pull && cargo install --locked --path .`; uninstall with
`cargo uninstall sprig` (and delete `~/.config/sprig`).

## Desktop app

`sprig-ui` is a window on a repository for Linux, macOS and Windows: Rust
commands over the same code the CLI runs, and a page of plain HTML, CSS and
JavaScript (`ui/web`, no npm) drawn in the system webview. Every button runs
the matching command, so the app and the CLI behave the same and everything
lands in `sprig ops`.

```sh
sprig-ui path/to/repo     # or run it inside the repo
```

<!-- screenshot: docs/app-inbox.png (Inbox) -->
<!-- screenshot: docs/app-change.png (a change: provenance beside its diff) -->
<!-- screenshot: docs/app-merge.png (merge editor: ours | result | theirs) -->
<!-- screenshot: docs/app-settings.png (Settings: identity, signing keys) -->

- **Inbox:** what needs you. Conflicts HEAD recorded, the working tree with
  +N −N per file, agent changes without a passing `review` check (what
  `sprig audit` reports) and recent changes; filter by All, Agents or People.
- **Changes:** tick the files to commit, write the message, commit; the diff
  beside them is unified or side by side.
- **History:** every change; each opens as a page with its provenance
  (agent, model, intent, checks, signature, parents, conflicts) beside its diff.
- **Conflicts:** the files a merge left, what each side did and whether
  they are resolved; accept ours or theirs for several at once, or open the
  merge editor: ours, result and theirs side by side, with » Ours, Both and
  Theirs « on each conflict, or edit the result by hand.
- **Settings:** commit name and email (this repository or all), the signing
  key (any of your `~/.ssh` ed25519 keys, or a new one) and the keys this
  repository trusts. Without a signing key the app offers to set one up.
- **Branches and sync:** the branch menu switches (asking first when the
  other branch shares no history), creates and merges branches. Refresh
  fetches from GitHub and offers to delete local branches whose GitHub
  branch is gone. The sidebar shows who worked this week and how far the
  branch is ahead of or behind GitHub, with Pull and Push.

Linux needs WebKitGTK to build it: `sudo dnf install webkit2gtk4.1-devel
libsoup3-devel` or `sudo apt install libwebkit2gtk-4.1-dev libsoup-3.0-dev`
(macOS and Windows 10/11 ship their webview).

The look follows Cursor's Agents tab and Graphite's inbox; colors, radii and
type are tokens on `:root` in `ui/web/style.css`. References, from
[Mobbin](https://mobbin.com):

| Pattern | Taken from |
|---|---|
| Sidebar with the repository, pages and who is working; a change page with provenance beside the diff | Cursor Agents tab ([1](https://mobbin.com/screens/58450174-4034-4e5d-a126-9def2378b0a0), [2](https://mobbin.com/screens/44239877-86ce-46c8-a615-273ec82046f5), [3](https://mobbin.com/screens/facfdb37-d755-40d8-b4c9-40f630d826f3)) |
| Inbox of collapsible sections with a count; rows with avatar, title, checks, +N −N and age; dark canvas, bordered cards, blue primary | Graphite inbox and PR page ([1](https://mobbin.com/screens/a83d8ccd-c487-4e28-8edc-967742f33219), [2](https://mobbin.com/screens/3e2205a5-5840-410b-abb3-38ea45add32a), [3](https://mobbin.com/screens/24233aed-0981-4a40-a745-451ee3416edc)) |
| Diff with line-number gutters, tinted lines, +N −N per file, Unified/Split | GitLab commit diff ([1](https://mobbin.com/screens/95e376da-81cd-465c-a99b-a0051f9a2baf)), GitHub compare ([2](https://mobbin.com/screens/168fa0b6-a33c-4172-8fe0-984ecc63db45)), Neon ([3](https://mobbin.com/screens/9cb22cfd-3f04-4161-94b9-729266bc97b0)) |
| Merge editor: ours, result, theirs with per-conflict accept buttons | IntelliJ's merge tool (not on Mobbin); split-diff styling from Cofounder ([1](https://mobbin.com/screens/c50251b3-860c-4f81-be1d-2795eae0182b)) |
| Signing keys as a table with Default / Trusted tags | Proton keys ([1](https://mobbin.com/screens/2ee09018-18d6-4e41-8def-c7d484d9d33d)), Render and Railway settings ([2](https://mobbin.com/screens/c8aa0a45-8741-4ab9-a7c0-da832878df37), [3](https://mobbin.com/screens/1442266b-fb02-40ad-b031-030adcabe16e)) |

## What's different from git

| Idea | How sprig does it |
|---|---|
| **Provenance is structured** | A *change* (sprig's commit) carries `intent`, `agent`, `model`, and `checks` (e.g. `tests=pass`, `review=approved`) as fields, not as text in the message. `sprig log --agent X` and `sprig audit` query them. Changes can be signed (ed25519), so `audit --signed` can tell who really made them. |
| **Conflicts are data** | A conflicted merge still *completes*. The conflicts are recorded inside the merge change, the files get markers, and you keep working. `sprig conflicts` lists them; they carry forward until the file is fixed. |
| **Everything is undoable** | Every command that moves a branch or HEAD is written to an operation log with the before/after state. `sprig undo` reverts the last one, including merges, and undoing an undo redoes it. |
| **API first** | `sprig serve` exposes refs, history, files and directory listings as JSON/HTTP. A CI job can read `main:src/x.go` with one GET. Every CLI command that reads has `--json`. |
| **Server-side landing** | `sprig land` (or one POST) sends just the edited files; the server merges them onto the current tip and retries if someone landed first. Agents need no clone, and nothing queues behind a global lock. |
| **Server-side policy** | The server refuses ref updates that are non-fast-forward, point at an unknown change, or whose tip has unresolved conflicts. Objects are hash-verified and only accepted once everything they reference is present, so a ref can never point at partial history. |
| **No index** | No staging area. `commit` snapshots the working tree (minus `.sprigignore`); `commit -m msg <path>...` commits only those paths, and `--hunk path:N` only some hunks of a file. |
| **SHA-256 everywhere** | Objects are `blob`, `tree`, `change`, addressed by SHA-256 and verified on every read, including ones rebuilt from deltas in a pack. |
| **Merges follow renames** | Renames are detected by content (`-M` sets the similarity, default 50%). When one side renames a file and the other edits it, the edit lands in the renamed file; files added to a directory the other side moved follow it. `land` does the same against renames that landed after your base. |
| **Merges understand code** | When a line merge conflicts, sprig retries per import and per top-level declaration for Go, Python, JavaScript/JSX, TypeScript/TSX, Rust and Java (tree-sitter), and per member inside a class, `impl` or Java type both sides changed. Two sides adding imports or functions merge cleanly; the same function changed both ways still conflicts. The result must parse without errors. |

## Quick tour

```sh
sprig init
echo hello > a.txt
SPRIG_AGENT=claude sprig commit -m "add greeting" \
    --intent "user asked for a greeting" --model sonnet --check tests=pass

sprig checkout -b feature    # uncommitted edits come along
...edit, commit...
sprig checkout main
sprig merge feature          # exit code 3 if conflicts were recorded
sprig conflicts
sprig resolve --theirs a.txt
sprig commit -m "resolve"
sprig undo                   # back to the conflicted state
sprig undo                   # and forward again

sprig audit --require review # agent-authored changes without a passing review
sprig commit -m "docs only" README.md docs/   # commit just these paths
sprig diff                                     # hunks are numbered: "@@ ... @@ hunk 2"
sprig commit -m "one fix" --hunk main.go:2     # commit just that hunk
sprig gc [--repack]                            # pack objects, deltas between versions
sprig watch &                                  # optional: status re-checks only changed files
```

### Landing (server-side commit)

Instead of commit + pull + push, `sprig land` sends only the files you
changed and the change you edited against. The server three-way merges them
onto the branch's current tip, so a stale base is fine unless someone else
touched the same lines. Only the final compare-and-swap of the ref is
serialized; if another landing got in first, the server re-merges onto the
new tip and tries again. Your branch and working tree then move to the
landed change.

```sh
echo fix >> a.txt
sprig land -m "fix a" --intent "flaky test" --check tests=pass
```

Agents and CI don't need a working copy at all. They can read files with
`GET /v1/file` and land with one request:

```sh
curl -X POST $SERVER/v1/land -H "Authorization: Bearer $SPRIG_TOKEN" -d '{
  "branch": "main", "base": "<id from GET /v1/refs>", "message": "bump version",
  "author": "ci <ci@example.com>", "meta": {"agent": "ci"},
  "files": {"VERSION": {"mode": "file", "data": "MS4yLjAK"}, "old.txt": null}}'
```

`data` is base64 and `null` deletes. A conflict returns HTTP 409 with the
paths and the current tip, and nothing lands. Landed changes are linear: one
parent, never a merge. The server only looks up the edited paths and rewrites
the directories along them, so a landing costs about the same whatever the
size of the repository.

### Scaling out: several servers and caches

The ref swap uses an OS file lock (flock on Unix, LockFileEx on Windows), so
several `sprig serve` processes can share one repository directory and land
concurrently. A served repository is durable: object and ref writes are
fsynced before a push or landing is acknowledged, and every ref change is
appended to `reflog.jsonl` (time, ref, old and new id, how, by whom).

A cache server keeps its own copy of objects, which never change once written,
and passes most other requests (refs, log, files) through to the main server.
It does landings itself: it reads the tip from the main server, merges using
its own object copy, uploads only the objects the landing created, and sends
one compare-and-swap of the ref. So caches on any number of machines can take
landing load, and the main server only checks and moves refs:

```sh
sprig init --bare cache.sprig
sprig serve --addr 127.0.0.1:7701 --upstream http://main-host:7700 cache.sprig
```

Point clients at the cache. It checks tokens the same way the main server
does (`--token`, `--tokens`) and fetches from upstream with `SPRIG_TOKEN`.
It keeps a copy of upstream's refs current by long-polling `/v1/refs`, so a
landing starts from the right tip without an extra round trip.
`--upstream` takes a comma-separated list; requests go to the first server
that answers.

A standby mirrors another server read-only, following its refs as they
change:

```sh
sprig init --bare standby.sprig
sprig serve --addr 0.0.0.0:7700 --follow http://main-host:7700 standby.sprig
```

It serves reads and clones and refuses writes (HTTP 503). If the main server
dies, restart the standby without `--follow` and it becomes the main server;
caches listed with `--upstream main,standby` switch over by themselves.

### Partial clones

```sh
sprig clone --path services/api http://127.0.0.1:7700 work
```

This downloads history and directory trees but no file contents. Only files
under `--path` are written to disk; leave `--path` out to check out
everything. Any file content sprig needs later (diff, merge, show) is
downloaded on first use and kept. A merge conflict in a file outside the view
adds that file to the view, so it can be resolved. Files outside the view count as unchanged,
so `commit`, `land` and `push` work as usual.

Change the view later with `sprig sparse <path>...`, or `sprig sparse .` to
check out everything.

### Signed changes

```sh
sprig keygen                       # writes ~/.config/sprig/signing.key (or $SPRIG_KEY)
sprig keygen --ssh ~/.ssh/id_ed25519   # or sign with your SSH key (ed25519)
sprig trust ed25519:AbC... alice   # keys audit --signed accepts
sprig trust "$(cat ~/.ssh/id_ed25519.pub)" alice   # a .pub line works too
sprig audit --signed --require review
```

An SSH key without a passphrase is read directly; one with a passphrase is
signed with by ssh-agent (`ssh-add` it first; sprig asks the agent over
`SSH_AUTH_SOCK` and needs the key's `.pub` beside it). The signatures are
the same either way.

With a key, `commit`, `merge` and `land` sign the changes they create, and
`show`/`log` print who signed each one. A landed change is assembled by the
server, so the client signs its request (base, edited paths and contents,
message, metadata). Verification checks that signature and that the server
changed no other path. A signed path may still hold merged content if someone
else edited the same file concurrently.

### Per-directory permissions

```sh
cat > tokens.json <<'JSON'
{ "docs-bot-token-0123456789": ["docs"],
  "api-team-token-0123456789": ["services/api", "libs/common"] }
JSON
SPRIG_TOKEN=admin-secret sprig serve --tokens tokens.json repo.sprig
```

A scoped token can read everything but can only `land`, and only under its
directories. Moving refs directly (`push`) is refused, because that could
carry changes to any path. `SPRIG_TOKEN` stays full access.

To limit reading too, give the token an object:

```json
{ "contractor-token-0123456789": { "write": ["services/api"], "read": ["services/api", "docs"] } }
```

Such a token sees a projection of the repository: every change rebuilt with
only its paths, and changes that touched nothing there left out. Projected
changes are real objects, so the token can `sprig clone`, `fetch`, `log`
and `land` normally; its refs, `/log`, `/changes` and object requests are
answered from the projection, so no file, tree or commit message outside
its paths reaches it. `/file` and `/ls` hide everything else too. A landing
names a projected base; the server maps it back to the real change, merges,
and answers with the projected result. The mapping is stored under
`projections/` and extended as history grows. Projected changes carry
`projected_from` (the real id) and no signature; verify signatures on the
server.

## Working with git and GitHub

sprig speaks no git protocol; git does the network part, with your own
credentials (SSH keys, ssh-agent, credential helpers).

### GitHub

```sh
sprig github set git@github.com:you/repo.git   # once per repository
sprig github key ~/.ssh/deploy_key             # optional: one key for it ('' drops it)
sprig github fetch    # GitHub's branches arrive as git/<branch>
sprig github pull     # fetch, then merge git/<branch> into the current branch
sprig github push     # export the current branch and push it
```

sprig keeps a bare mirror in `.sprig/github.git`. `fetch` never creates
local branches; branches deleted on GitHub are dropped from `git/…`, and
local branches of the same name are listed (the app asks whether to
delete them). A branch whose tip has unresolved conflicts is not pushed:
resolve and commit first.

### A .git in the same folder

When the working tree has a `.git` next to `.sprig`, sprig keeps it in step:
after every command that moves a branch or HEAD (commit, merge, checkout,
branch, undo, pull...), the same branches, HEAD and index appear in git, so
`git log`, `git status` and editors agree with sprig. Each commit is written
as its changes against its parent, so this costs tens of milliseconds. A
branch that git alone moved (a plain `git commit`) is never overwritten;
sprig says so and leaves it. `.sprig/` goes into `.git/info/exclude`.

### Any git repository

```sh
mkdir proj && cd proj && sprig init
sprig import-git ../proj.git     # all local branches, with authors, dates, merges
...work, commit...
sprig export-git ../proj.git     # then git push from there
```

Import keeps each commit's sha in `meta.git_sha` and is deterministic, so
re-importing gives the same ids; submodules are skipped with a note. If both
sides moved a branch, `import-git` keeps git's version as `git/<branch>` to
merge. New changes export as commits with `Sprig-Intent`/`Sprig-Agent`/
`Sprig-Model`/`Sprig-Check` trailers and a `Sprig-Change` id, which
`import-git` maps back instead of duplicating. Export is deterministic,
branch updates must fast-forward, and tips with unresolved conflicts are
skipped (`--force` overrides both).

## Server and remotes

```sh
sprig init --bare repo.sprig
SPRIG_TOKEN=secret sprig serve --addr 127.0.0.1:7700 repo.sprig

# in a working copy
export SPRIG_TOKEN=secret
sprig remote add origin http://127.0.0.1:7700
sprig push            # fast-forward only unless -f
sprig pull            # fetch + merge (fast-forwards when possible)
```

The server won't listen on a non-loopback address without a token (or
`--tokens`). Remotes must be sprig servers; for GitHub use
[`sprig github`](#github).

API (all paths under `/v1`, bearer token when configured):

| Method | Path | Returns |
|---|---|---|
| GET | `/refs?wait=<etag>` | all refs and their etag; with `wait`, blocks until they differ from it (up to 25 s) |
| GET | `/log?ref=main&limit=50` | changes with metadata |
| GET | `/changes/{id}` | one change |
| GET | `/file?ref=main&path=src/x.go` | file contents |
| GET | `/ls?ref=main&path=src` | directory listing |
| GET/PUT | `/object/{id}` | one raw object (transport) |
| POST | `/objects/get` | many objects `{ids}` as a record stream (transport) |
| POST | `/objects/put` | upload a record stream, dependencies first (transport) |
| POST | `/objects/missing` | which of the given ids the server lacks |
| POST | `/refs/{name}` | compare-and-swap ref update `{old,new,force}` (full-access token only) |
| POST | `/land` | apply edits onto a branch tip server-side (see Landing) |

A record is `<id> <type> <len>\n` followed by `<len>` bytes; every object is
hash-checked on arrival.

### Storage and gc

New objects are written loose, one zlib file each. `sprig gc` moves them into
a pack: each file and directory version is stored as a delta against its
previous version (line-based or byte-level, whichever is smaller), trees in a
compact binary form, everything zstd-compressed, and the result is read back
and hash-checked before any loose copy is deleted. Once more than 8 packs
exist, or with `--repack`, gc merges them into one. Commands that add objects
(commit, merge, land, fetch, server uploads) start `sprig gc --auto` in the
background when about 6,400 loose objects have piled up; `gc.lock` keeps runs
from overlapping. Set `SPRIG_NO_AUTOGC=1` to turn that off.

### The index, and faster status with `sprig watch`

`.sprig/index` is one sorted, mmap'd file (like git's index) that records,
for every path, HEAD's entry and the working file's mode, content id, size
and mtime as last seen. `status`, `diff`, `commit` and `checkout` `lstat`
every file (in parallel), read only the ones whose stat changed, and compare
the two halves of each record, so nothing is flattened or rebuilt: `diff`
reads only the changed files (and stores nothing), `commit` stores only them
and rewrites only the directories along their paths, and `checkout` writes
only the paths that differ between the two trees. `sprig watch` runs in the background,
watches the working tree with inotify/kqueue, and tells them which paths
changed since the last check, so nothing else is touched. Without a running
watcher, or when it can't vouch for the whole period (just started, event
overflow, `.sprigignore` changed), sprig falls back to the full walk. Set
`SPRIG_NO_WATCH=1` to ignore a running watcher.

## Environment

| Variable | Used for |
|---|---|
| `SPRIG_AUTHOR` | `Name <email>` on your changes; wins over the repository's author and `~/.config/sprig/author` (default: your login `@localhost`) |
| `SPRIG_AGENT`, `SPRIG_MODEL` | default `--agent` / `--model` for commit and land |
| `SPRIG_TOKEN` | bearer token for `serve` and for clients; a cache uses it to reach upstream |
| `SPRIG_KEY` | signing key file (default `~/.config/sprig/signing.key`) |
| `SPRIG_NO_WATCH` | ignore a running `sprig watch` and walk the whole tree |
| `SPRIG_NO_AUTOGC` | never start a background `gc --auto` |

## Layout

```
src/store.rs      objects, refs, HEAD, operation log, config, revision parsing
src/tree.rs       trees, flat form, tree diffs, rename detection, writing files
src/index.rs      the index and the working-tree scan (status in time proportional to change)
src/diff.rs       LCS line diff, unified diff, three-way merge
src/merge.rs      history walking, merge base, tree merge, commit/merge/checkout/undo
src/pack.rs       packfiles (zstd, compact trees, deltas), object cache, gc, auto gc
src/codemerge.rs  code-aware merge with tree-sitter (Go, Python, JS/TS, Rust, Java)
src/server.rs     HTTP API, landing, scoped tokens, cache tier, standbys, reflog
src/remote.rs     client: batched object transfer, push, fetch, land
src/project.rs    projected history for read-scoped tokens
src/git.rs        git import and export, GitHub sync, keeping a working tree's .git in step
src/sign.rs       ed25519 change signatures, SSH keys, the default author
src/watch.rs      `sprig watch` file watcher (notify)
src/util.rs       errors, hex, varints, Go-compatible JSON, globs, time
src/cli.rs        the command line
src/complete.rs   shell completion (bash, zsh, fish)
src/bin/sprig-ui.rs  the desktop app's commands (`--features ui`)
ui/               the desktop app's page (web/), Tauri config, icon
tests/            integration tests (CLI as a subprocess, servers in-process)
bench/            git vs sprig benchmark (run.sh), charts (chart.py), results
```

## Measured

`bench/run.sh` builds the same history in git and sprig (100 files per
directory, 100 commits of 3-file edits, imported into sprig and packed with
`gc`), runs each command 20 times with a clean `HOME`, and writes
`bench/results.csv`; `bench/chart.py` (Python standard library only) draws
the charts. Run on a 4-core Linux VM with costly system calls (git itself is
about 2x slower here than on the 12-core machine of earlier numbers), warm
disk cache, average wall time. The previous Go build was measured in the
same runs (`bench/results-go.csv`).

![git vs sprig at 3,000 files](bench/at-3000.svg)

![each command against repo size, 1k to 30k files](bench/scaling.svg)

| Command | 1k files: git / sprig (Go before) | 3k | 10k | 30k |
|---|---|---|---|---|
| starting the program (`version`) | 2.3 / **2.2** (5.5) | 2.2 / 2.6 (5.5) | 2.9 / **2.6** (5.3) | 2.2 / **2.1** (6.2) |
| `status` (clean) | 4.9 / **4.4** (11.0) | 9.7 / **6.2** (13.1) | 17.1 / **12.0** (28.2) | 33.2 / **22.3** (82.8) |
| `status`, `sprig watch` running | 5.4 / **3.1** (9.0) | 8.2 / **3.5** (9.7) | 16.3 / **3.8** (8.0) | 40.1 / **3.9** (9.6) |
| `status` (1 modified) | 5.9 / 6.6 (13.6) | 7.5 / 7.6 (16.8) | 17.2 / **15.7** (38.1) | 38.2 / **28.2** (93.0) |
| `diff` (working tree) | 4.2 / 7.0 (13.2) | 7.1 / 7.6 (18.3) | 9.8 / 13.2 (36.4) | 20.7 / 23.3 (130.6) |
| `log -n 100` | 5.3 / **3.7** (7.8) | 5.2 / **4.1** (7.8) | 6.4 / **5.4** (8.0) | 5.7 / **5.5** (9.9) |
| `show HEAD~10` | 3.2 / 3.3 (7.7) | 3.9 / 4.5 (8.7) | 4.1 / **3.7** (9.9) | 3.8 / 4.4 (13.1) |
| `diff HEAD~50 HEAD` | 14.7 / **6.3** (15.2) | 18.4 / **7.5** (13.5) | 13.6 / **8.2** (13.4) | 11.7 / **9.8** (17.9) |
| `checkout` (50 commits back) | 67.1 / **34.2** (51.0) | 29.5 / 35.5 (60.7) | 78.9 / **46.7** (132.9) | 96.9 / **67.4** (369.9) |
| `commit` (one file) | 14.1 / **11.2** (20.6) | 16.2 / **10.8** (31.7) | 26.7 / **18.0** (63.0) | 51.8 / **38.5** (180.5) |
| storage after gc (KB) | 472 / **384** (424) | 848 / **840** (948) | 2,168 / 2,436 (2,784) | 5,924 / 7,084 (8,152) |

(ms unless noted; bold where sprig beats git. This VM is noisy: single
numbers can be off by 20% or more, e.g. git's `checkout` at 1k.)

What this shows:

- **The Rust build is 2-10x faster than the Go build** on everything that
  touches the working tree, and starts in the same time as git (~2 ms;
  Go's runtime and cgo cost ~3 ms here).
- **`status`, `commit` and `checkout` now beat git** at 10k-30k files: the
  index answers "what changed?" from one mmap'd file, and each command only
  does work for the paths that changed (see `PLAN-speed.md`).
- **With `sprig watch`, `status` stays at ~3-4 ms at any size** (10x faster
  than git at 30k files), because only the files the watcher saw change are
  looked at.
- **Working-tree `diff` and `show` are about even with git**; the remaining
  gap is reading and hashing the changed files and starting the walk.
- **Storage is smaller than git's up to a few thousand files and ~1.2x at
  30k** (it was ~1.4x with the Go build).

## Dependencies

Direct dependencies, each the standard crate for its job:

| Crate | For |
|---|---|
| `sha2`, `ed25519-dalek` (RustCrypto, dalek-cryptography) | object ids, change signatures |
| `flate2`, `zstd` | loose objects (zlib), packs (zstd) |
| `serde`, `serde_json`, `base64` | JSON objects and API |
| `tree-sitter` + Go, Python, JavaScript, TypeScript, Rust, Java grammars | code-aware merge |
| `tiny_http`, `ureq` (rustls) | server, client |
| `notify` | `sprig watch` |
| `memmap2`, `libc`, `getrandom` | mmap of packs and the index, local time, randomness |

The desktop app adds `tauri` (and `tauri-build` at build time), only when
built with `--features ui`; the CLI build does not include it.

All are pinned in `Cargo.lock` (82 crates in the CLI build, about 200 with
the app on Linux). Checked before release:

- `cargo audit` against the RustSec advisory database: no vulnerabilities,
  no unmaintained or yanked crates.
- Every crate with a build script was looked at: they compile the C of
  tree-sitter grammars, zstd and ring, generate bindings, or check the
  compiler version; none fetches anything.
- No crate name is a look-alike of a popular one; all come from the
  projects' own maintainers (rust-lang, RustCrypto, tree-sitter, dtolnay,
  algesten, notify-rs, ...).

Re-check after changing dependencies with `cargo install cargo-audit` and
`cargo audit`.

## Not done (yet)

- **Code-aware merge covers six languages.** Others (C, C++, C#, Ruby...)
  get the line merge; adding one is a grammar crate plus a table entry in
  `src/codemerge.rs`. Merged files keep each side's spacing (Go output is no
  longer run through gofmt).
- **One authority holds the refs.** Caches on any machine do the landing
  work and standbys mirror everything, but every ref swap goes to a single
  main server (Scion keeps refs in Spanner). Failover to a standby is manual.
- **Projections are built on first use.** The first request from a
  read-scoped token walks the whole history; later ones only project new
  changes. Caches pass such tokens' requests to the main server, and a read
  scope can't be widened for an existing clone (clone again).
- **Loose objects use a file each** until gc runs. Auto gc starts at about
  6,400 of them (estimated, as git does), so a busy server holds up to that
  many loose files between runs.
- **Rename detection has limits.** Beyond 2,000 added x 2,000 deleted files
  only exact renames are found; a file renamed differently on both sides of a
  merge is left as two files rather than reported as a conflict.
- **No pull requests.** `sprig github push` pushes a branch; open the pull
  request on GitHub or with `gh pr create`.
- **Only ed25519 keys sign.** RSA and ECDSA SSH keys are not supported, and
  signing through ssh-agent works on Linux and macOS, not Windows.
