# Plan: status, diff, commit and checkout as fast as git on large repos

## Where we are

From `bench/run.sh` (100 commits, ms, average of 20 runs):

| Command | git 3k | sprig 3k | git 30k | sprig 30k |
|---|---|---|---|---|
| `status` (clean) | 3.9 | 5.9 | 18.1 | 34.8 |
| `diff` (working tree, 1 file changed) | 2.7 | 8.1 | 9.7 | 52.8 |
| `commit` (1 file) | 9.3 | 11.0 | 21.6 | 75.0 |
| `checkout` (50 commits back, ~150 files differ) | 12.2 | 19.3 | 27.8 | 135 |

Startup (~1.2 ms behind git) is not covered here: that is Go's runtime floor
and needs a separate decision (split the networking out, or another language).

## Why: what profiling the 30k-file repo shows

All four commands share one problem. They do work for every file in the repo,
even when only one file changed, and most of it is sprig's own bookkeeping
rather than the file system.

| Cost | Who pays | Share |
|---|---|---|
| Walk: one `getdents` per directory and one `fstatat` per file. git does the same, so this is the floor (`find` doing the same: 24 ms single-threaded). | all four | 40-50% |
| Stat cache loaded into a 30k-entry map with 30k hex strings, serially, before the walk starts | all four | ~8 ms |
| A fresh 30k-entry stat cache and a full `Flat` map built on every run, even when nothing changed | all four | ~15% |
| `Snapshot(store=true)` calls `HasObject` for every cached file (30k pack index lookups) | diff, commit | 11-13% |
| `diff` stores every changed file as a blob just to diff it | diff | small, but pointless |
| `BuildTree` rebuilds and re-marshals every directory's tree object from the full flat map | commit | 16% |
| `ChangeFlat` flattens both whole commits: every tree is parsed from JSON (`parseTree`, `encoding/json`) | checkout | 38% |
| The "is the working tree clean?" check does a second full `Snapshot` | checkout | 20% |
| The flat memo (2.5 MB) and the stat cache (1.7 MB) are rewritten whole after small changes | checkout, commit | ~6% |
| Worker pool fixed at 8. On 12 cores, 4 workers was fastest and more made it slower. | all four | — |

Writing the changed files during `checkout` is only 6%, and hashing is negligible.

## The fix

The fix has two parts. One index, like git's, makes "what changed?" cheap.
Then every command does work only for the paths that changed.

### Phase 1: the index (status)

Files: `tree.go` (stat cache, `snapper`), `statusfast.go`, new `index.go`.

1. **New stat cache format `SPRIGSC2`.** It has a header, then fixed-width
   records sorted by path: path offset, size, mtime, mode and a raw 32-byte
   id. A string table follows. It is read with `mmapFile` and searched by path
   with binary search, so there is no map, nothing is decoded and no hex is
   made at load. On anything unexpected, such as a torn write or the old
   format, the cache is ignored and every file is re-read once, as today.
2. **Compare while walking.** The walker checks each file's stat against its
   record and keeps only the paths that differ, plus the count of files seen.
   If the count matches and nothing differs, the tree is clean: no `Flat` and
   no new cache are built. The racily-clean rule stays: entries newer than the
   cache's write time minus 1 s are re-read.
3. **Patch the cache instead of rebuilding it.** When something changed,
   write a new file by merging the sorted old records with the changed ones in
   one pass, which is linear and needs no map.
4. **Directory mtimes.** Also record each directory's mtime. If it hasn't
   changed, no file was added to or removed from it, so the walk stats the
   known files by name and skips `getdents`. This is what git's untracked
   cache does. If the mtime is within the racy window, the directory is read
   as usual.
5. **Worker pool sized to the machine:** `min(GOMAXPROCS, 4..8)`, with the
   exact number chosen by measuring. Work is split by directory, as today.

Target for clean `status` at 30k files: 18-22 ms, down from 34.8 (git 18.1).
With `sprig watch` it stays at ~3.6 ms.

### Phase 2: diff (working tree)

Files: `main.go` (`cmdDiff`), `statusfast.go`.

1. Get the changed paths from `statusDeltas`, the same fast path as `status`,
   instead of `Snapshot(true)`.
2. Read only those files and diff them against HEAD's blobs. Nothing goes into
   the object store, since `diff` only reads.

Target at 30k: ~20 ms, down from 52.8 (git 9.7). The rest is the walk; with
`sprig watch` it would be ~4 ms.

### Phase 3: commit

Files: `merge.go` (`Commit`), `tree.go` (`BuildTree`, an incremental update
alongside it), `statusfast.go`.

1. Get the changed paths from `statusDeltas`, store only those files as blobs,
   and drop the per-file `HasObject` check. The stat cache only records an id
   after its blob is written. To keep that true across `gc`, `gc` drops the
   stat cache whenever it removes unreachable loose objects, so a working
   file's blob can never be pruned while the cache still points at it.
2. **Incremental tree update.** Start from HEAD's tree and rewrite only the
   directories on the changed paths, from the leaf up to the root, so one
   edited file means ~3 tree objects instead of 300. Unchanged subtrees keep
   their ids. `GetTree` is already cached, so each directory is parsed once.
3. Update the stat cache and status memo for the changed paths only
   (Phase 1, step 3).

Target at 30k: ~22 ms, down from 75 (git 21.6).

### Phase 4: checkout

Files: `merge.go` (`Checkout`), `tree.go` (`Materialize`, `workingMatches`,
flat memo).

1. **Cleanliness check through `statusDeltas`** instead of a second full
   `Snapshot`. If something is dirty and `-f` isn't given, refuse, as today.
2. **Diff the two commits' trees with `DiffTrees`**, which already skips
   identical subtrees, instead of flattening both. Only the ~150 paths that
   differ are written or removed.
3. **Parse trees from the pack's compact form.** Trees are stored compact in
   packs, but `GetTree` re-encodes them to JSON and parses that JSON with
   `encoding/json`. Decode the compact form straight into a `Tree`; loose
   trees keep the JSON path.
4. **Update, don't rewrite.** Record the written files in the stat cache with
   their new stat, via the same patch as Phase 1 step 3. Drop the 2.5 MB flat
   memo: once the index holds the ids, the memo is redundant.

Target at 30k: ~30 ms, down from 135 (git 27.8): one walk plus writing the
changed files.

### Phase 5: re-measure and document

- Run `bench/run.sh`, regenerate the charts and update the README's Measured
  section and its "Not done" bullet.
- `SPRIG_CPUPROFILE` each command at 30k again. Anything above 10% that isn't
  a system call gets a look.

## Order and size

Do 1, then 2, 3 and 4. Everything after Phase 1 reuses the index and
`statusDeltas`; after that, the phases are independent.

| Phase | Effort | Risk |
|---|---|---|
| 1 index | 1-2 days | Medium: a wrong cache means wrong `status`. Mitigated by the fallback to a full re-read. |
| 2 diff | ½ day | Low |
| 3 commit | 1 day | Medium: a wrong tree means a wrong commit. Mitigated by a test against a full `BuildTree`. |
| 4 checkout | 1-1½ days | Medium: must never overwrite uncommitted work. The existing refusal tests cover it. |
| 5 measure and docs | ½ day | — |

No new dependencies. Everything uses the standard library plus the existing
`mmapFile` and `listDir`.

## Verification

- `go vet ./...` and `go test -race ./...` after each phase. Only the known
  `TestServerRejectsBadObjects` `a//b` failure is allowed.
- New tests in `sprig_test.go`, using the existing `newRepo`/`must` helpers:
  - The index gives the same `status` as a full `Snapshot` for: an edit, an
    add, a delete, a chmod, a symlink, an edit inside the racy window, an
    added file in a directory whose mtime is unchanged, and a torn cache file.
  - The incremental tree update gives the same root tree id as `BuildTree`
    of the full flat map, for random edits, adds, deletes and new or removed
    directories.
  - A checkout through `DiffTrees` leaves the same working tree and the same
    `status` as today's full-flatten checkout, and still refuses over
    uncommitted changes without `-f`.
  - After `gc` prunes loose objects, `commit` still writes every blob it
    references (`TestStoreIntegrity`-style read-back).
- Benchmark: `bench/run.sh` before and after each phase. Each phase must not
  slow anything at 1k files and must hit its 30k target within ~20%.

## What this does not fix

- **Startup**, about 1.2 ms behind git on every command. See the README.
- **The file system itself.** Without `sprig watch`, `status` still stats every
  file, as git does without fsmonitor, so at best it ties git there. To be
  clearly faster on big repos, run `sprig watch`. It already brings `status`
  to ~3.6 ms, and these phases let `diff`, `commit` and `checkout` use it too.
