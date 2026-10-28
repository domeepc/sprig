# The sprig-cli skill

`.claude/skills/sprig-cli/SKILL.md` teaches an AI agent to use sprig: commit
with sprig (never `git commit` in a sprig repository), give an intent and
only the checks that ran, make partial commits, finish a merge with
recorded conflicts, undo, and sync with GitHub. It works with or without the
[MCP server](mcp.md); with it, the agent uses the tools instead of the shell.

## Claude Code

**In this repository** nothing to do: Claude Code loads skills from
`.claude/skills/`, so `sprig-cli` is there when you open the project.

**In another repository**, copy it into that project (and commit it with
sprig, so everyone working there gets it):

```sh
mkdir -p .claude/skills/sprig-cli
cp /path/to/sprig/.claude/skills/sprig-cli/SKILL.md .claude/skills/sprig-cli/
```

**For every project on your machine**, put it in your user skills:

```sh
mkdir -p ~/.claude/skills/sprig-cli
curl -fsSL https://raw.githubusercontent.com/domeepc/sprig/main/.claude/skills/sprig-cli/SKILL.md \
  -o ~/.claude/skills/sprig-cli/SKILL.md
```

Start a new session afterwards. The skill shows up as `sprig-cli` in the
skill list; Claude uses it on its own in any folder with a `.sprig`, or when
you ask it to commit, merge, push or undo with sprig. You can also call it
by name: `/sprig-cli`.

To update it, copy or download the file again; to remove it, delete the
`sprig-cli` folder.

## Other agents

The skill is plain Markdown with a short front matter, so other tools can
read the same text:

- **Cursor**: save it as `.cursor/rules/sprig.mdc` (keep the `description`
  line; Cursor uses it to decide when the rule applies).
- **Codex, Aider and agents that read `AGENTS.md`**: paste the body (without
  the front matter) into `AGENTS.md` at the repository root.
- **Anything else**: give the agent the file as instructions.

## Keeping a folder name in mind

In a repository whose `.sprigignore` has a bare `sprig` line (this one did,
for an old build output), a folder named `sprig` is ignored, which is why
the skill lives in `sprig-cli`. Keep that name, or the skill won't be
committed.
