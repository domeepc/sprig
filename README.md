# sprig

A small version control system built for a world where most commits come
from agents. It is a hybrid: the storage is content-addressed and
distributed like git, and a server exposes the repository as an API so
always-online tools can ask questions without cloning.

Prototype, about 3,500 lines of Go plus tests, no dependencies outside the standard library.

```
go build -o sprig .
go test ./...
```

## What's different from git

| Idea | How sprig does it |
|---|---|
| **Provenance is structured** | A *change* (sprig's commit) carries `intent`, `agent`, `model`, and `checks` (e.g. `tests=pass`, `review=approved`) as fields, not as text in the message. `sprig log --agent X` and `sprig audit` query them. |
| **Conflicts are data** | A conflicted merge still *completes*. The conflicts are recorded inside the merge change, the files get markers, and you keep working. `sprig conflicts` lists them; they carry forward until the file is fixed. |
| **Everything is undoable** | Every command that moves a branch or HEAD is written to an operation log with the before/after state. `sprig undo` reverts the last one, including merges, and undoing an undo redoes it. |
| **API first** | `sprig serve` exposes refs, history, files and directory listings as JSON/HTTP. A CI job can read `main:src/x.go` with one GET. Every CLI command that reads has `--json`. |
| **Server-side policy** | The server refuses ref updates that are non-fast-forward, point at an unknown change, or whose tip has unresolved conflicts. Objects are hash-verified and only accepted once everything they reference is present, so a ref can never point at partial history. |
| **No index** | No staging area. `commit` snapshots the working tree (minus `.sprigignore`). |
| **SHA-256 everywhere** | Objects are `blob`, `tree`, `change`, addressed by SHA-256 and verified on every read. |

## Quick tour

```sh
sprig init
echo hello > a.txt
SPRIG_AGENT=claude sprig commit -m "add greeting" \
    --intent "user asked for a greeting" --model sonnet --check tests=pass

sprig checkout -b feature
...edit, commit...
sprig checkout main
sprig merge feature          # exit code 3 if conflicts were recorded
sprig conflicts
sprig resolve --theirs a.txt
sprig commit -m "resolve"
sprig undo                   # back to the conflicted state
sprig undo                   # and forward again

sprig audit --require review # agent-authored changes without a passing review
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

The ref swap uses a file lock, so several `sprig serve` processes can share
one repository directory (on one machine) and land concurrently.

A cache server keeps its own copy of objects, which never change once written,
and passes everything else (refs, landing, log) through to the main server:

```sh
sprig init --bare cache.sprig
sprig serve --addr 127.0.0.1:7701 --upstream http://main-host:7700 cache.sprig
```

Point clients at the cache. It checks tokens the same way the main server
does (`--token`, `--tokens`) and fetches from upstream with `SPRIG_TOKEN`.

### Partial clones

```sh
sprig clone --path services/api http://127.0.0.1:7700 work
```

This downloads history and directory trees but no file contents. Only files
under `--path` are written to disk; leave `--path` out to check out
everything. Any file content sprig needs later (diff, merge, show) is
downloaded on first use and kept. Files outside the view count as unchanged,
so `commit`, `land` and `push` work as usual.

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

The server won't listen on a non-loopback address without a token.

API (all paths under `/v1`, bearer token when configured):

| Method | Path | Returns |
|---|---|---|
| GET | `/refs` | all refs |
| GET | `/log?ref=main&limit=50` | changes with metadata |
| GET | `/changes/{id}` | one change |
| GET | `/file?ref=main&path=src/x.go` | file contents |
| GET | `/ls?ref=main&path=src` | directory listing |
| GET/PUT | `/object/{id}` | raw object (transport) |
| POST | `/objects/missing` | which of the given ids the server lacks |
| POST | `/refs/{name}` | compare-and-swap ref update `{old,new,force}` |
| POST | `/land` | apply edits onto a branch tip server-side (see below) |

## Layout

```
store.go      objects, refs, HEAD, operation log, config, revision parsing
tree.go       trees, flattening, working-tree snapshot/materialize, ignore
diff.go       LCS line diff, unified diff, three-way merge
merge.go      history walking, merge base, tree merge, commit/merge/checkout/undo
server.go     HTTP API, landing, scoped tokens, cache tier, push/fetch client
gitimport.go  git -> sprig converter (via `git cat-file --batch`)
gitexport.go  sprig -> git converter (via `git fast-import`)
main.go       CLI
```

## Measured

A synthetic git repo with 3,001 commits and 300 files imports in about 8s;
afterwards `status` takes 8 ms and `log` 4 ms.

## Not done (yet)

- **No delta compression or packfiles.** Objects are stored individually and
  zlib-compressed, so storage is about 2x git's on the same history.
- **Line-based merge only.** The plan was AST-aware merge via tree-sitter;
  this needs a cgo dependency, so it was left out of the prototype.
- **Criss-cross merges** pick one merge base rather than recursing.
- **Fetch is one HTTP request per object.** Fine locally, slow over a real
  network; batching is the obvious next step.
- **No signatures yet**, so the `agent`/`model` fields are only as trustworthy
  as whoever wrote the change. Signed changes are what would make `audit`
  actually mean something.
- No rename detection, no partial checkout.
- **Export re-streams every sprig-made change each run.** The output is
  identical so nothing new lands in git, but it costs time on long histories.
- **Servers must share a disk.** The ref lock is a Unix file lock, so
  multiple servers need the same local filesystem. Machines on separate
  disks would need a shared lock or database (Scion uses Spanner).
- **Permissions restrict writes only.** Any valid token can read the whole
  repository.
- **Partial clones can't widen.** To change `--path`, clone again. A merge
  conflict in a file outside the view is recorded but not visible on disk.
- **Diverged branches don't sync.** If GitHub and sprig both moved a branch,
  `import-git` skips it; there's no fetch-into-a-side-branch to merge from yet.
