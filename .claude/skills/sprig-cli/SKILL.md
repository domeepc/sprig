---
name: sprig-cli
description: Use when working in a repository that has a .sprig folder, or when asked to commit, branch, merge, undo, push or pull with sprig. Covers the sprig CLI (and the same commands as MCP tools via `sprig mcp`): committing with intent and checks, partial commits, recorded conflicts, undo, GitHub sync and landing.
---

# Using sprig

sprig is the version control here. A `.sprig` folder marks a sprig
repository; if there is also a `.git`, sprig keeps it in step after every
command, so **make commits with sprig, never with `git commit`** (git-only
commits are left alone and warned about, not picked up).

The same commands exist as MCP tools (`status`, `log`, `diff`, `show`,
`commit`, `branch`, `checkout`, `merge`, `conflicts`, `resolve`, `undo`,
`ops`, `audit`, `land`, `github`) when the `sprig` MCP server is connected
(`sprig mcp`); prefer them over the shell when available.

## Committing

There is no staging area: `commit` snapshots the working tree.

```sh
sprig status
sprig diff                                   # hunks are numbered: "@@ ... @@ hunk 2"
sprig commit -m "Fix rounding in totals" \
    --agent claude --model <your model id> \
    --intent "user reported totals off by a cent" \
    --check tests=pass                       # only checks you actually ran
sprig commit -m "msg" src/a.rs docs/         # only these paths
sprig commit -m "msg" --hunk src/a.rs:2      # only hunk 2 of a file
```

- Always give `--intent` (why, in the user's terms) and `--agent`/`--model`
  for your own changes; `SPRIG_AGENT`/`SPRIG_MODEL` set defaults.
- `--check name=value` records a check that ran (`tests=pass`,
  `clippy=pass`, `build=fail`). Never record one you didn't run. `review`
  is for a human's review; `sprig audit` lists agent changes without a
  passing one.
- Keep unrelated work in separate commits with path or hunk selection.
- Changes are signed automatically when the user has a key; nothing to do.

## Branches, merges, conflicts

```sh
sprig branch                    # list;  sprig branch name [start];  sprig branch -d name
sprig checkout name             # uncommitted edits come along
sprig checkout -b name          # create and switch
sprig merge other               # exit code 3 = merged with conflicts recorded
sprig conflicts                 # what is still conflicted
sprig resolve --theirs path     # or --ours / --base; or edit the markers by hand
sprig commit -m "Resolve …"     # records the resolution
```

A conflicted merge still completes: the conflicts are stored in the merge
change and the files carry markers. Resolve every file, check that no
`<<<<<<<` markers remain, run the build/tests, then commit. Never commit
files that still have markers. Before merging, check the two branches share
history (`sprig log` both); merging unrelated histories conflicts every file.

## Undo

`sprig ops` lists every command that moved a branch or HEAD; `sprig undo`
reverts the last one (merges included), and undoing an undo redoes it. Use
it instead of "fixing" a bad commit or merge by hand.

## GitHub and servers

```sh
sprig github fetch    # GitHub's branches as git/<branch>; never creates local ones
sprig github pull     # fetch + merge git/<current branch>
sprig github push     # push the current branch (refused if its tip has conflicts)
sprig land -m "msg" --intent "…" --check tests=pass   # sprig servers only, not GitHub
```

Pushing is outward-facing: only push when the user asked. `land` needs a
sprig remote (`sprig remote add origin http://…`); for GitHub, commit and
`github push`. sprig has no pull requests: after pushing, use `gh pr create`
if asked for one.

## Reading history

```sh
sprig log -n 20                 # with agent, model, intent, checks
sprig log --agent claude
sprig show <rev>                # a change and its diff; revs: id prefix, branch, HEAD~2, git/main
sprig audit --signed --require review
```
