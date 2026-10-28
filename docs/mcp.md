# sprig as an MCP server

`sprig mcp` serves sprig's commands as [Model Context Protocol](https://modelcontextprotocol.io)
tools, so an AI agent can commit, merge, undo and sync without a shell. It
speaks JSON-RPC 2.0 on stdin/stdout, one message per line (the MCP stdio
transport).

Pair it with the [sprig-cli skill](skill.md), which teaches the agent how to
use these tools well (intents, honest checks, finishing merges).

Each tool becomes the matching `sprig` command line and runs the same code
as the CLI and the desktop app. So everything a tool does is in `sprig ops`
(and `sprig undo` reverts it), and a `.git` next to `.sprig` follows along.

## Setup

You need `sprig` 0.2.0 with the `mcp` command on your PATH:

```sh
cargo install --locked --path .   # from a sprig checkout
sprig mcp --help                  # prints usage: mcp [--repo dir]
```

The server works on one repository: the directory it starts in, or the one
given with `--repo`.

### Claude Code

This repository already registers it in `.mcp.json`, so Claude Code offers
the `sprig` server when you open the project (approve it once). For another
repository, add the same file at its root:

```json
{
  "mcpServers": {
    "sprig": { "command": "sprig", "args": ["mcp"] }
  }
}
```

or register it from the command line:

```sh
claude mcp add sprig -- sprig mcp                          # this project only
claude mcp add --scope user sprig -- sprig mcp             # every project (uses the current directory)
```

Check it with `claude mcp list`, or `/mcp` inside a session.

### Claude Desktop, Cursor and other clients

Desktop apps don't start in your repository and often don't see your shell's
PATH, so give the full path to `sprig` and the repository:

```json
{
  "mcpServers": {
    "sprig": {
      "command": "/Users/you/.cargo/bin/sprig",
      "args": ["mcp", "--repo", "/Users/you/code/project"]
    }
  }
}
```

- Claude Desktop: `~/Library/Application Support/Claude/claude_desktop_config.json`
  on macOS, `%APPDATA%\Claude\claude_desktop_config.json` on Windows; restart
  the app after editing.
- Cursor: `.cursor/mcp.json` in the project, or `~/.cursor/mcp.json` for all
  projects.
- Anything else: command `sprig`, arguments `["mcp", "--repo", "<dir>"]`,
  stdio transport.

## Tools

| Tool | Arguments | Runs |
|---|---|---|
| `status` | none | `sprig status` |
| `log` | `n` (default 20), `agent`, `rev` | `sprig log -n N [--agent A] [rev]` |
| `diff` | `from`, `to` (both optional) | `sprig diff [from [to]]` |
| `show` | `rev` (default HEAD) | `sprig show [rev]` |
| `commit` | **`message`**, `intent`, `paths`, `checks`, `agent`, `model` | `sprig commit -m … --intent … --agent … --check k=v… [paths]` |
| `branch` | `name`, `start`, `delete` | `sprig branch [-d] [name [start]]` |
| `checkout` | **`target`**, `create` | `sprig checkout [-b] target` |
| `merge` | **`rev`**, `message` | `sprig merge [-m msg] rev` |
| `conflicts` | none | `sprig conflicts` |
| `resolve` | **`side`** (`ours`, `theirs`, `base`), **`paths`** | `sprig resolve --side paths…` |
| `undo` | none | `sprig undo` |
| `ops` | none | `sprig ops` |
| `audit` | `require` (default `review`), `signed` | `sprig audit [--require c] [--signed]` |
| `land` | **`message`**, `intent`, `checks`, `agent`, `model`, `remote`, `branch` | `sprig land …` (sprig servers only) |
| `github` | **`action`** (`fetch`, `pull`, `push`) | `sprig github action` |

Bold arguments are required. `checks` is an object of check names to
results, e.g. `{"tests": "pass", "clippy": "pass"}`; record only checks that
actually ran. `paths` is a list of repository paths.

There is no `push`/`pull` to sprig servers and no `serve`, `watch`,
`keygen` or `trust`: those either run forever or touch keys, so they stay on
the command line.

## How it behaves

- **Who made it.** `commit` and `land` pass `--agent`: the call's `agent`
  if given, else `SPRIG_AGENT`, else the client's name from `initialize`
  (`claude-code`, `cursor`…). Pass `model` (and set `SPRIG_MODEL`) to record
  the model as well. Changes are signed with the user's key as usual.
- **Results.** A tool returns what the command printed as text. A failed
  command returns `isError: true` with sprig's message.
- **Conflicts are not errors.** A `merge` (or `github pull`) that records
  conflicts succeeds and says so; use `conflicts`, then `resolve` or edit the
  files, then `commit`.
- **Undo.** Anything a tool did can be reverted with the `undo` tool or
  `sprig undo`; `ops` shows what happened.
- **Outward actions.** `github push` and `land` send changes off the
  machine; a client should ask before calling them.

## Trying it by hand

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"me","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status","arguments":{}}}' \
  | sprig mcp
```

Each answer is one JSON line. Or use the MCP inspector:
`npx @modelcontextprotocol/inspector sprig mcp`.

## Troubleshooting

- **The client says the server failed to start**: run `sprig mcp --help` in
  a terminal. "unknown command" means an older sprig is first on the PATH;
  reinstall, or put the full path in the client's config.
- **"not a sprig repository"**: the server started outside a repository; add
  `--repo /path/to/repo`.
- **Desktop app can't find `sprig`**: use the full path
  (`which sprig`), since GUI apps don't read your shell profile.
- **Changes credited to `mcp`**: the client sent no name; pass `agent` in the
  call or set `SPRIG_AGENT` in the server's environment (`"env"` in the
  config).
