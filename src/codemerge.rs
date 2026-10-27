//! Structure-aware merge for source code, using tree-sitter grammars. A line
//! merge conflicts whenever both sides touch neighbouring lines, which in
//! code is mostly "both added an import" or "both appended a function".
//! Here the file is split into top-level units keyed by what they declare,
//! each unit merges on its own, and the result keeps ours' order with
//! theirs' additions after the unit they follow. Statements that declare
//! nothing (imports, expressions) are keyed by their text, which makes
//! imports merge as a set. Comments, decorators and attributes stay with the
//! unit after them. When both sides changed the same class (or impl, Java
//! type, or Go import block), its members are merged the same way, one level
//! down. The result must parse without errors; otherwise the line merge's
//! conflict stands.

use crate::diff::merge3_text;
use std::collections::HashMap;
use tree_sitter::{Language, Node, Parser, Tree};

struct Lang {
    lang: Language,
    /// node kinds that belong to the unit after them
    attached: &'static [&'static str],
    /// "" means: key by the text
    key: fn(Node, &str) -> String,
    /// a container's members node, or None
    body: for<'t> fn(Node<'t>) -> Option<Node<'t>>,
    /// default separator between top-level units
    gap: &'static str,
}

fn lang_for(path: &str) -> Option<Lang> {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    let comments: &[&str] = &["comment"];
    let l = |lang: Language, attached, key, body, gap| Some(Lang { lang, attached, key, body, gap });
    match ext.as_str() {
        "go" => l(tree_sitter_go::LANGUAGE.into(), comments, go_key, go_body, "\n\n"),
        "py" => l(tree_sitter_python::LANGUAGE.into(), comments, py_key, py_body, "\n\n\n"),
        "js" | "mjs" | "cjs" | "jsx" => l(tree_sitter_javascript::LANGUAGE.into(), comments, js_key, js_body, "\n\n"),
        "ts" | "mts" | "cts" => {
            l(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), comments, js_key, js_body, "\n\n")
        }
        "tsx" => l(tree_sitter_typescript::LANGUAGE_TSX.into(), comments, js_key, js_body, "\n\n"),
        "rs" => l(
            tree_sitter_rust::LANGUAGE.into(),
            &["line_comment", "block_comment", "attribute_item", "inner_attribute_item"],
            rs_key,
            rs_body,
            "\n\n",
        ),
        "java" => l(tree_sitter_java::LANGUAGE.into(), &["line_comment", "block_comment"], java_key, java_body, "\n\n"),
        _ => None,
    }
}

fn text<'a>(n: Option<Node>, src: &'a str) -> &'a str {
    n.map_or("", |n| &src[n.start_byte()..n.end_byte()])
}

fn named<'a>(n: Node, field: &str, src: &'a str) -> &'a str {
    text(n.child_by_field_name(field), src)
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn named_children(n: Node) -> Vec<Node> {
    (0..n.named_child_count() as u32).filter_map(|i| n.named_child(i)).collect()
}

fn go_key(n: Node, src: &str) -> String {
    match n.kind() {
        "package_clause" => "package".into(),
        "import_declaration" => "import".into(),
        "function_declaration" => match named(n, "name", src) {
            "init" | "_" => String::new(), // no stable identity
            name => format!("func {name}"),
        },
        "method_declaration" => {
            let recv = n.child_by_field_name("receiver").and_then(|r| r.named_child(0));
            let typ = recv.map_or("", |r| named(r, "type", src));
            format!("func ({}).{}", squash(typ), named(n, "name", src))
        }
        k @ ("type_declaration" | "const_declaration" | "var_declaration") => {
            fn names(n: Node, src: &str, out: &mut Vec<String>) {
                for c in named_children(n) {
                    if c.kind().ends_with("_spec_list") {
                        names(c, src, out);
                    } else {
                        let mut cur = c.walk();
                        out.extend(c.children_by_field_name("name", &mut cur).map(|i| text(Some(i), src).to_string()));
                    }
                }
            }
            let mut ns = vec![];
            names(n, src, &mut ns);
            if ns.is_empty() || ns.iter().any(|n| n == "_") {
                return String::new();
            }
            format!("{} {}", k.trim_end_matches("_declaration"), ns.join(","))
        }
        _ => String::new(),
    }
}

/// An import block's specs; a single unparenthesized import is its own body.
fn go_body(n: Node) -> Option<Node> {
    if n.kind() != "import_declaration" {
        return None;
    }
    named_children(n).into_iter().find(|c| c.kind() == "import_spec_list").or(Some(n))
}

fn py_key(n: Node, src: &str) -> String {
    match n.kind() {
        "function_definition" => format!("def {}", named(n, "name", src)),
        "class_definition" => format!("class {}", named(n, "name", src)),
        "decorated_definition" => n.child_by_field_name("definition").map_or(String::new(), |d| py_key(d, src)),
        "expression_statement" => match n.named_child(0) {
            Some(a) if a.kind() == "assignment" => match a.child_by_field_name("left") {
                Some(l) if l.kind() == "identifier" => format!("= {}", text(Some(l), src)),
                _ => String::new(),
            },
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn py_body(n: Node) -> Option<Node> {
    match n.kind() {
        "class_definition" => n.child_by_field_name("body"),
        "decorated_definition" => n.child_by_field_name("definition").and_then(py_body),
        _ => None,
    }
}

fn js_key(n: Node, src: &str) -> String {
    match n.kind() {
        k @ ("function_declaration"
        | "generator_function_declaration"
        | "class_declaration"
        | "abstract_class_declaration"
        | "interface_declaration"
        | "type_alias_declaration"
        | "enum_declaration"
        | "function_signature"
        | "internal_module"
        | "module") => match named(n, "name", src) {
            "" => String::new(),
            name => format!("{k} {name}"),
        },
        "lexical_declaration" | "variable_declaration" => {
            let names: Vec<&str> = named_children(n)
                .into_iter()
                .filter(|c| c.kind() == "variable_declarator")
                .map(|c| named(c, "name", src))
                .collect();
            if names.is_empty() { String::new() } else { format!("var {}", names.join(",")) }
        }
        "export_statement" => match n.child_by_field_name("declaration").map(|d| js_key(d, src)) {
            Some(k) if !k.is_empty() => format!("export {k}"),
            _ => String::new(),
        },
        "method_definition" | "method_signature" | "abstract_method_signature" => {
            let mut prefix = vec![]; // get/set/static make different members with the same name
            for i in 0..n.child_count() {
                let c = n.child(i).unwrap();
                if matches!(c.kind(), "property_identifier" | "private_property_identifier" | "computed_property_name")
                {
                    return format!("method {} {}", prefix.join(" "), text(Some(c), src));
                }
                if !c.is_named() {
                    prefix.push(c.kind());
                }
            }
            String::new()
        }
        "field_definition" | "public_field_definition" => ["property", "name"]
            .iter()
            .map(|f| named(n, f, src))
            .find(|s| !s.is_empty())
            .map_or(String::new(), |name| format!("field {name}")),
        _ => String::new(),
    }
}

fn js_body(n: Node) -> Option<Node> {
    match n.kind() {
        "class_declaration" | "abstract_class_declaration" | "class" => n.child_by_field_name("body"),
        "export_statement" => n.child_by_field_name("declaration").and_then(js_body),
        _ => None,
    }
}

fn rs_key(n: Node, src: &str) -> String {
    match n.kind() {
        k @ ("function_item"
        | "function_signature_item"
        | "struct_item"
        | "enum_item"
        | "union_item"
        | "trait_item"
        | "type_item"
        | "const_item"
        | "static_item"
        | "mod_item"
        | "macro_definition"
        | "associated_type") => match named(n, "name", src) {
            "" => String::new(),
            name => format!("{k} {name}"),
        },
        "impl_item" => {
            let mut key = format!("impl {}", squash(named(n, "type", src)));
            let tr = named(n, "trait", src);
            if !tr.is_empty() {
                key += &format!(" for {}", squash(tr));
            }
            key
        }
        _ => String::new(),
    }
}

fn rs_body(n: Node) -> Option<Node> {
    match n.kind() {
        "impl_item" | "trait_item" | "mod_item" => n.child_by_field_name("body"),
        _ => None,
    }
}

fn java_key(n: Node, src: &str) -> String {
    match n.kind() {
        k @ ("class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration") => format!("{k} {}", named(n, "name", src)),
        "method_declaration" => format!("method {}{}", named(n, "name", src), squash(named(n, "parameters", src))),
        "constructor_declaration" => format!("ctor {}", squash(named(n, "parameters", src))),
        "field_declaration" => {
            let names: Vec<&str> = named_children(n)
                .into_iter()
                .filter(|c| c.kind() == "variable_declarator")
                .map(|c| named(c, "name", src))
                .collect();
            format!("field {}", names.join(","))
        }
        "package_declaration" => "package".into(),
        _ => String::new(),
    }
}

fn java_body(n: Node) -> Option<Node> {
    match n.kind() {
        "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" => {
            n.child_by_field_name("body")
        }
        _ => None,
    }
}

/// A run of sibling nodes split into keyed units.
#[derive(Default)]
struct Units {
    order: Vec<String>,
    text: HashMap<String, String>,
    /// whitespace after a unit, for all but the last
    gap: HashMap<String, String>,
}

impl Units {
    /// The gap between a and b if they are neighbours here.
    fn between(&self, a: &str, b: &str) -> Option<&str> {
        let i = self.order.iter().position(|k| k == a)?;
        (self.order.get(i + 1)? == b).then(|| self.gap[a].as_str())
    }

    /// The gap in front of k here.
    fn before(&self, k: &str) -> Option<&str> {
        let i = self.order.iter().position(|x| x == k).filter(|&i| i > 0)?;
        Some(self.gap[&self.order[i - 1]].as_str())
    }
}

#[derive(Default)]
struct Side {
    units: Units,
    lead: String,
    tail: String,
}

impl Lang {
    fn parse(&self, src: &str) -> Option<Tree> {
        let mut p = Parser::new();
        p.set_language(&self.lang).ok()?;
        p.parse(src, None).filter(|t| !t.root_node().has_error())
    }

    /// Turns sibling nodes into units; attached nodes (comments...) join the
    /// unit after them. It fails on two units with the same key.
    fn split_units(&self, nodes: &[Node], src: &str) -> Option<Units> {
        let mut u = Units::default();
        let (mut starts, mut ends) = (vec![], vec![]);
        let mut pending: Option<usize> = None;
        let mut add = |u: &mut Units, start: usize, end: usize, key: String| {
            let key = if key.is_empty() { format!("text {}", &src[start..end]) } else { key };
            if u.text.contains_key(&key) {
                return false;
            }
            u.order.push(key.clone());
            u.text.insert(key, src[start..end].to_string());
            starts.push(start);
            ends.push(end);
            true
        };
        for n in nodes {
            if self.attached.contains(&n.kind()) {
                pending.get_or_insert(n.start_byte());
                continue;
            }
            let start = pending.take().unwrap_or(n.start_byte());
            if !add(&mut u, start, n.end_byte(), (self.key)(*n, src)) {
                return None;
            }
        }
        if let Some(p) = pending {
            // comments at the very end
            if !add(&mut u, p, nodes.last()?.end_byte(), String::new()) {
                return None;
            }
        }
        for i in 0..u.order.len().saturating_sub(1) {
            u.gap.insert(u.order[i].clone(), src[ends[i]..starts[i + 1]].to_string());
        }
        Some(u)
    }

    /// Splits the members of n (the root, or a container's body) into units.
    fn side(&self, n: Node, src: &str, from: usize, to: usize) -> Option<Side> {
        let kids = named_children(n);
        let (Some(first), Some(last)) = (kids.first(), kids.last()) else {
            return Some(Side { lead: src[from..to].to_string(), ..Default::default() });
        };
        Some(Side {
            units: self.split_units(&kids, src)?,
            lead: src[from..first.start_byte()].to_string(),
            tail: src[last.end_byte()..to].to_string(),
        })
    }

    /// Merges three unit runs. gap separates units that were not neighbours
    /// on the side they came from.
    fn merge_sides(&self, b: &Side, o: &Side, t: &Side, gap: &str, depth: usize) -> Option<String> {
        let (lead, keep) = pick3(&b.lead, &o.lead, &t.lead, true, true, true, None)?;
        if !keep && !o.lead.is_empty() {
            return None;
        }
        let (tail, keep) = pick3(&b.tail, &o.tail, &t.tail, true, true, true, None)?;
        if !keep && !o.tail.is_empty() {
            return None;
        }
        let order = merge_order(&b.units.text, &o.units.order, &o.units.text, &t.units.order, &t.units.text)?;
        let mut kept = vec![];
        let mut texts = HashMap::new();
        for k in &order {
            let get = |s: &Side| s.units.text.get(k).cloned();
            let (bt, ot, tt) = (get(b), get(o), get(t));
            let both = |b: &str, o: &str, t: &str| self.merge_container(b, o, t, depth + 1);
            let (txt, keep) = pick3(
                bt.as_deref().unwrap_or(""),
                ot.as_deref().unwrap_or(""),
                tt.as_deref().unwrap_or(""),
                bt.is_some(),
                ot.is_some(),
                tt.is_some(),
                Some(&both),
            )?;
            if keep {
                kept.push(k.as_str());
                texts.insert(k.as_str(), txt);
            }
        }
        let mut out = lead;
        for (i, k) in kept.iter().enumerate() {
            out += &texts[k];
            let Some(next) = kept.get(i + 1) else { break };
            let g = o
                .units
                .between(k, next)
                .or_else(|| t.units.between(k, next))
                // new neighbours: space the later one as on the side it comes from
                .or_else(|| if o.units.text.contains_key(*next) { o.units.before(next) } else { t.units.before(next) })
                .unwrap_or(gap);
            out += g;
        }
        out += &tail;
        Some(out)
    }

    /// Merges one unit both sides changed: member by member if it is a
    /// class-like container, else as lines.
    fn merge_container(&self, b: &str, o: &str, t: &str, depth: usize) -> Option<String> {
        let lines = || merge3_text(b, o, t);
        if depth > 3 {
            return lines();
        }
        let mut sides = vec![];
        let mut gap = String::new();
        for (i, src) in [b, o, t].into_iter().enumerate() {
            let Some(tree) = self.parse(src) else { return lines() };
            let kids = named_children(tree.root_node());
            let body = if kids.len() == 1 { (self.body)(kids[0]) } else { None };
            let Some(body) = body.filter(|b| b.named_child_count() > 0) else { return lines() };
            let Some(s) = self.side(body, src, 0, src.len()) else { return lines() };
            if i == 1 {
                // members' default separator: as in ours, else a blank line at ours' indent
                let first = body.named_child(0).unwrap().start_byte();
                let indent = first - src[..first].rfind('\n').map_or(0, |i| i + 1);
                gap = s
                    .units
                    .order
                    .first()
                    .and_then(|k| s.units.gap.get(k))
                    .cloned()
                    .unwrap_or(format!("\n\n{}", " ".repeat(indent)));
            }
            sides.push(s);
        }
        self.merge_sides(&sides[0], &sides[1], &sides[2], &gap, depth)
    }
}

/// Merges one unit: present flags and texts for base, ours, theirs. both
/// merges a unit both sides changed (None: a line merge of its text).
/// Returns the text and whether it stays, or None for a conflict.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn pick3(
    b: &str,
    o: &str,
    t: &str,
    hb: bool,
    ho: bool,
    ht: bool,
    both: Option<&dyn Fn(&str, &str, &str) -> Option<String>>,
) -> Option<(String, bool)> {
    match () {
        _ if ho && ht && o == t => Some((o.into(), true)),
        _ if ho && ht && hb && o == b => Some((t.into(), true)),
        _ if ho && ht && hb && t == b => Some((o.into(), true)),
        _ if ho && ht => Some((both.map_or_else(|| merge3_text(b, o, t), |f| f(b, o, t))?, true)),
        _ if ho && !hb => Some((o.into(), true)),
        _ if ht && !hb => Some((t.into(), true)),
        _ if (ho && o == b) || (ht && t == b) => Some((String::new(), false)), // deleted on the other side, untouched here
        _ if !ho && !ht => Some((String::new(), false)),
        _ => None, // deleted on one side, changed on the other
    }
}

/// The merged sequence of unit keys: ours' order, with theirs' additions
/// placed after the unit they follow in theirs. None when ours deleted a
/// unit that theirs changed.
fn merge_order(
    b: &HashMap<String, String>,
    o_order: &[String],
    o: &HashMap<String, String>,
    t_order: &[String],
    t: &HashMap<String, String>,
) -> Option<Vec<String>> {
    let mut order = o_order.to_vec();
    for (i, k) in t_order.iter().enumerate() {
        if o.contains_key(k) || b.contains_key(k) {
            continue; // in ours, or ours deleted it
        }
        let mut at = (0..i)
            .rev()
            .find_map(|j| order.iter().position(|x| *x == t_order[j]).map(|p| p + 1))
            .unwrap_or(order.len());
        while at < order.len() && !b.contains_key(&order[at]) && !t.contains_key(&order[at]) {
            at += 1; // after what ours added at the same spot, as git does
        }
        order.insert(at, k.clone());
    }
    for (k, bt) in b {
        if !o.contains_key(k) && !order.contains(k) && t.get(k).is_some_and(|tt| tt != bt) {
            return None;
        }
    }
    Some(order)
}

/// A structure-aware merge of a file in a supported language, or None.
pub fn code_merge(path: &str, base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    let l = lang_for(path)?;
    let srcs = [base, ours, theirs].map(|s| std::str::from_utf8(s).ok());
    let srcs = [srcs[0]?, srcs[1]?, srcs[2]?];
    let mut sides = vec![];
    for src in srcs {
        let tree = l.parse(src)?;
        sides.push(l.side(tree.root_node(), src, 0, src.len())?);
    }
    let merged = l.merge_sides(&sides[0], &sides[1], &sides[2], l.gap, 0)?;
    l.parse(&merged)?; // the result must parse cleanly
    Some(merged.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{merge3, split_lines};

    fn line_conflicts(b: &str, o: &str, t: &str) -> usize {
        merge3(&split_lines(b.as_bytes()), &split_lines(o.as_bytes()), &split_lines(t.as_bytes()), "o", "t").conflicts
    }

    fn cm(f: &str, b: &str, o: &str, t: &str) -> Option<String> {
        code_merge(f, b.as_bytes(), o.as_bytes(), t.as_bytes()).map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn go_aware_merge() {
        let base = "package x\n\nimport \"strings\"\n\n// A says hi.\nfunc A() string { return strings.ToUpper(\"a\") }\n\nfunc B() int { return 1 }\n";
        let ours = "package x\n\nimport (\n\t\"fmt\"\n\t\"strings\"\n)\n\n// A says hi.\nfunc A() string { return fmt.Sprint(strings.ToUpper(\"a\")) }\n\nfunc B() int { return 1 }\n\nfunc C() {}\n";
        let theirs = "package x\n\nimport (\n\t\"os\"\n\t\"strings\"\n)\n\n// A says hi.\nfunc A() string { return strings.ToUpper(\"a\") }\n\nfunc B() int { return len(os.Args) }\n\nfunc D() {}\n";
        assert!(line_conflicts(base, ours, theirs) > 0, "test needs a case the line merge cannot do");
        let got = cm("x.go", base, ours, theirs).expect("Go-aware merge gave up");
        for want in
            ["\"fmt\"", "\"os\"", "\"strings\"", "fmt.Sprint(", "len(os.Args)", "func C()", "func D()", "// A says hi."]
        {
            assert!(got.contains(want), "merged file lacks {want}:\n{got}");
        }
        assert_eq!(got.matches("\"strings\"").count(), 1, "{got}");
        let (o2, t2) = (base.replacen("return 1", "return 2", 1), base.replacen("return 1", "return 3", 1));
        assert!(cm("x.go", base, &o2, &t2).is_none(), "conflicting edits to one function merged");
    }

    #[test]
    fn tree_sitter_merge() {
        let cases: &[(&str, &str, &str, &str, &[&str])] = &[
            (
                "m.py",
                "import os\n\n\ndef a():\n    return 1\n",
                "import os\nimport sys\n\n\ndef a():\n    return 1\n\n\ndef b():\n    return sys.argv\n",
                "import os\nimport json\n\n\ndef a():\n    return 1\n\n\ndef c():\n    return json.dumps({})\n",
                &["import sys", "import json", "def b()", "def c()"],
            ),
            (
                "m.js",
                "import a from 'a';\n\nfunction one() { return 1; }\n",
                "import a from 'a';\nimport b from 'b';\n\nfunction one() { return 1; }\n\nfunction two() { return b; }\n",
                "import a from 'a';\nimport c from 'c';\n\nfunction one() { return 1; }\n\nexport const three = c;\n",
                &["import b from 'b'", "import c from 'c'", "function two()", "export const three"],
            ),
            (
                "m.ts",
                "export interface A { x: number }\n",
                "export interface A { x: number }\n\nexport type B = string;\n",
                "export interface A { x: number }\n\nexport function c(): number { return 1; }\n",
                &["type B", "function c()"],
            ),
            (
                "m.rs",
                "use std::io;\n\nfn a() {}\n",
                "use std::io;\nuse std::fs;\n\nfn a() {}\n\nfn b() {}\n",
                "use std::io;\nuse std::env;\n\nfn a() {}\n\n#[test]\nfn c() {}\n",
                &["use std::fs;", "use std::env;", "fn b()", "#[test]\nfn c()"],
            ),
            (
                "M.java",
                "package p;\n\nimport java.util.List;\n\nclass M {\n    int a() { return 1; }\n}\n",
                "package p;\n\nimport java.util.List;\nimport java.util.Map;\n\nclass M {\n    int a() { return 1; }\n\n    int b() { return 2; }\n}\n",
                "package p;\n\nimport java.util.List;\nimport java.util.Set;\n\nclass M {\n    int a() { return 1; }\n\n    int c() { return 3; }\n}\n",
                &["import java.util.Map;", "import java.util.Set;", "int b()", "int c()"],
            ),
            (
                "k.py", // members of one class, changed on both sides
                "class K:\n    def a(self):\n        return 1\n\n    def b(self):\n        return 2\n",
                "class K:\n    def a(self):\n        return 10\n\n    def b(self):\n        return 2\n",
                "class K:\n    def a(self):\n        return 1\n\n    def b(self):\n        return 20\n\n    def c(self):\n        return 3\n",
                &["return 10", "return 20", "def c(self)"],
            ),
        ];
        for (file, base, ours, theirs, want) in cases {
            let Some(got) = cm(file, base, ours, theirs) else {
                panic!(
                    "{file}: structural merge gave up (line merge conflicts: {})",
                    line_conflicts(base, ours, theirs)
                );
            };
            for w in *want {
                assert!(got.contains(w), "{file}: result lacks {w:?}:\n{got}");
            }
        }
        let b = "def f():\n    return 1\n";
        assert!(cm("x.py", b, "def f():\n    return 2\n", "def f():\n    return 3\n").is_none());
        let jb = "class M {\n    int a() { return 1; }\n}\n";
        assert!(cm("M.java", jb, &jb.replacen('1', "2", 1), &jb.replacen('1', "3", 1)).is_none());
        assert!(cm("x.txt", "a\n", "b\n", "c\n").is_none(), "plain text was merged structurally");
    }
}
