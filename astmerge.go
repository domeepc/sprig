package main

import (
	"bytes"
	"go/ast"
	"go/format"
	"go/parser"
	"go/token"
	"sort"
	"strings"
)

// Structure-aware merge for Go source, using the standard library's parser.
// A line merge conflicts whenever both sides touch neighbouring lines, which
// in Go is mostly "both added an import" or "both appended a function". Here
// the file is split into its imports and its top-level declarations, keyed by
// name; imports merge as a set and each declaration merges on its own. Any
// case this does not understand falls back to the line merge's conflict.

type goFile struct {
	header  string            // package clause and comments, imports removed
	imports map[string]string // "name path" -> spec text
	order   []string          // declaration keys in file order
	decls   map[string]string // key -> source text, up to the next declaration
}

func parseGoFile(src []byte) (*goFile, bool) {
	if len(src) == 0 {
		return &goFile{imports: map[string]string{}, decls: map[string]string{}}, true
	}
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, "", src, parser.ParseComments)
	if err != nil {
		return nil, false
	}
	off := func(p token.Pos) int { return fset.Position(p).Offset }
	g := &goFile{imports: map[string]string{}, decls: map[string]string{}}
	type unit struct {
		key        string
		start, end int
	}
	var units []unit
	var header strings.Builder
	last := 0
	for _, d := range f.Decls {
		start := off(d.Pos())
		if doc := declDoc(d); doc != nil {
			start = off(doc.Pos())
		}
		if gd, ok := d.(*ast.GenDecl); ok && gd.Tok == token.IMPORT {
			header.Write(src[last:start])
			for _, s := range gd.Specs {
				is := s.(*ast.ImportSpec)
				name := ""
				if is.Name != nil {
					name = is.Name.Name
				}
				g.imports[name+" "+is.Path.Value] = string(src[off(is.Pos()):off(is.End())])
			}
			last = off(d.End())
			continue
		}
		key := declKey(d, src, off)
		if key == "" || g.decls[key] != "" {
			return nil, false // unnamed or repeated (init, var _): no stable identity
		}
		g.decls[key] = "x" // reserve; text filled in below
		units = append(units, unit{key, start, 0})
	}
	if len(units) == 0 {
		header.Write(src[last:])
	} else {
		header.Write(src[last:units[0].start])
	}
	for i := range units {
		end := len(src)
		if i+1 < len(units) {
			end = units[i+1].start
		}
		// trailing blank lines depend on what follows, not on the declaration
		g.decls[units[i].key] = strings.TrimRight(string(src[units[i].start:end]), " \t\n")
		g.order = append(g.order, units[i].key)
	}
	g.header = strings.TrimRight(header.String(), " \t\n")
	return g, true
}

func declDoc(d ast.Decl) *ast.CommentGroup {
	switch d := d.(type) {
	case *ast.FuncDecl:
		return d.Doc
	case *ast.GenDecl:
		return d.Doc
	}
	return nil
}

func declKey(d ast.Decl, src []byte, off func(token.Pos) int) string {
	switch d := d.(type) {
	case *ast.FuncDecl:
		if d.Name.Name == "init" || d.Name.Name == "_" {
			return ""
		}
		if d.Recv != nil && len(d.Recv.List) > 0 {
			t := d.Recv.List[0].Type
			return "func (" + string(src[off(t.Pos()):off(t.End())]) + ")." + d.Name.Name
		}
		return "func " + d.Name.Name
	case *ast.GenDecl:
		var names []string
		for _, s := range d.Specs {
			switch s := s.(type) {
			case *ast.TypeSpec:
				names = append(names, s.Name.Name)
			case *ast.ValueSpec:
				for _, n := range s.Names {
					if n.Name == "_" {
						return ""
					}
					names = append(names, n.Name)
				}
			}
		}
		if len(names) == 0 {
			return ""
		}
		return d.Tok.String() + " " + strings.Join(names, ",")
	}
	return ""
}

// pick3 merges one unit: present flags and texts for base, ours, theirs.
func pick3(b, o, t string, hb, ho, ht bool) (string, bool, bool) {
	return pick3With(b, o, t, hb, ho, ht, nil)
}

// pick3With is pick3 with a merge for "both sides changed it" (nil: a line
// merge of the unit's text).
func pick3With(b, o, t string, hb, ho, ht bool, both func(b, o, t string) (string, bool)) (string, bool, bool) {
	switch {
	case ho && ht && o == t:
		return o, true, true
	case ho && ht && hb && o == b:
		return t, true, true
	case ho && ht && hb && t == b:
		return o, true, true
	case ho && ht && both != nil: // both changed (or both added) the same declaration
		text, ok := both(b, o, t)
		return text, true, ok
	case ho && ht:
		res := Merge3(splitLines([]byte(b)), splitLines([]byte(o)), splitLines([]byte(t)), "ours", "theirs")
		return strings.Join(res.Lines, ""), true, res.Conflicts == 0
	case ho && !hb:
		return o, true, true
	case ht && !hb:
		return t, true, true
	case ho && o == b, ht && t == b: // deleted on the other side, untouched here
		return "", false, true
	case !ho && !ht:
		return "", false, true
	}
	return "", false, false // deleted on one side, changed on the other
}

func goMerge(base, ours, theirs []byte) ([]byte, bool) {
	bf, ok1 := parseGoFile(base)
	of, ok2 := parseGoFile(ours)
	tf, ok3 := parseGoFile(theirs)
	if !ok1 || !ok2 || !ok3 {
		return nil, false
	}
	header, keep, ok := pick3(bf.header, of.header, tf.header, true, true, true)
	if !ok || !keep {
		return nil, false
	}
	// imports: ours, plus what theirs added, minus what theirs removed
	imports := map[string]string{}
	for k, v := range of.imports {
		if _, inB := bf.imports[k]; !inB || tf.imports[k] != "" {
			imports[k] = v
		}
	}
	for k, v := range tf.imports {
		if _, inB := bf.imports[k]; !inB {
			imports[k] = v
		}
	}
	order, ok := mergeOrder(bf.decls, of.order, of.decls, tf.order, tf.decls)
	if !ok {
		return nil, false
	}
	var out bytes.Buffer
	out.WriteString(strings.TrimRight(header, "\n") + "\n")
	if len(imports) > 0 {
		specs := make([]string, 0, len(imports))
		for _, v := range imports {
			specs = append(specs, v)
		}
		sort.Strings(specs)
		out.WriteString("\nimport (\n\t" + strings.Join(specs, "\n\t") + "\n)\n\n")
	}
	for _, k := range order {
		b, hb := bf.decls[k]
		o, ho := of.decls[k]
		t, ht := tf.decls[k]
		text, keep, ok := pick3(b, o, t, hb, ho, ht)
		if !ok {
			return nil, false
		}
		if keep {
			out.WriteString(strings.TrimRight(text, "\n") + "\n\n")
		}
	}
	res, err := format.Source(out.Bytes())
	if err != nil {
		return nil, false
	}
	return res, true
}

// mergeOrder gives the merged sequence of unit keys: ours' order, with
// theirs' additions placed after the unit they follow in theirs. It reports
// false when ours deleted a unit that theirs changed.
func mergeOrder(b map[string]string, oOrder []string, o map[string]string, tOrder []string, t map[string]string) ([]string, bool) {
	order := append([]string{}, oOrder...)
	for i, k := range tOrder {
		if _, inO := o[k]; inO {
			continue
		}
		if _, inB := b[k]; inB {
			continue // ours deleted it
		}
		at := len(order)
		for j := i - 1; j >= 0; j-- {
			if p := indexOf(order, tOrder[j]); p >= 0 {
				at = p + 1
				break
			}
		}
		for at < len(order) { // after what ours added at the same spot, as git does
			_, inB := b[order[at]]
			_, inT := t[order[at]]
			if inB || inT {
				break
			}
			at++
		}
		order = append(order[:at], append([]string{k}, order[at:]...)...)
	}
	for k, bt := range b {
		if _, inO := o[k]; !inO && indexOf(order, k) < 0 {
			if tt, inT := t[k]; inT && tt != bt {
				return nil, false
			}
		}
	}
	return order, true
}

func indexOf(list []string, s string) int {
	for i, v := range list {
		if v == s {
			return i
		}
	}
	return -1
}
