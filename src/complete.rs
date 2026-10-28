<<<<<<< main
//! Shell completion. `sprig completion bash|zsh|fish` prints a small script;
//! the script calls `sprig __complete <words>` on every Tab, so commands,
//! flags, branches, remotes and conflicted paths come from here and stay
//! the same in every shell. Flags are read from the usage text, so a flag
//! documented there completes. When nothing is printed the shell falls back
//! to completing file names.

use crate::cli::{Out, USAGE};
use crate::err;
use crate::store::Repo;
use crate::util::Result;

/// Every command, in the order usage lists them.
pub const COMMANDS: &[&str] = &[
    "init",
    "clone",
    "commit",
    "status",
    "log",
    "show",
    "diff",
    "branch",
    "checkout",
    "merge",
    "conflicts",
    "resolve",
    "ops",
    "undo",
    "audit",
    "keygen",
    "trust",
    "remote",
    "push",
    "fetch",
    "pull",
    "land",
    "serve",
    "import-git",
    "export-git",
    "cat",
    "gc",
    "watch",
    "github",
    "sparse",
    "completion",
    "version",
];

/// What a positional argument is.
enum Arg {
    Words(&'static [&'static str]),
    Revs,
    Remotes,
    Branches,
    Conflicts,
    Files, // left to the shell
}

fn arg(cmd: &str, pos: usize) -> Arg {
    match (cmd, pos) {
        ("completion", 0) => Arg::Words(&["bash", "zsh", "fish"]),
        ("remote", 0) => Arg::Words(&["add", "list"]),
        ("github", 0) => Arg::Words(&["set", "key", "pull", "push"]),
        ("push" | "fetch" | "pull" | "land", 0) => Arg::Remotes,
        ("push" | "fetch" | "pull" | "land", 1) => Arg::Branches,
        ("branch" | "checkout" | "merge" | "show" | "log" | "diff" | "audit", _) => Arg::Revs,
        ("resolve", _) => Arg::Conflicts,
        _ => Arg::Files,
    }
}

/// The flags usage documents for cmd, each with whether it takes a value:
/// "[-b branch]" does, "[-f]" and "(--ours|--theirs)" don't.
fn flags(cmd: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = vec![];
    for line in USAGE.lines() {
        // a line can describe several commands ("push ...    fetch ..."):
        // take each piece that starts with cmd, up to the description gap
        for piece in line.split("   ").map(str::trim) {
            if piece.split(' ').next() != Some(cmd) {
                continue;
            }
            let toks: Vec<&str> = piece.split_whitespace().collect();
            for (i, t) in toks.iter().enumerate() {
                let alts = t.trim_matches(|c| matches!(c, '[' | '(' | ']' | ')' | '.')).split('|');
                let closed = t.trim_end_matches('.').ends_with([']', ')']) || t.contains('|');
                let next_is_value = toks.get(i + 1).is_some_and(|n| !n.starts_with(['[', '<', '-', '(', '|']));
                for f in alts.filter(|f| f.starts_with('-') && f.len() > 1) {
                    if !out.iter().any(|(g, _)| g == f) {
                        out.push((f.to_string(), !closed && next_is_value));
                    }
                }
            }
        }
    }
    out
}

/// Candidates for the last of words (the one being typed, maybe empty).
pub fn complete(words: &[String], out: Out) -> Result<()> {
    let Some((cur, done)) = words.split_last() else { return Ok(()) };
    let cands: Vec<String> = match done.split_first() {
        None => COMMANDS.iter().map(|s| s.to_string()).collect(),
        Some((cmd, rest)) => {
            let fl = flags(cmd);
            if cur.starts_with('-') {
                fl.into_iter().map(|(f, _)| f).collect()
            } else {
                // the positional index of cur: skip flags and their values
                let (mut pos, mut skip) = (0, false);
                for w in rest {
                    if std::mem::take(&mut skip) {
                        continue;
                    }
                    match fl.iter().find(|(f, _)| f == w) {
                        Some((_, takes)) => skip = *takes,
                        None if w.starts_with('-') => {}
                        None => pos += 1,
                    }
                }
                if skip {
                    vec![] // a flag's value: let the shell offer files
                } else {
                    dynamic(arg(cmd, pos))
                }
            }
        }
    };
    for c in cands.iter().filter(|c| c.starts_with(cur.as_str())) {
        writeln!(out, "{c}")?;
    }
    Ok(())
}

/// Reads branches, remotes or conflicts from the repository, if there is one.
fn dynamic(a: Arg) -> Vec<String> {
    let words = |ws: &[&str]| ws.iter().map(|s| s.to_string()).collect();
    let repo = || Repo::find().ok();
    let refs = |prefix: &str| -> Vec<String> {
        repo().map_or(vec![], |r| {
            r.list_refs().into_keys().filter_map(|k| k.strip_prefix(prefix).map(String::from)).collect()
        })
    };
    match a {
        Arg::Words(ws) => words(ws),
        Arg::Files => vec![],
        Arg::Branches => refs("heads/"),
        Arg::Revs => {
            let mut v = refs("heads/");
            v.extend(refs("remotes/"));
            v.push("HEAD".into());
            v
        }
        Arg::Remotes => repo().map_or(vec![], |r| r.load_config().remotes.into_keys().collect()),
        Arg::Conflicts => repo()
            .and_then(|r| {
                let (_, head) = r.head();
                r.get_change(&head).ok()
            })
            .map_or(vec![], |c| c.conflicts.into_iter().map(|c| c.path).collect()),
    }
}

/// The script `sprig completion <shell>` prints.
pub fn script(shell: &str) -> Result<&'static str> {
    Ok(match shell {
        "bash" => BASH,
        "zsh" => ZSH,
        "fish" => FISH,
        _ => return Err(err!("completion: unknown shell {shell:?} (bash, zsh or fish)")),
    })
}

const BASH: &str = r#"# sprig completion for bash: eval "$(sprig completion bash)"
_sprig() {
    local IFS=$'\n'
    COMPREPLY=($(sprig __complete "${COMP_WORDS[@]:1:COMP_CWORD}" 2>/dev/null))
}
complete -o default -F _sprig sprig
"#;

const ZSH: &str = r#"#compdef sprig
# sprig completion for zsh: source <(sprig completion zsh)   (after compinit)
_sprig() {
    local -a c
    c=(${(f)"$(sprig __complete "${(@)words[2,CURRENT]}" 2>/dev/null)"})
    if (( ${#c} )); then compadd -a c; else _files; fi
}
compdef _sprig sprig
"#;

const FISH: &str = r#"# sprig completion for fish: sprig completion fish | source
function __sprig_complete
    set -l c (sprig __complete (commandline -opc)[2..-1] (commandline -ct) 2>/dev/null)
    if test (count $c) -gt 0
        printf '%s\n' $c
    else
        __fish_complete_path (commandline -ct)
    end
end
complete -c sprig -f -a '(__sprig_complete)'
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn run(words: &[&str]) -> Vec<String> {
        let words: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        let mut out = vec![];
        complete(&words, &mut out).unwrap();
        String::from_utf8(out).unwrap().lines().map(String::from).collect()
    }

    #[test]
    fn commands_and_flags() {
        // every command is documented in usage, so its flags can be found
        for c in COMMANDS {
            assert!(USAGE.lines().any(|l| l.split("   ").any(|p| p.trim().split(' ').next() == Some(*c))), "{c}");
        }
        assert_eq!(run(&["ch"]), ["checkout"]);
        assert_eq!(run(&["resolve", "--"]), ["--ours", "--theirs", "--base"]);
        assert!(run(&["commit", "-"]).contains(&"--intent".to_string()));
        assert_eq!(flags("init"), [("--bare".to_string(), false), ("-b".to_string(), true)]);
        assert_eq!(flags("pull"), []);
        assert_eq!(run(&["github", ""]), ["set", "key", "pull", "push"]);
        assert_eq!(run(&["completion", "z"]), ["zsh"]);
        // a flag's value and file arguments are left to the shell
        assert!(run(&["commit", "-m", ""]).is_empty());
    }
}
=======
//! Shell completion. `sprig completion bash|zsh|fish` prints a small script;
//! the script calls `sprig __complete <words>` on every Tab, so commands,
//! flags, branches, remotes and conflicted paths come from here and stay
//! the same in every shell. Flags are read from the usage text, so a flag
//! documented there completes. When nothing is printed the shell falls back
//! to completing file names.

use crate::cli::{Out, USAGE};
use crate::err;
use crate::store::Repo;
use crate::util::Result;

/// Every command, in the order usage lists them.
pub const COMMANDS: &[&str] = &[
    "init",
    "clone",
    "commit",
    "status",
    "log",
    "show",
    "diff",
    "branch",
    "checkout",
    "merge",
    "conflicts",
    "resolve",
    "ops",
    "undo",
    "audit",
    "keygen",
    "trust",
    "remote",
    "push",
    "fetch",
    "pull",
    "land",
    "serve",
    "import-git",
    "export-git",
    "cat",
    "gc",
    "watch",
    "github",
    "sparse",
    "completion",
    "version",
];

/// What a positional argument is.
enum Arg {
    Words(&'static [&'static str]),
    Revs,
    Remotes,
    Branches,
    Conflicts,
    Files, // left to the shell
}

fn arg(cmd: &str, pos: usize) -> Arg {
    match (cmd, pos) {
        ("completion", 0) => Arg::Words(&["bash", "zsh", "fish"]),
        ("remote", 0) => Arg::Words(&["add", "list"]),
        ("github", 0) => Arg::Words(&["set", "pull", "push"]),
        ("push" | "fetch" | "pull" | "land", 0) => Arg::Remotes,
        ("push" | "fetch" | "pull" | "land", 1) => Arg::Branches,
        ("branch" | "checkout" | "merge" | "show" | "log" | "diff" | "audit", _) => Arg::Revs,
        ("resolve", _) => Arg::Conflicts,
        _ => Arg::Files,
    }
}

/// The flags usage documents for cmd, each with whether it takes a value:
/// "[-b branch]" does, "[-f]" and "(--ours|--theirs)" don't.
fn flags(cmd: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = vec![];
    for line in USAGE.lines() {
        // a line can describe several commands ("push ...    fetch ..."):
        // take each piece that starts with cmd, up to the description gap
        for piece in line.split("   ").map(str::trim) {
            if piece.split(' ').next() != Some(cmd) {
                continue;
            }
            let toks: Vec<&str> = piece.split_whitespace().collect();
            for (i, t) in toks.iter().enumerate() {
                let alts = t.trim_matches(|c| matches!(c, '[' | '(' | ']' | ')' | '.')).split('|');
                let closed = t.trim_end_matches('.').ends_with([']', ')']) || t.contains('|');
                let next_is_value = toks.get(i + 1).is_some_and(|n| !n.starts_with(['[', '<', '-', '(', '|']));
                for f in alts.filter(|f| f.starts_with('-') && f.len() > 1) {
                    if !out.iter().any(|(g, _)| g == f) {
                        out.push((f.to_string(), !closed && next_is_value));
                    }
                }
            }
        }
    }
    out
}

/// Candidates for the last of words (the one being typed, maybe empty).
pub fn complete(words: &[String], out: Out) -> Result<()> {
    let Some((cur, done)) = words.split_last() else { return Ok(()) };
    let cands: Vec<String> = match done.split_first() {
        None => COMMANDS.iter().map(|s| s.to_string()).collect(),
        Some((cmd, rest)) => {
            let fl = flags(cmd);
            if cur.starts_with('-') {
                fl.into_iter().map(|(f, _)| f).collect()
            } else {
                // the positional index of cur: skip flags and their values
                let (mut pos, mut skip) = (0, false);
                for w in rest {
                    if std::mem::take(&mut skip) {
                        continue;
                    }
                    match fl.iter().find(|(f, _)| f == w) {
                        Some((_, takes)) => skip = *takes,
                        None if w.starts_with('-') => {}
                        None => pos += 1,
                    }
                }
                if skip {
                    vec![] // a flag's value: let the shell offer files
                } else {
                    dynamic(arg(cmd, pos))
                }
            }
        }
    };
    for c in cands.iter().filter(|c| c.starts_with(cur.as_str())) {
        writeln!(out, "{c}")?;
    }
    Ok(())
}

/// Reads branches, remotes or conflicts from the repository, if there is one.
fn dynamic(a: Arg) -> Vec<String> {
    let words = |ws: &[&str]| ws.iter().map(|s| s.to_string()).collect();
    let repo = || Repo::find().ok();
    let refs = |prefix: &str| -> Vec<String> {
        repo().map_or(vec![], |r| {
            r.list_refs().into_keys().filter_map(|k| k.strip_prefix(prefix).map(String::from)).collect()
        })
    };
    match a {
        Arg::Words(ws) => words(ws),
        Arg::Files => vec![],
        Arg::Branches => refs("heads/"),
        Arg::Revs => {
            let mut v = refs("heads/");
            v.extend(refs("remotes/"));
            v.push("HEAD".into());
            v
        }
        Arg::Remotes => repo().map_or(vec![], |r| r.load_config().remotes.into_keys().collect()),
        Arg::Conflicts => repo()
            .and_then(|r| {
                let (_, head) = r.head();
                r.get_change(&head).ok()
            })
            .map_or(vec![], |c| c.conflicts.into_iter().map(|c| c.path).collect()),
    }
}

/// The script `sprig completion <shell>` prints.
pub fn script(shell: &str) -> Result<&'static str> {
    Ok(match shell {
        "bash" => BASH,
        "zsh" => ZSH,
        "fish" => FISH,
        _ => return Err(err!("completion: unknown shell {shell:?} (bash, zsh or fish)")),
    })
}

const BASH: &str = r#"# sprig completion for bash: eval "$(sprig completion bash)"
_sprig() {
    local IFS=$'\n'
    COMPREPLY=($(sprig __complete "${COMP_WORDS[@]:1:COMP_CWORD}" 2>/dev/null))
}
complete -o default -F _sprig sprig
"#;

const ZSH: &str = r#"#compdef sprig
# sprig completion for zsh: source <(sprig completion zsh)   (after compinit)
_sprig() {
    local -a c
    c=(${(f)"$(sprig __complete "${(@)words[2,CURRENT]}" 2>/dev/null)"})
    if (( ${#c} )); then compadd -a c; else _files; fi
}
compdef _sprig sprig
"#;

const FISH: &str = r#"# sprig completion for fish: sprig completion fish | source
function __sprig_complete
    set -l c (sprig __complete (commandline -opc)[2..-1] (commandline -ct) 2>/dev/null)
    if test (count $c) -gt 0
        printf '%s\n' $c
    else
        __fish_complete_path (commandline -ct)
    end
end
complete -c sprig -f -a '(__sprig_complete)'
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn run(words: &[&str]) -> Vec<String> {
        let words: Vec<String> = words.iter().map(|s| s.to_string()).collect();
        let mut out = vec![];
        complete(&words, &mut out).unwrap();
        String::from_utf8(out).unwrap().lines().map(String::from).collect()
    }

    #[test]
    fn commands_and_flags() {
        // every command is documented in usage, so its flags can be found
        for c in COMMANDS {
            assert!(USAGE.lines().any(|l| l.split("   ").any(|p| p.trim().split(' ').next() == Some(*c))), "{c}");
        }
        assert_eq!(run(&["ch"]), ["checkout"]);
        assert_eq!(run(&["resolve", "--"]), ["--ours", "--theirs", "--base"]);
        assert!(run(&["commit", "-"]).contains(&"--intent".to_string()));
        assert_eq!(flags("init"), [("--bare".to_string(), false), ("-b".to_string(), true)]);
        assert_eq!(flags("pull"), []);
        assert_eq!(run(&["github", ""]), ["set", "pull", "push"]);
        assert_eq!(run(&["completion", "z"]), ["zsh"]);
        // a flag's value and file arguments are left to the shell
        assert!(run(&["commit", "-m", ""]).is_empty());
    }
}
>>>>>>> git/main
