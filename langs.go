package main

import (
	"path"
	"strings"
	"sync"

	sitter "github.com/tree-sitter/go-tree-sitter"
	java "github.com/tree-sitter/tree-sitter-java/bindings/go"
	javascript "github.com/tree-sitter/tree-sitter-javascript/bindings/go"
	python "github.com/tree-sitter/tree-sitter-python/bindings/go"
	rust "github.com/tree-sitter/tree-sitter-rust/bindings/go"
	typescript "github.com/tree-sitter/tree-sitter-typescript/bindings/go"
)

// Structure-aware merge for other languages, using tree-sitter grammars.
// The approach is the one goMerge uses: split the file into top-level units
// keyed by what they declare, merge per unit, keep ours' order with theirs'
// additions after the unit they follow. Statements that declare nothing
// (imports, expressions) are keyed by their text, which makes imports merge
// as a set. Comments, decorators and attributes stay with the unit after
// them. When both sides changed the same class (or impl, or Java type), its
// members are merged the same way, one level down. The result must parse
// without errors; otherwise the line merge's conflict stands.

type codeLang struct {
	lang     *sitter.Language
	attached map[string]bool                         // node kinds that belong to the unit after them
	key      func(n *sitter.Node, src []byte) string // "" means: key by the text
	body     func(n *sitter.Node) *sitter.Node       // a container's members node, or nil
	gap      string                                  // default separator between top-level units
}

var codeLangs = sync.OnceValue(func() map[string]*codeLang {
	py := &codeLang{sitter.NewLanguage(python.Language()), set("comment"), pyKey, pyBody, "\n\n\n"}
	js := &codeLang{sitter.NewLanguage(javascript.Language()), set("comment"), jsKey, jsBody, "\n\n"}
	ts := &codeLang{sitter.NewLanguage(typescript.LanguageTypescript()), set("comment"), jsKey, jsBody, "\n\n"}
	tsx := &codeLang{sitter.NewLanguage(typescript.LanguageTSX()), set("comment"), jsKey, jsBody, "\n\n"}
	rs := &codeLang{sitter.NewLanguage(rust.Language()), set("line_comment", "block_comment", "attribute_item", "inner_attribute_item"), rsKey, rsBody, "\n\n"}
	jv := &codeLang{sitter.NewLanguage(java.Language()), set("line_comment", "block_comment"), javaKey, javaBody, "\n\n"}
	return map[string]*codeLang{".py": py, ".js": js, ".mjs": js, ".cjs": js, ".jsx": js,
		".ts": ts, ".mts": ts, ".cts": ts, ".tsx": tsx, ".rs": rs, ".java": jv}
})

func set(kinds ...string) map[string]bool {
	m := map[string]bool{}
	for _, k := range kinds {
		m[k] = true
	}
	return m
}

func text(n *sitter.Node, src []byte) string {
	if n == nil {
		return ""
	}
	return n.Utf8Text(src)
}

func named(n *sitter.Node, field string, src []byte) string {
	return text(n.ChildByFieldName(field), src)
}

func pyKey(n *sitter.Node, src []byte) string {
	switch n.Kind() {
	case "function_definition":
		return "def " + named(n, "name", src)
	case "class_definition":
		return "class " + named(n, "name", src)
	case "decorated_definition":
		if d := n.ChildByFieldName("definition"); d != nil {
			return pyKey(d, src)
		}
	case "expression_statement":
		if a := n.NamedChild(0); a != nil && a.Kind() == "assignment" {
			if l := a.ChildByFieldName("left"); l != nil && l.Kind() == "identifier" {
				return "= " + text(l, src)
			}
		}
	}
	return ""
}

func pyBody(n *sitter.Node) *sitter.Node {
	switch n.Kind() {
	case "class_definition":
		return n.ChildByFieldName("body")
	case "decorated_definition":
		if d := n.ChildByFieldName("definition"); d != nil {
			return pyBody(d)
		}
	}
	return nil
}

func jsKey(n *sitter.Node, src []byte) string {
	switch k := n.Kind(); k {
	case "function_declaration", "generator_function_declaration", "class_declaration", "abstract_class_declaration",
		"interface_declaration", "type_alias_declaration", "enum_declaration", "function_signature", "internal_module", "module":
		if name := named(n, "name", src); name != "" {
			return k + " " + name
		}
	case "lexical_declaration", "variable_declaration":
		var names []string
		for i := uint(0); i < n.NamedChildCount(); i++ {
			if c := n.NamedChild(i); c.Kind() == "variable_declarator" {
				names = append(names, named(c, "name", src))
			}
		}
		if len(names) > 0 {
			return "var " + strings.Join(names, ",")
		}
	case "export_statement":
		if d := n.ChildByFieldName("declaration"); d != nil {
			if k := jsKey(d, src); k != "" {
				return "export " + k
			}
		}
	case "method_definition", "method_signature", "abstract_method_signature":
		var prefix []string // get/set/static make different members with the same name
		for i := uint(0); i < n.ChildCount(); i++ {
			c := n.Child(i)
			if c.Kind() == "property_identifier" || c.Kind() == "private_property_identifier" || c.Kind() == "computed_property_name" {
				return "method " + strings.Join(prefix, " ") + " " + text(c, src)
			}
			if !c.IsNamed() {
				prefix = append(prefix, c.Kind())
			}
		}
	case "field_definition", "public_field_definition":
		for _, f := range []string{"property", "name"} {
			if name := named(n, f, src); name != "" {
				return "field " + name
			}
		}
	}
	return ""
}

func jsBody(n *sitter.Node) *sitter.Node {
	switch n.Kind() {
	case "class_declaration", "abstract_class_declaration", "class":
		return n.ChildByFieldName("body")
	case "export_statement":
		if d := n.ChildByFieldName("declaration"); d != nil {
			return jsBody(d)
		}
	}
	return nil
}

func rsKey(n *sitter.Node, src []byte) string {
	switch k := n.Kind(); k {
	case "function_item", "function_signature_item", "struct_item", "enum_item", "union_item", "trait_item",
		"type_item", "const_item", "static_item", "mod_item", "macro_definition", "associated_type":
		if name := named(n, "name", src); name != "" {
			return k + " " + name
		}
	case "impl_item":
		key := "impl " + strings.Join(strings.Fields(named(n, "type", src)), " ")
		if tr := named(n, "trait", src); tr != "" {
			key += " for " + strings.Join(strings.Fields(tr), " ")
		}
		return key
	}
	return ""
}

func rsBody(n *sitter.Node) *sitter.Node {
	switch n.Kind() {
	case "impl_item", "trait_item", "mod_item":
		return n.ChildByFieldName("body")
	}
	return nil
}

func javaKey(n *sitter.Node, src []byte) string {
	switch k := n.Kind(); k {
	case "class_declaration", "interface_declaration", "enum_declaration", "record_declaration", "annotation_type_declaration":
		return k + " " + named(n, "name", src)
	case "method_declaration":
		return "method " + named(n, "name", src) + strings.Join(strings.Fields(named(n, "parameters", src)), " ")
	case "constructor_declaration":
		return "ctor " + strings.Join(strings.Fields(named(n, "parameters", src)), " ")
	case "field_declaration":
		var names []string
		for i := uint(0); i < n.NamedChildCount(); i++ {
			if c := n.NamedChild(i); c.Kind() == "variable_declarator" {
				names = append(names, named(c, "name", src))
			}
		}
		return "field " + strings.Join(names, ",")
	case "package_declaration":
		return "package"
	}
	return ""
}

func javaBody(n *sitter.Node) *sitter.Node {
	switch n.Kind() {
	case "class_declaration", "interface_declaration", "enum_declaration", "record_declaration":
		return n.ChildByFieldName("body")
	}
	return nil
}

// codeUnits is a run of sibling nodes split into keyed units.
type codeUnits struct {
	order []string
	text  map[string]string
	gap   map[string]string // whitespace after a unit, for all but the last
}

// between is the gap between a and b if they are neighbours here.
func (u *codeUnits) between(a, b string) (string, bool) {
	i := indexOf(u.order, a)
	if i < 0 || i+1 >= len(u.order) || u.order[i+1] != b {
		return "", false
	}
	return u.gap[a], true
}

// before is the gap in front of k here.
func (u *codeUnits) before(k string) (string, bool) {
	if i := indexOf(u.order, k); i > 0 {
		return u.gap[u.order[i-1]], true
	}
	return "", false
}

// splitUnits turns sibling nodes into units; attached nodes (comments...)
// join the unit after them. It fails on two units with the same key.
func (l *codeLang) splitUnits(nodes []*sitter.Node, src []byte) (*codeUnits, bool) {
	u := &codeUnits{text: map[string]string{}, gap: map[string]string{}}
	var ends []int
	pending := -1
	add := func(start, end int, key string) bool {
		if key == "" {
			key = "text " + string(src[start:end])
		}
		if _, dup := u.text[key]; dup {
			return false
		}
		u.order = append(u.order, key)
		u.text[key] = string(src[start:end])
		ends = append(ends, end)
		return true
	}
	starts := []int{}
	for _, n := range nodes {
		if l.attached[n.Kind()] {
			if pending < 0 {
				pending = int(n.StartByte())
			}
			continue
		}
		start := int(n.StartByte())
		if pending >= 0 {
			start, pending = pending, -1
		}
		starts = append(starts, start)
		if !add(start, int(n.EndByte()), l.key(n, src)) {
			return nil, false
		}
	}
	if pending >= 0 { // comments at the very end
		last := nodes[len(nodes)-1]
		starts = append(starts, pending)
		if !add(pending, int(last.EndByte()), "") {
			return nil, false
		}
	}
	for i := 0; i+1 < len(u.order); i++ {
		u.gap[u.order[i]] = string(src[ends[i]:starts[i+1]])
	}
	return u, true
}

// parseUnits parses src and splits root's children into units, returning
// also the text before the first unit and after the last.
func (l *codeLang) parse(src []byte) (*sitter.Tree, *sitter.Node, bool) {
	p := sitter.NewParser()
	defer p.Close()
	if p.SetLanguage(l.lang) != nil {
		return nil, nil, false
	}
	t := p.Parse(src, nil)
	if t == nil {
		return nil, nil, false
	}
	root := t.RootNode()
	if root.HasError() {
		t.Close()
		return nil, nil, false
	}
	return t, root, true
}

func children(n *sitter.Node) []*sitter.Node {
	var out []*sitter.Node
	for i := uint(0); i < n.NamedChildCount(); i++ {
		out = append(out, n.NamedChild(i))
	}
	return out
}

type codeSide struct {
	units      *codeUnits
	lead, tail string
}

// side splits the members of n (the root, or a container's body) into units.
func (l *codeLang) side(n *sitter.Node, src []byte, from, to int) (codeSide, bool) {
	kids := children(n)
	if len(kids) == 0 {
		return codeSide{&codeUnits{text: map[string]string{}, gap: map[string]string{}}, string(src[from:to]), ""}, true
	}
	u, ok := l.splitUnits(kids, src)
	if !ok {
		return codeSide{}, false
	}
	first := int(kids[0].StartByte())
	last := int(kids[len(kids)-1].EndByte())
	return codeSide{u, string(src[from:first]), string(src[last:to])}, true
}

// mergeSides merges three unit runs. gap separates units that were not
// neighbours on the side they came from.
func (l *codeLang) mergeSides(b, o, t codeSide, gap string, depth int) (string, bool) {
	lead, keep, ok := pick3(b.lead, o.lead, t.lead, true, true, true)
	if !ok || !keep && o.lead != "" {
		return "", false
	}
	tail, keep, ok := pick3(b.tail, o.tail, t.tail, true, true, true)
	if !ok || !keep && o.tail != "" {
		return "", false
	}
	order, ok := mergeOrder(b.units.text, o.units.order, o.units.text, t.units.order, t.units.text)
	if !ok {
		return "", false
	}
	var kept []string
	texts := map[string]string{}
	for _, k := range order {
		bt, hb := b.units.text[k]
		ot, ho := o.units.text[k]
		tt, ht := t.units.text[k]
		txt, keep, ok := pick3With(bt, ot, tt, hb, ho, ht, func(bt, ot, tt string) (string, bool) {
			return l.mergeContainer(bt, ot, tt, depth+1)
		})
		if !ok {
			return "", false
		}
		if keep {
			kept = append(kept, k)
			texts[k] = txt
		}
	}
	var out strings.Builder
	out.WriteString(lead)
	for i, k := range kept {
		out.WriteString(texts[k])
		if i+1 == len(kept) {
			break
		}
		next := kept[i+1]
		g, ok := o.units.between(k, next)
		if !ok {
			g, ok = t.units.between(k, next)
		}
		if !ok { // new neighbours: space the later one as on the side it comes from
			if _, inO := o.units.text[next]; inO {
				g, ok = o.units.before(next)
			} else {
				g, ok = t.units.before(next)
			}
		}
		if !ok {
			g = gap
		}
		out.WriteString(g)
	}
	out.WriteString(tail)
	return out.String(), true
}

// mergeContainer merges one unit both sides changed: member by member if it
// is a class-like container, else as lines.
func (l *codeLang) mergeContainer(b, o, t string, depth int) (string, bool) {
	lines := func() (string, bool) {
		res := Merge3(splitLines([]byte(b)), splitLines([]byte(o)), splitLines([]byte(t)), "ours", "theirs")
		return strings.Join(res.Lines, ""), res.Conflicts == 0
	}
	if depth > 3 {
		return lines()
	}
	var sides [3]codeSide
	var gap string
	for i, src := range []string{b, o, t} {
		tr, root, ok := l.parse([]byte(src))
		if !ok {
			return lines()
		}
		defer tr.Close()
		kids := children(root)
		var body *sitter.Node
		if len(kids) == 1 {
			body = l.body(kids[0])
		}
		if body == nil || body.NamedChildCount() == 0 {
			return lines()
		}
		s, ok := l.side(body, []byte(src), 0, len(src))
		if !ok {
			return lines()
		}
		sides[i] = s
		if i == 1 { // members' default separator: as in ours, else a blank line at ours' indent
			first := int(body.NamedChild(0).StartByte())
			indent := first - (strings.LastIndexByte(src[:first], '\n') + 1)
			gap = "\n\n" + strings.Repeat(" ", max(indent, 0))
			for _, g := range s.units.gap {
				gap = g
				break
			}
		}
	}
	return l.mergeSides(sides[0], sides[1], sides[2], gap, depth)
}

// treeSitterMerge merges a file in a supported language, or reports false.
func treeSitterMerge(p string, base, ours, theirs []byte) ([]byte, bool) {
	l := codeLangs()[strings.ToLower(path.Ext(p))]
	if l == nil {
		return nil, false
	}
	var sides [3]codeSide
	for i, src := range [][]byte{base, ours, theirs} {
		tr, root, ok := l.parse(src)
		if !ok {
			return nil, false
		}
		defer tr.Close()
		if sides[i], ok = l.side(root, src, 0, len(src)); !ok {
			return nil, false
		}
	}
	merged, ok := l.mergeSides(sides[0], sides[1], sides[2], l.gap, 0)
	if !ok {
		return nil, false
	}
	tr, _, ok := l.parse([]byte(merged))
	if !ok {
		return nil, false // the result must parse cleanly
	}
	tr.Close()
	return []byte(merged), true
}

// codeMerge picks the structure-aware merge for a file, if there is one.
func codeMerge(p string, base, ours, theirs []byte) ([]byte, bool) {
	if strings.HasSuffix(p, ".go") {
		return goMerge(base, ours, theirs)
	}
	return treeSitterMerge(p, base, ours, theirs)
}
