//! `sprig mcp`: a Model Context Protocol server on stdin/stdout, so an agent
//! (Claude Code, an editor, sprig-ui's neighbours) drives sprig through
//! tools instead of a shell. Each tool becomes the matching command line and
//! runs through `cli::run`, the code the CLI and the app run, so everything
//! lands in the operation log and in a working tree's .git. Changes made
//! through it are credited to the client (`--agent`) unless SPRIG_AGENT or
//! the call says otherwise.
//!
//!     sprig mcp [--repo dir]
//!
//! Messages are JSON-RPC 2.0, one per line.

use crate::util::{Error, Result};
use crate::{bail, err};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

const PROTOCOL: &str = "2025-06-18";

/// One tool: its name, what it does, its input schema, and how its
/// arguments become a sprig command line.
struct Tool {
    name: &'static str,
    about: &'static str,
    schema: fn() -> Value,
    args: fn(&Value, &str) -> Result<Vec<String>>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(String::from)
}

fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn list(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map_or(vec![], |a| a.iter().filter_map(Value::as_str).map(String::from).collect())
}

fn need(v: &Value, k: &str) -> Result<String> {
    s(v, k).ok_or_else(|| err!("missing required argument {k:?}"))
}

/// --intent, --agent, --model and --check k=v for commit and land.
fn provenance(v: &Value, agent: &str, out: &mut Vec<String>) {
    for (k, flag) in [("intent", "--intent"), ("model", "--model")] {
        if let Some(x) = s(v, k) {
            out.extend([flag.into(), x]);
        }
    }
    let who = s(v, "agent").or_else(|| std::env::var("SPRIG_AGENT").ok().filter(|a| !a.is_empty()));
    out.extend(["--agent".into(), who.unwrap_or_else(|| agent.to_string())]);
    if let Some(m) = v.get("checks").and_then(Value::as_object) {
        for (k, x) in m {
            out.extend(["--check".into(), format!("{k}={}", x.as_str().unwrap_or(&x.to_string()))]);
        }
    }
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

fn checks_schema() -> Value {
    json!({ "type": "object", "description": "checks that ran, e.g. {\"tests\": \"pass\"}", "additionalProperties": { "type": "string" } })
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "status",
        about: "The current branch, uncommitted changes and conflicts HEAD recorded.",
        schema: || obj(json!({}), &[]),
        args: |_, _| Ok(vec!["status".into()]),
    },
    Tool {
        name: "log",
        about: "Recent changes with their provenance (agent, model, intent, checks).",
        schema: || {
            obj(
                json!({ "n": { "type": "integer", "description": "how many (default 20)" },
                         "agent": { "type": "string", "description": "only this agent's changes" },
                         "rev": { "type": "string", "description": "start here instead of HEAD" } }),
                &[],
            )
        },
        args: |v, _| {
            let n = v.get("n").and_then(Value::as_u64).unwrap_or(20);
            let mut a = vec!["log".into(), "-n".into(), n.to_string()];
            if let Some(x) = s(v, "agent") {
                a.extend(["--agent".into(), x]);
            }
            a.extend(s(v, "rev"));
            Ok(a)
        },
    },
    Tool {
        name: "diff",
        about: "A unified diff: the working tree against HEAD, or between two revisions.",
        schema: || obj(json!({ "from": { "type": "string" }, "to": { "type": "string" } }), &[]),
        args: |v, _| {
            Ok([vec!["diff".into()], s(v, "from").into_iter().collect(), s(v, "to").into_iter().collect()].concat())
        },
    },
    Tool {
        name: "show",
        about: "One change (default HEAD): message, provenance, signature and diff.",
        schema: || obj(json!({ "rev": { "type": "string" } }), &[]),
        args: |v, _| Ok([vec!["show".into()], s(v, "rev").into_iter().collect()].concat()),
    },
    Tool {
        name: "commit",
        about: "Snapshot the working tree (or only `paths`) as a new change on the current branch, recording why and what checks ran. There is no staging area.",
        schema: || {
            obj(
                json!({ "message": { "type": "string" },
                         "intent": { "type": "string", "description": "what was asked, or why" },
                         "paths": { "type": "array", "items": { "type": "string" }, "description": "commit only these" },
                         "checks": checks_schema(),
                         "agent": { "type": "string", "description": "who made it (default: this MCP client)" },
                         "model": { "type": "string" } }),
                &["message"],
            )
        },
        args: |v, agent| {
            let mut a = vec!["commit".into(), "-m".into(), need(v, "message")?];
            provenance(v, agent, &mut a);
            a.extend(list(v, "paths"));
            Ok(a)
        },
    },
    Tool {
        name: "branch",
        about: "List branches, create one (at `start`, default HEAD), or delete one.",
        schema: || {
            obj(
                json!({ "name": { "type": "string" }, "start": { "type": "string" }, "delete": { "type": "boolean" } }),
                &[],
            )
        },
        args: |v, _| {
            let mut a = vec!["branch".into()];
            if b(v, "delete") {
                a.push("-d".into());
            }
            a.extend(s(v, "name"));
            a.extend(s(v, "start"));
            Ok(a)
        },
    },
    Tool {
        name: "checkout",
        about: "Switch to a branch or revision; `create` makes the branch first. Uncommitted edits come along.",
        schema: || obj(json!({ "target": { "type": "string" }, "create": { "type": "boolean" } }), &["target"]),
        args: |v, _| {
            let mut a = vec!["checkout".into()];
            if b(v, "create") {
                a.push("-b".into());
            }
            a.push(need(v, "target")?);
            Ok(a)
        },
    },
    Tool {
        name: "merge",
        about: "Merge a revision into the current branch. Conflicts do not stop it: they are recorded in the merge change (see `conflicts`).",
        schema: || obj(json!({ "rev": { "type": "string" }, "message": { "type": "string" } }), &["rev"]),
        args: |v, _| {
            let mut a = vec!["merge".into()];
            if let Some(m) = s(v, "message") {
                a.extend(["-m".into(), m]);
            }
            a.push(need(v, "rev")?);
            Ok(a)
        },
    },
    Tool {
        name: "conflicts",
        about: "Files with conflicts recorded in HEAD, with the blob of each side.",
        schema: || obj(json!({}), &[]),
        args: |_, _| Ok(vec!["conflicts".into()]),
    },
    Tool {
        name: "resolve",
        about: "Resolve conflicted files by taking one side whole. To merge by hand, edit the file instead; then commit.",
        schema: || {
            obj(
                json!({ "side": { "type": "string", "enum": ["ours", "theirs", "base"] },
                         "paths": { "type": "array", "items": { "type": "string" } } }),
                &["side", "paths"],
            )
        },
        args: |v, _| {
            let side = need(v, "side")?;
            if !["ours", "theirs", "base"].contains(&side.as_str()) {
                bail!("side must be ours, theirs or base");
            }
            let paths = list(v, "paths");
            if paths.is_empty() {
                bail!("give at least one path");
            }
            Ok([vec!["resolve".into(), format!("--{side}")], paths].concat())
        },
    },
    Tool {
        name: "undo",
        about: "Undo the last operation that moved a branch or HEAD (commit, merge, checkout...). Undoing an undo redoes it.",
        schema: || obj(json!({}), &[]),
        args: |_, _| Ok(vec!["undo".into()]),
    },
    Tool {
        name: "ops",
        about: "The operation log: every command that moved a branch or HEAD, newest last.",
        schema: || obj(json!({}), &[]),
        args: |_, _| Ok(vec!["ops".into()]),
    },
    Tool {
        name: "audit",
        about: "Agent-made changes lacking a passing check (default: review) or, with `signed`, a trusted signature.",
        schema: || obj(json!({ "require": { "type": "string" }, "signed": { "type": "boolean" } }), &[]),
        args: |v, _| {
            let mut a = vec!["audit".into()];
            if let Some(r) = s(v, "require") {
                a.extend(["--require".into(), r]);
            }
            if b(v, "signed") {
                a.push("--signed".into());
            }
            Ok(a)
        },
    },
    Tool {
        name: "land",
        about: "Send the edited files to a sprig server, which merges them onto the branch tip (retrying if someone landed first). Needs a sprig remote, not GitHub.",
        schema: || {
            obj(
                json!({ "message": { "type": "string" }, "intent": { "type": "string" }, "checks": checks_schema(),
                         "agent": { "type": "string" }, "model": { "type": "string" },
                         "remote": { "type": "string" }, "branch": { "type": "string" } }),
                &["message"],
            )
        },
        args: |v, agent| {
            let mut a = vec!["land".into(), "-m".into(), need(v, "message")?];
            provenance(v, agent, &mut a);
            a.extend(s(v, "remote"));
            a.extend(s(v, "branch"));
            Ok(a)
        },
    },
    Tool {
        name: "github",
        about: "Sync with the repository's GitHub URL: fetch (branches as git/<b>), pull (fetch + merge) or push (the current branch).",
        schema: || obj(json!({ "action": { "type": "string", "enum": ["fetch", "pull", "push"] } }), &["action"]),
        args: |v, _| {
            let action = need(v, "action")?;
            if !["fetch", "pull", "push"].contains(&action.as_str()) {
                bail!("action must be fetch, pull or push");
            }
            Ok(vec!["github".into(), action])
        },
    },
];

/// Runs one tool; the text is what the command printed, or why it failed.
fn call(name: &str, args: &Value, agent: &str) -> (String, bool) {
    let Some(t) = TOOLS.iter().find(|t| t.name == name) else {
        return (format!("unknown tool {name:?}"), true);
    };
    let line = match (t.args)(args, agent) {
        Ok(l) => l,
        Err(e) => return (e.to_string(), true),
    };
    let mut out = vec![];
    let res = crate::cli::run(&line[0], &line[1..], &mut out);
    let text = String::from_utf8_lossy(&out).trim().to_string();
    match res {
        Ok(()) => (if text.is_empty() { "done".into() } else { text }, false),
        Err(Error::Exit(3, _)) => (format!("{text}\n(conflicts were recorded; see the conflicts tool)"), false),
        Err(Error::Exit(_, m)) if m.is_empty() => (text, true),
        Err(e) => ([text, e.to_string()].join("\n").trim().to_string(), true),
    }
}

fn reply(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn fail(id: &Value, code: i64, msg: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
}

/// Answers one message; None for notifications.
fn handle(msg: &Value, agent: &mut String) -> Option<Value> {
    let id = msg.get("id")?.clone(); // no id: a notification, nothing to answer
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    Some(match msg.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => {
            if let Some(n) = params.pointer("/clientInfo/name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                *agent = n.to_string();
            }
            let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL);
            reply(
                &id,
                json!({ "protocolVersion": version, "capabilities": { "tools": {} },
                        "serverInfo": { "name": "sprig", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": "Version control with sprig. Commit with `commit`, giving an intent and the checks you ran; there is no staging area. Merges record conflicts instead of stopping; fix them, then commit. `undo` reverts any branch move." }),
            )
        }
        "ping" => reply(&id, json!({})),
        "tools/list" => reply(
            &id,
            json!({ "tools": TOOLS.iter().map(|t| json!({ "name": t.name, "description": t.about, "inputSchema": (t.schema)() })).collect::<Vec<_>>() }),
        ),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let (text, is_error) = call(name, params.get("arguments").unwrap_or(&json!({})), agent);
            reply(&id, json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
        }
        m => fail(&id, -32601, &format!("method not found: {m}")),
    })
}

pub fn serve(args: &[String]) -> Result<()> {
    match args {
        [] => {}
        [f, dir] if f == "--repo" => std::env::set_current_dir(dir).map_err(|e| err!("{dir}: {e}"))?,
        _ => bail!("usage: mcp [--repo dir]"),
    }
    let mut agent = "mcp".to_string();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let answer = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, &mut agent),
            Err(e) => Some(fail(&Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(a) = answer {
            writeln!(stdout, "{a}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
