package jed_test

import (
	"bytes"
	"go/ast"
	"go/build"
	"go/parser"
	"go/printer"
	"go/token"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// The exported Go API is pinned in testdata/api.txt (spec/design/api.md §11.7): one line per
// exported declaration of an ordinary build, so a newly exported name — an engine internal leaking
// out — or a changed signature fails here until the file is regenerated on purpose:
//
//	JED_UPDATE_API=1 go test -run TestAPIGolden .

const apiGoldenPath = "testdata/api.txt"

func TestAPIGolden(t *testing.T) {
	got := exportedAPI(t)
	if os.Getenv("JED_UPDATE_API") != "" {
		if err := os.WriteFile(apiGoldenPath, []byte(got), 0o644); err != nil {
			t.Fatal(err)
		}
		return
	}
	want, err := os.ReadFile(apiGoldenPath)
	if err != nil {
		t.Fatalf("%v (generate it with JED_UPDATE_API=1 go test -run TestAPIGolden .)", err)
	}
	if got == string(want) {
		return
	}
	g, w := strings.Split(got, "\n"), strings.Split(string(want), "\n")
	var added, removed []string
	for _, l := range g {
		if !slices.Contains(w, l) {
			added = append(added, "+ "+l)
		}
	}
	for _, l := range w {
		if !slices.Contains(g, l) {
			removed = append(removed, "- "+l)
		}
	}
	t.Fatalf("the exported API differs from %s:\n%s\n%s\nIf the change is intended, regenerate the file with\n"+
		"JED_UPDATE_API=1 go test -run TestAPIGolden . and update spec/design/api.md and the docs with it.",
		apiGoldenPath, strings.Join(removed, "\n"), strings.Join(added, "\n"))
}

// exportedAPI lists the package's exported declarations in an ordinary build (default build tags,
// so jedtooling hooks are excluded), sorted, one per line. Parameter names are dropped, so only a
// change of the API itself shows.
func exportedAPI(t *testing.T) string {
	t.Helper()
	ctx := build.Default
	pkg, err := ctx.ImportDir(".", 0)
	if err != nil {
		t.Fatal(err)
	}
	fset := token.NewFileSet()
	var lines []string
	expr := func(e ast.Expr) string {
		var b bytes.Buffer
		if err := printer.Fprint(&b, fset, e); err != nil {
			t.Fatal(err)
		}
		return strings.Join(strings.Fields(b.String()), " ")
	}
	fields := func(fl *ast.FieldList) string {
		if fl == nil {
			return ""
		}
		var parts []string
		for _, f := range fl.List {
			n := max(len(f.Names), 1)
			for range n {
				parts = append(parts, expr(f.Type))
			}
		}
		return strings.Join(parts, ", ")
	}
	typeParams := func(fl *ast.FieldList) string {
		if fl == nil {
			return ""
		}
		var ps []string
		for _, p := range fl.List {
			for _, n := range p.Names {
				ps = append(ps, n.Name+" "+expr(p.Type))
			}
		}
		return "[" + strings.Join(ps, ", ") + "]"
	}
	signature := func(ft *ast.FuncType) string {
		s := "(" + fields(ft.Params) + ")"
		if r := fields(ft.Results); r != "" {
			if ft.Results.NumFields() > 1 {
				r = "(" + r + ")"
			}
			s += " " + r
		}
		return s
	}
	for _, name := range pkg.GoFiles {
		f, err := parser.ParseFile(fset, filepath.Join(pkg.Dir, name), nil, parser.SkipObjectResolution)
		if err != nil {
			t.Fatal(err)
		}
		for _, d := range f.Decls {
			switch d := d.(type) {
			case *ast.FuncDecl:
				if !d.Name.IsExported() {
					continue
				}
				if d.Recv == nil {
					lines = append(lines, "func "+d.Name.Name+typeParams(d.Type.TypeParams)+signature(d.Type))
					continue
				}
				recv := d.Recv.List[0].Type
				base := recv
				if s, ok := base.(*ast.StarExpr); ok {
					base = s.X
				}
				if ix, ok := base.(*ast.IndexExpr); ok {
					base = ix.X
				}
				if id, ok := base.(*ast.Ident); !ok || !id.IsExported() {
					continue // a method of an unexported type is not API
				}
				lines = append(lines, "method ("+expr(recv)+") "+d.Name.Name+signature(d.Type))
			case *ast.GenDecl:
				var groupType ast.Expr // a const spec with no type or value repeats the previous one's
				for _, spec := range d.Specs {
					switch s := spec.(type) {
					case *ast.TypeSpec:
						if !s.Name.IsExported() {
							continue
						}
						lines = append(lines, typeLines(s, expr, typeParams, signature)...)
					case *ast.ValueSpec:
						typ := s.Type
						if d.Tok == token.CONST {
							if typ == nil && len(s.Values) == 0 {
								typ = groupType
							}
							groupType = typ
						}
						for _, n := range s.Names {
							if !n.IsExported() {
								continue
							}
							line := d.Tok.String() + " " + n.Name
							if typ != nil {
								line += " " + expr(typ)
							}
							lines = append(lines, line)
						}
					}
				}
			}
		}
	}
	slices.Sort(lines)
	return strings.Join(lines, "\n") + "\n"
}

// typeLines describes an exported type: its kind, plus each exported struct field and interface
// method as its own line.
func typeLines(s *ast.TypeSpec, expr func(ast.Expr) string, typeParams func(*ast.FieldList) string,
	signature func(*ast.FuncType) string,
) []string {
	name := s.Name.Name + typeParams(s.TypeParams)
	eq := ""
	if s.Assign.IsValid() {
		eq = "= "
	}
	switch t := s.Type.(type) {
	case *ast.StructType:
		out := []string{"type " + name + " " + eq + "struct"}
		for _, f := range t.Fields.List {
			for _, n := range f.Names {
				if n.IsExported() {
					out = append(out, "field "+s.Name.Name+"."+n.Name+" "+expr(f.Type))
				}
			}
			if len(f.Names) == 0 && ast.IsExported(strings.TrimPrefix(expr(f.Type), "*")) {
				out = append(out, "embedded "+s.Name.Name+" "+expr(f.Type))
			}
		}
		return out
	case *ast.InterfaceType:
		out := []string{"type " + name + " " + eq + "interface"}
		for _, m := range t.Methods.List {
			if ft, ok := m.Type.(*ast.FuncType); ok {
				for _, n := range m.Names {
					out = append(out, "imethod "+s.Name.Name+"."+n.Name+signature(ft))
				}
			} else {
				out = append(out, "embedded "+s.Name.Name+" "+expr(m.Type))
			}
		}
		return out
	case *ast.FuncType:
		return []string{"type " + name + " " + eq + "func" + signature(t)}
	default:
		return []string{"type " + name + " " + eq + expr(s.Type)}
	}
}
