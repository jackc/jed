// Mutation operators for the Go core's mutation-testing harness.
//
// A mutator walks a parsed Go file and enumerates candidate source edits ("mutants"):
// each is a deliberate bug — a flipped comparison, an off-by-one constant, a dropped
// guard — that *should* make at least one conformance test fail. A mutant the corpus
// still passes is a SURVIVOR: untested logic, quantified (spec/design/mutation-testing.md).
//
// Edits are recorded as byte ranges over the ORIGINAL source, not AST rewrites: each
// mutant is applied by splicing one replacement into a pristine copy of the file, so the
// rest of the file is byte-identical and only the one spot differs. This keeps mutants
// minimal and the compiler's verdict crisp.
package main

import (
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"sort"
	"strings"
)

// Mutation is one candidate source edit over a single target file.
type Mutation struct {
	RelFile string // path relative to the Go module root, for display + workspace lookup
	Mutator string // operator name: comparison, arith, logic, bool, offbyone, condneg, incdec
	Desc    string // human description, e.g. "< -> <="
	Line    int    // 1-based line of the edit (original file)
	Col     int    // 1-based column of the edit
	ByteOff int    // start byte offset in the original file (inclusive)
	ByteEnd int    // end byte offset in the original file (exclusive)
	Orig    string // original text in [ByteOff,ByteEnd) — a sanity anchor
	Repl    string // replacement text
}

// ID is a stable, human-meaningful identifier for a mutant.
func (m Mutation) ID() string {
	return fmt.Sprintf("%s:%d:%d %s %s", m.RelFile, m.Line, m.Col, m.Mutator, m.Desc)
}

// allMutators is the canonical operator set. Each maps directly to a bug class the design
// brief calls out (flip a comparison, off-by-one a boundary, drop a NULL check, swap a
// connective). They are deliberately type-preserving where possible so most mutants compile;
// the few that do not (e.g. swapping + to - on strings) are reported as INVALID, not scored.
var allMutators = []string{"comparison", "arith", "logic", "bool", "offbyone", "condneg", "incdec"}

// enumerate parses src (the original bytes of relFile) and returns every candidate mutation,
// filtered to the enabled operators. The walk order is deterministic, so a given (file, src,
// operator set) always yields the identical mutation list — the property the seeded sampler
// and the reproducibility of a run both rest on.
func enumerate(relFile string, src []byte, enabled map[string]bool) ([]Mutation, error) {
	fset := token.NewFileSet()
	f, err := parser.ParseFile(fset, relFile, src, parser.SkipObjectResolution)
	if err != nil {
		return nil, fmt.Errorf("parse %s: %w", relFile, err)
	}

	// selTargets holds the Idents that are the `.Sel` of a selector (e.g. the `true` in
	// `x.true`), so the bool mutator does not touch a field/method named true/false.
	selTargets := map[*ast.Ident]bool{}
	ast.Inspect(f, func(n ast.Node) bool {
		if sel, ok := n.(*ast.SelectorExpr); ok {
			selTargets[sel.Sel] = true
		}
		return true
	})

	var muts []Mutation
	emit := func(pos token.Pos, off, end int, mutator, desc, repl string) {
		if off < 0 || end > len(src) || off >= end {
			return
		}
		p := fset.Position(pos)
		muts = append(muts, Mutation{
			RelFile: relFile,
			Mutator: mutator,
			Desc:    desc,
			Line:    p.Line,
			Col:     p.Column,
			ByteOff: off,
			ByteEnd: end,
			Orig:    string(src[off:end]),
			Repl:    repl,
		})
	}

	ast.Inspect(f, func(n ast.Node) bool {
		switch x := n.(type) {
		case *ast.BinaryExpr:
			opOff := fset.Position(x.OpPos).Offset
			opEnd := opOff + len(x.Op.String())
			// Guard against position/offset drift: the bytes at the operator must match.
			if opEnd > len(src) || string(src[opOff:opEnd]) != x.Op.String() {
				return true
			}
			switch x.Op {
			case token.LSS, token.LEQ, token.GTR, token.GEQ, token.EQL, token.NEQ:
				if enabled["comparison"] {
					for _, repl := range comparisonSwaps[x.Op] {
						emit(x.OpPos, opOff, opEnd, "comparison",
							fmt.Sprintf("%s -> %s", x.Op, repl), repl.String())
					}
				}
			case token.ADD, token.SUB, token.MUL, token.QUO, token.REM:
				if enabled["arith"] {
					repl := arithSwap[x.Op]
					emit(x.OpPos, opOff, opEnd, "arith",
						fmt.Sprintf("%s -> %s", x.Op, repl), repl.String())
				}
			case token.LAND, token.LOR:
				if enabled["logic"] {
					repl := logicSwap[x.Op]
					emit(x.OpPos, opOff, opEnd, "logic",
						fmt.Sprintf("%s -> %s", x.Op, repl), repl.String())
				}
			}

		case *ast.IncDecStmt:
			if enabled["incdec"] {
				off := fset.Position(x.TokPos).Offset
				end := off + 2 // "++" / "--"
				if end <= len(src) && string(src[off:end]) == x.Tok.String() {
					repl := token.DEC
					if x.Tok == token.DEC {
						repl = token.INC
					}
					emit(x.TokPos, off, end, "incdec",
						fmt.Sprintf("%s -> %s", x.Tok, repl), repl.String())
				}
			}

		case *ast.BasicLit:
			if enabled["offbyone"] && x.Kind == token.INT {
				off := fset.Position(x.Pos()).Offset
				end := off + len(x.Value)
				if end <= len(src) && string(src[off:end]) == x.Value {
					// Wrap rather than re-parse the literal: (N + 1) / (N - 1) is a valid
					// constant expression anywhere an integer literal appears, and sidesteps
					// base/underscore parsing (0x1F, 1_000, big values).
					emit(x.Pos(), off, end, "offbyone",
						fmt.Sprintf("%s -> (%s + 1)", x.Value, x.Value),
						fmt.Sprintf("(%s + 1)", x.Value))
					emit(x.Pos(), off, end, "offbyone",
						fmt.Sprintf("%s -> (%s - 1)", x.Value, x.Value),
						fmt.Sprintf("(%s - 1)", x.Value))
				}
			}

		case *ast.Ident:
			if enabled["bool"] && (x.Name == "true" || x.Name == "false") && !selTargets[x] {
				off := fset.Position(x.Pos()).Offset
				end := off + len(x.Name)
				if end <= len(src) && string(src[off:end]) == x.Name {
					repl := "false"
					if x.Name == "false" {
						repl = "true"
					}
					emit(x.Pos(), off, end, "bool",
						fmt.Sprintf("%s -> %s", x.Name, repl), repl)
				}
			}

		case *ast.IfStmt:
			if enabled["condneg"] && x.Cond != nil {
				off := fset.Position(x.Cond.Pos()).Offset
				end := fset.Position(x.Cond.End()).Offset
				if off >= 0 && end <= len(src) && off < end {
					cond := string(src[off:end])
					emit(x.Cond.Pos(), off, end, "condneg",
						"negate if-condition", "!("+cond+")")
				}
			}
		}
		return true
	})

	// Deterministic order: by byte offset, then by description (two off-by-one mutants share
	// an offset). The AST walk already visits in source order, but sorting makes it explicit
	// and independent of walk-order quirks.
	sort.SliceStable(muts, func(i, j int) bool {
		if muts[i].ByteOff != muts[j].ByteOff {
			return muts[i].ByteOff < muts[j].ByteOff
		}
		return muts[i].Desc < muts[j].Desc
	})
	return muts, nil
}

// apply splices the mutant into a pristine copy of the original source.
func (m Mutation) apply(orig []byte) []byte {
	out := make([]byte, 0, len(orig)-(m.ByteEnd-m.ByteOff)+len(m.Repl))
	out = append(out, orig[:m.ByteOff]...)
	out = append(out, m.Repl...)
	out = append(out, orig[m.ByteEnd:]...)
	return out
}

// comparisonSwaps maps each comparison operator to the mutants we generate for it: one
// boundary shift (< <-> <=) and one negation (< -> >=), the two highest-signal classes.
var comparisonSwaps = map[token.Token][]token.Token{
	token.LSS: {token.LEQ, token.GEQ}, // <  -> <= (boundary), >= (negate)
	token.LEQ: {token.LSS, token.GTR}, // <= -> <  (boundary), >  (negate)
	token.GTR: {token.GEQ, token.LEQ}, // >  -> >= (boundary), <= (negate)
	token.GEQ: {token.GTR, token.LSS}, // >= -> >  (boundary), <  (negate)
	token.EQL: {token.NEQ},            // == -> !=
	token.NEQ: {token.EQL},            // != -> ==
}

var arithSwap = map[token.Token]token.Token{
	token.ADD: token.SUB,
	token.SUB: token.ADD,
	token.MUL: token.QUO,
	token.QUO: token.MUL,
	token.REM: token.MUL,
}

var logicSwap = map[token.Token]token.Token{
	token.LAND: token.LOR,
	token.LOR:  token.LAND,
}

// parseMutators validates and normalizes a comma-separated operator list ("all" = every op).
func parseMutators(spec string) (map[string]bool, error) {
	enabled := map[string]bool{}
	spec = strings.TrimSpace(spec)
	if spec == "" || spec == "all" {
		for _, m := range allMutators {
			enabled[m] = true
		}
		return enabled, nil
	}
	known := map[string]bool{}
	for _, m := range allMutators {
		known[m] = true
	}
	for _, m := range strings.Split(spec, ",") {
		m = strings.TrimSpace(m)
		if m == "" {
			continue
		}
		if !known[m] {
			return nil, fmt.Errorf("unknown mutator %q (known: %s)", m, strings.Join(allMutators, ", "))
		}
		enabled[m] = true
	}
	if len(enabled) == 0 {
		return nil, fmt.Errorf("no mutators selected")
	}
	return enabled, nil
}
