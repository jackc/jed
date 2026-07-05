package jed

import "testing"

// Per-operator cost base (functions.md §8). The evaluator charges operatorCost(name) for an
// operator node instead of a flat OperatorEval; operatorCost returns the operator's catalog Cost if
// authored, else the uniform OperatorEval. The conformance corpus CANNOT observe this while every
// built-in uses the uniform default (CLAUDE.md §10), so these per-core tests pin the mechanism.
// Mirrored in Rust (executor.rs registry_tests) and TS (tests/operator_cost.test.ts).

// operatorCost must reflect the generated Operators table for EVERY operator — proving the lookup
// is data-driven, so authoring a Cost in catalog.toml is honored with no evaluator change.
func TestOperatorCostReflectsCatalog(t *testing.T) {
	t.Parallel()
	for _, o := range operators {
		want := costs.OperatorEval
		if o.Cost != 0 {
			want = o.Cost
		}
		if got := operatorCost(o.Name); got != want {
			t.Errorf("operatorCost(%q) = %d, want %d", o.Name, got, want)
		}
	}
	// An unknown name falls back to the uniform OperatorEval.
	if got := operatorCost("definitely_not_an_operator"); got != costs.OperatorEval {
		t.Errorf("unknown operator cost = %d, want %d", got, costs.OperatorEval)
	}
}

// Every operator-enum → catalog-name mapping the evaluator charges through must resolve to a real
// catalog operator, so a typo in catalogName / a wired literal is caught here, not silently masked
// by the uniform-weight fallback.
func TestWiredOperatorNamesExistInCatalog(t *testing.T) {
	t.Parallel()
	var names []string
	for _, op := range []binaryOp{opAdd, opSub, opMul, opDiv, opMod, opEq, opNe, opLt, opGt, opLe, opGe} {
		names = append(names, op.catalogName())
	}
	names = append(names, "neg", "not", "and", "or")
	for _, name := range names {
		found := false
		for _, o := range operators {
			if o.Name == name {
				found = true
				break
			}
		}
		if !found {
			t.Errorf("wired operator name %q is not in the catalog", name)
		}
	}
}
