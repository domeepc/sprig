# sprig

A small version control system built for a world where most commits come
from agents. It is a hybrid: the storage is content-addressed and
distributed like git, and a server exposes the repository as an API so
always-online tools can ask questions without cloning.

Prototype, about 9,600 lines of Go plus 2,000 of tests. Dependencies outside
the standard library: `fsnotify` (for `sprig watch`), `klauspost/compress`
(zstd, for packs) and tree-sitter with its Python, JavaScript, TypeScript,
Rust and Java grammars (for code-aware merge). All are pinned by hash in
`go.sum`.

## Install

You need:

- **Go 1.26 or newer.** Check with `go version`. Older Go (1.21+) offers to
  download 1.26 by itself; set `GOTOOLCHAIN=local` to refuse that and
  install it yourself instead.
- **A C compiler** (gcc or clang). Tree-sitter is C, so sprig builds with
  cgo; `CGO_ENABLED=0` builds fail.
- **git**, only for `import-git`/`export-git` and for fetching the source.

| System | Command |
|---|---|
| Fedora / RHEL | `sudo dnf install golang gcc git` |
| Debian / Ubuntu | `sudo apt install golang gcc git` (if its Go is older than 1.26, get Go from go.dev/dl) |
| Arch | `sudo pacman -S go gcc git` |
| macOS | `xcode-select --install` (clang and git), then `brew install go` |
| Windows | Go from go.dev/dl, plus gcc from MSYS2 (`pacman -S mingw-w64-ucrt-x86_64-gcc`) on the PATH. Untested; `sprig watch` is Unix only. |

Build and put it on your PATH:

```sh
git clone https://github.com/domeepc/sprig.git
cd sprig
go build -o sprig .
install -m 755 sprig ~/.local/bin/    # or: sudo install -m 755 sprig /usr/local/bin/
sprig version
```

(`go install github.com/...` does not work: the module is named `sprig`, not
its GitHub path.) `~/.local/bin` must be on your PATH; if `sprig version`
isn't found, add `export PATH="$HOME/.local/bin:$PATH"` to `~/.bashrc` or
`~/.zshrc`.

Check the build with `go test ./...`; one known failure,
`TestServerRejectsBadObjects` (`a//b` path), is expected for now.

Optional setup:

- `sprig keygen`: sign your changes (key in `~/.config/sprig/`).
- `SPRIG_AUTHOR="Name <email>"`: author for your changes (see
  [Environment](#environment)).
- `sprig watch` in a repo you work in a lot keeps `status` fast.
- Running a server: see `sprig serve` under [Quick tour](#quick-tour).

To update: `git pull && go build -o sprig . && install -m 755 sprig ~/.local/bin/`.
To uninstall: delete the binary, and `~/.config/sprig` if you made a key.

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
| **Merges understand code** | When a line merge conflicts, sprig retries per import and per top-level declaration for Go (`go/parser`), Python, JavaScript/JSX, TypeScript/TSX, Rust and Java (tree-sitter), and per member inside a class, `impl` or Java type both sides changed. Two sides adding imports or functions merge cleanly; the same function changed both ways still conflicts. The result must parse without errors. |

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

### Importing from git

```sh
mkdir myproject-sprig && cd myproject-sprig
sprig init
sprig import-git ../myproject
```

All local branches are converted, with authors, timestamps, messages,
merge structure, symlinks and exec bits kept; the original commit SHA is stored
in `meta.git_sha`. Import is deterministic, so re-running it reproduces the
same ids. Submodules are skipped with a note.

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
sprig trust ed25519:AbC... alice   # keys audit --signed accepts
sprig audit --signed --require review
```

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

### Pushing to GitHub (or any git host)

sprig has no git protocol of its own; it writes into a local git clone and
git does the pushing. A bare clone works best, since nothing is checked out
there to go stale.

```sh
git clone --bare https://github.com/you/proj proj.git
mkdir proj && cd proj && sprig init
sprig import-git ../proj.git
...work, commit...
sprig export-git ../proj.git
git -C ../proj.git push origin main

# later, bring in commits others pushed
git -C ../proj.git fetch origin '+refs/heads/*:refs/heads/*'
sprig import-git ../proj.git     # fast-forwards your branches
```

If both sides moved a branch, `import-git` keeps git's version as
`git/<branch>`; run `sprig merge git/<branch>` and export again.

Imported history keeps its original git commits, so pushes fast-forward on
GitHub. New changes become commits with `Sprig-Intent`/`Sprig-Agent`/
`Sprig-Model`/`Sprig-Check` trailers and a `Sprig-Change` id, which
`import-git` maps back to the same change instead of duplicating it. Export is
deterministic, branch updates must fast-forward, and tips with unresolved
conflicts are skipped (`--force` overrides both).

### Server and remotes

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
`--tokens`). Remotes must be sprig servers; a GitHub URL is refused with a
pointer to `export-git` (see above).

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

### Faster status with `sprig watch`

`status` and `commit` normally `lstat` every file (in parallel) and only read
the ones whose size or mtime changed. `sprig watch` runs in the background,
watches the working tree with inotify/kqueue, and tells them which paths
changed since the last check, so nothing else is touched. Without a running
watcher, or when it can't vouch for the whole period (just started, event
overflow, `.sprigignore` changed), sprig falls back to the full walk. Set
`SPRIG_NO_WATCH=1` to ignore a running watcher.

## Environment

| Variable | Used for |
|---|---|
| `SPRIG_AUTHOR` | `Name <email>` on your changes (default: your login `@localhost`) |
| `SPRIG_AGENT`, `SPRIG_MODEL` | default `--agent` / `--model` for commit and land |
| `SPRIG_TOKEN` | bearer token for `serve` and for clients; a cache uses it to reach upstream |
| `SPRIG_KEY` | signing key file (default `~/.config/sprig/signing.key`) |
| `SPRIG_NO_WATCH` | ignore a running `sprig watch` and walk the whole tree |
| `SPRIG_NO_AUTOGC` | never start a background `gc --auto` |
| `SPRIG_CPUPROFILE` | write a CPU profile of the command to this file |

## Layout

```
store.go      objects, refs, HEAD, operation log, config, revision parsing
tree.go       trees, flattening, working-tree snapshot/materialize, ignore
diff.go       LCS line diff, unified diff, three-way merge
merge.go      history walking, merge base, tree merge, commit/merge/checkout/undo
server.go     HTTP API, landing, scoped tokens, cache tier, push/fetch client
gitimport.go  git -> sprig converter (via `git cat-file --batch`)
gitexport.go  sprig -> git converter (via `git fast-import`)
batch.go      batched object transfer
sign.go       ed25519 change signatures
pack.go       packfiles (zstd, compact trees, deltas), gc, auto gc
cache.go      per-process cache of decoded objects and trees
statusfast.go status that only compares what may have changed
astmerge.go   Go-aware merge (go/parser), shared unit merge
langs.go      tree-sitter merge for Python, JS/TS, Rust, Java
watch.go      `sprig watch` file watcher (fsnotify)
follow.go     ref long-poll, caches following upstream, read-only standbys
reflog.go     server ref log
lock_*.go     ref lock per platform
project.go    projected history for read-scoped tokens
main.go       CLI
bench/        git vs sprig benchmark (run.sh), charts (chart.py), results
```

## Measured

`bench/run.sh` builds the same history in git and sprig (100 files per
directory, 100 commits of 3-file edits, imported into sprig and packed with
`gc`), runs each command 20 times with a clean `HOME`, and writes
`bench/results.csv`; `bench/chart.py` (Python standard library only) draws
the charts. Linux, 12 cores, warm disk cache, average wall time.

![git vs sprig at 3,000 files](bench/at-3000.svg)

![each command against repo size, 1k to 30k files](bench/scaling.svg)

| Command | 1k files git / sprig | 3k | 10k | 30k |
|---|---|---|---|---|
| starting the program (`version`) | 1.2 / 2.5 | 1.2 / 2.4 | 1.2 / 2.3 | 1.2 / 2.4 |
| `status` (clean) | 3.4 / 4.2 | 3.9 / 5.9 | 7.4 / 12.0 | 18.1 / 34.8 |
| `status`, `sprig watch` running | 3.3 / **3.0** | 4.0 / **3.0** | 7.7 / **3.0** | 19.5 / **3.6** |
| `status` (1 modified) | 3.3 / 4.8 | 4.0 / 7.4 | 7.9 / 17.1 | 18.0 / 43.7 |
| `diff` (working tree) | 2.6 / 5.2 | 2.7 / 8.1 | 4.3 / 20.0 | 9.7 / 52.8 |
| `log -n 100` | 3.6 / 4.0 | 3.6 / 4.0 | 3.4 / 4.1 | 2.9 / 4.5 |
| `show HEAD~10` | 2.4 / 4.0 | 2.3 / 4.2 | 2.3 / 4.8 | 2.4 / 6.6 |
| `diff HEAD~50 HEAD` | 10.4 / **7.8** | 10.5 / **7.6** | 7.3 / 8.1 | 6.9 / 9.4 |
| `checkout` (50 commits back) | 11.3 / 11.8 | 12.2 / 19.3 | 15.8 / 47.4 | 27.8 / 135 |
| `commit` (one file) | 6.3 / 7.7 | 9.3 / 11.0 | 11.3 / 25.2 | 21.6 / 75.0 |
| storage after gc (KB) | 424 / **404** | 800 / 928 | 2,120 / 2,764 | 5,936 / 8,132 |

(ms unless noted; bold where sprig wins.)

What this shows:

- **Startup costs ~1.2 ms more than git** at every size: Go's runtime
  (~0.3 ms), linking tree-sitter through cgo with a dynamic libc (~0.3 ms) and
  package initialisation (~0.6 ms). On small repos that is most of the gap.
- **With `sprig watch`, `status` stays at ~3 ms at any size**, because it
  only checks the files the watcher saw change, while git stats every file
  (5x faster than git at 30k files).
- **Without the watcher, sprig falls behind as repos grow.** `status`,
  working-tree `diff`, `checkout` and `commit` all walk or rewrite more of the
  tree than git does; at 30k files they are 2-5x slower. git's index keeps
  the whole tree flat and sorted in one mmap'd file; sprig loads a stat
  cache and rebuilds tree objects per directory.
- **History commands are close to git** and grow slowly with size; diffs
  between commits skip identical directories and win at small sizes.
- **Storage is about git's at 1k files and ~1.4x at 30k.**

`SPRIG_CPUPROFILE=file` writes a CPU profile of any command.

## Not done (yet)

- **Code-aware merge covers six languages.** Others (C, C++, C#, Ruby...)
  get the line merge; adding one is a grammar import plus a table entry in
  `langs.go`. Only Go output is reformatted; elsewhere merged files keep
  each side's spacing.
- **One authority holds the refs.** Caches on any machine do the landing
  work and standbys mirror everything, but every ref swap goes to a single
  main server (Scion keeps refs in Spanner). Failover to a standby is manual.
- **Projections are built on first use.** The first request from a
  read-scoped token walks the whole history; later ones only project new
  changes. Caches pass such tokens' requests to the main server, and a read
  scope can't be widened for an existing clone (clone again).
- **Small commands start ~1 ms behind git.** That is Go's runtime, cgo and
  package initialisation; a statically linked build (needs a static libc,
  e.g. `glibc-static`) would trim part of it.
- **Large working trees are slower than git without `sprig watch`.** At 30k
  files `status` is 2x, `commit` 3.5x and `checkout` 5x git's time (see
  Measured); run `sprig watch` for status, the rest needs an index like git's.
- **Loose objects use a file each** until gc runs. Auto gc starts at about
  6,400 of them (estimated, as git does), so a busy server holds up to that
  many loose files between runs.
- **Rename detection has limits.** Beyond 2,000 added x 2,000 deleted files
  only exact renames are found; a file renamed differently on both sides of a
  merge is left as two files rather than reported as a conflict.
