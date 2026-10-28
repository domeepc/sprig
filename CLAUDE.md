# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

sprig is a prototype version control system for agent-authored commits: content-addressed storage like git, plus a JSON/HTTP server (`sprig serve`) so tools can read and land changes without cloning. README.md is the user-facing spec. Keep it in step when behavior changes (commands, env vars, API, the Layout section).

## Commands

```sh
cargo build --release                 # CLI only -> target/release/sprig
cargo build --release --features ui   # also the desktop app, sprig-ui (Tauri; needs Rust 1.90)
cargo test --release --features ui --bin sprig-ui   # the app's diff-parser test
cargo test --release                  # full suite (what the README tells users to run)
cargo test --test github              # one integration test file (tests/github.rs, local.rs, remote.rs, status.rs)
cargo test --test local some_name     # one test by name filter
cargo test --lib complete             # unit tests inside one module (#[cfg(test)] in src/)
cargo clippy --all-targets            # lint
cargo fmt                             # rustfmt.toml: max_width 120, use_small_heuristics "Max"
cargo build --release && bench/run.sh 1000 3000 > bench/results.csv   # git vs sprig benchmark; SPRIG=path overrides the binary
python3 bench/chart.py                # redraw bench/*.svg from results.csv
```

Rust 1.89+, edition 2024. A C compiler is needed (tree-sitter grammars, zstd). git must be installed for the import/export/github tests. Those tests return early without it.

## Architecture

- **Single library, thin binaries.** All logic is in the `sprig` lib (`src/lib.rs`). `src/main.rs` just calls `cli::main()`. `sprig-ui` is Tauri: `src/bin/sprig-ui.rs` holds a few `#[tauri::command]`s (read state, diffs, conflicts, and `run`, which calls `sprig::cli::run`), and the page is plain HTML/CSS/JS in `ui/web` (no npm, no build step; `app.js` renders with template strings, so repository text must go through `esc()`). Config, icon and capabilities are in `ui/`; `build.rs` runs tauri-build only for `--features ui`. Actions never reimplement operations: new user-facing operations go in `cli.rs` first, and the page invokes them by command name through `run`.
- **`Repo` (src/store.rs) is the hub.** Most functionality is `impl Repo` blocks spread across modules: `merge.rs` (commit/merge/checkout/undo, history walks), `git.rs` (import/export, GitHub sync), `remote.rs` (push/fetch/land client), `pack.rs` (packs, gc), `index.rs` (working-tree scan). Look for `impl Repo` in a module, not only in store.rs.
- **Objects**: `blob`, `tree`, `change` (sprig's commit, with structured `meta`: intent/agent/model/checks, signature, `git_sha`), SHA-256 ids verified on every read. Loose zlib objects until `gc` packs them (zstd, deltas). Commands that add objects trigger a detached `gc --auto` (see the end of `cli::run`). Tests can disable it with `SPRIG_NO_AUTOGC`.
- **Operation log / undo**: anything that moves refs or HEAD must go through `Repo::record(cmd, || ...)`, which snapshots state before and after so `sprig undo` works. Skipping it breaks undo.
- **Conflicts are data**: a conflicted merge still creates a change, with the conflicts recorded in it. The CLI signals this with exit code 3 (`Error::Exit(3, _)`), which callers (including the UI) treat as success.
- **Errors**: `util::Result` with the `bail!`/`err!` macros (from `crate::{bail, err}`). `Error::Exit(code, msg)` carries the process exit code.
- **Go compatibility is a hard constraint.** The Rust code replaced a Go implementation and must produce byte-identical objects, packs and wire protocol (same change ids for the same git import). JSON written into objects goes through Go-compatible helpers in `util.rs` (`go_json_str`, etc.), not plain serde_json output. Don't change serialization, field order or escaping casually.
- **git interop shells out to the `git` binary**. sprig speaks no git protocol. Import uses one long-lived `git cat-file --batch`, export writes a `git fast-import` stream. GitHub sync (`sprig github set/key/pull/push`) keeps a bare mirror at `.sprig/github.git`, and git does the network part with the user's credentials (`core.sshCommand` is set there when `github key` is configured).
- **Local .git sync**: `Repo::record` (store.rs) calls `sync_local_git` (git.rs) after every ref/HEAD move when the working tree has a `.git`: export into `refs/sprig/heads/*`, then move `refs/heads/*` only when safe (never over a git-only commit), set HEAD, `git reset -q`. Export writes each commit as a diff against its first parent. `github fetch` imports GitHub's branches as `remotes/git/*` only (never creates local heads) and prunes deleted ones.
- **Server** (`server.rs`, tiny_http): `/v1` API, server-side `land` (three-way merge onto the tip, retried on ref compare-and-swap failure under an OS file lock), scoped tokens, cache tier (`--upstream`), read-only standbys (`--follow`). Read-scoped tokens see a projected history built in `project.rs`.
- **Status speed path**: `.sprig/index` is an mmap'd sorted file. `status`/`diff`/`commit`/`checkout` only do work for paths whose stat changed. `sprig watch` (notify) can narrow this further. `PLAN-speed.md` documents the design and the benchmark targets. Don't regress `bench/run.sh` numbers.
- **Code-aware merge** (`codemerge.rs`): on line-merge conflict, retries per import/top-level declaration via tree-sitter for Go, Python, JS/TS, Rust, Java. Adding a language means adding a grammar crate and a table entry there.

## Tests

Integration tests in `tests/` run the real binary as a subprocess (`env!("CARGO_BIN_EXE_sprig")`) in a fresh temp dir per test, so they run in parallel. Servers are started in-process. Helpers are in `tests/common/mod.rs`: `Ctx::new()`, `c.must(&[...])` (run sprig, panic on failure), `c.sp(...)` (returns `Err(Failed{code,..})`), `c.write/read`, `c.repo()`, `c.temp()`, and `git(dir, args)` with fixed author/date env. GitHub sync tests use a local bare repo as the "host".
