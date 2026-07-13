package jed

import "testing"

func estimatorNamedSelectivity(t *testing.T, token string) selectivityExpr {
	t.Helper()
	switch token {
	case "all":
		return selectivityExpr{kind: selectivityAll}
	case "zero":
		return selectivityExpr{kind: selectivityZero}
	case "unique":
		return selectivityExpr{kind: selectivityUnique}
	case "equality":
		return fractionSelectivity(selectivityEquality)
	case "inequality":
		return fractionSelectivity(selectivityInequality)
	case "paired_range":
		return fractionSelectivity(selectivityPairedRange)
	case "null_test":
		return fractionSelectivity(selectivityNullTest)
	case "match":
		return fractionSelectivity(selectivityMatch)
	case "matching":
		return fractionSelectivity(selectivityMatching)
	case "boolean":
		return fractionSelectivity(selectivityBoolean)
	case "opaque":
		return fractionSelectivity(selectivityOpaque)
	default:
		t.Fatalf("unknown selectivity token %q", token)
		return selectivityExpr{}
	}
}

func estimatorPostfix(t *testing.T, tokens []string) selectivityExpr {
	t.Helper()
	stack := make([]selectivityExpr, 0, len(tokens))
	for _, token := range tokens {
		switch token {
		case "not":
			stack[len(stack)-1] = stack[len(stack)-1].Not()
		case "and", "or":
			rhs := stack[len(stack)-1]
			lhs := stack[len(stack)-2]
			stack = stack[:len(stack)-2]
			if token == "and" {
				stack = append(stack, lhs.And(rhs))
			} else {
				stack = append(stack, lhs.Or(rhs))
			}
		default:
			stack = append(stack, estimatorNamedSelectivity(t, token))
		}
	}
	if len(stack) != 1 {
		t.Fatalf("postfix stack has %d values", len(stack))
	}
	return stack[0]
}

func TestSharedEstimatorVectors(t *testing.T) {
	for _, row := range readTomlTables(t, specPath(t, "cost/estimator_vectors.toml"), "arithmetic") {
		a, b := row.int("a"), row.int("b")
		var actual int64
		switch row.str("op") {
		case "sat_add":
			actual = satEstimateAdd(a, b)
		case "sat_mul":
			actual = satEstimateMul(a, b)
		case "scale_ceil":
			actual = scaleEstimateCeil(a, estimatorFraction{numerator: b, denominator: row.int("c")})
		default:
			t.Fatalf("%s: unknown arithmetic op", row.str("id"))
		}
		if actual != row.int("expected") {
			t.Errorf("%s: got %d want %d", row.str("id"), actual, row.int("expected"))
		}
	}
	for _, row := range readTomlTables(t, specPath(t, "cost/estimator_vectors.toml"), "predicate") {
		actual := estimateSelectivity(estimatorPostfix(t, row.strs("tokens")), row.int("n"))
		if actual != row.int("expected") {
			t.Errorf("%s: got %d want %d", row.str("id"), actual, row.int("expected"))
		}
	}
	for _, row := range readTomlTables(t, specPath(t, "cost/estimator_vectors.toml"), "candidate") {
		estimate := estimateCandidate(candidateEstimateInputs{
			kind: row.str("kind"), indexName: row.str("index_name"),
			scanRows: row.int("scan_rows"), outputRows: row.int("output_rows"),
			accessPages: row.int("access_pages"), tableHeight: row.int("table_height"),
			filterNodes: row.int("filter_nodes"), accessWork: row.int("access_work"),
			producesRows: row.boolVal("produces_rows"),
		})
		if estimate.rows != row.int("est_rows") || estimate.cost != row.int("est_cost") || estimate.tieKey != row.str("tie_key") {
			t.Errorf("%s: got rows/cost/tie %d/%d/%q", row.str("id"), estimate.rows, estimate.cost, estimate.tieKey)
		}
		for i, id := range estimatorUnitIDs {
			expected := int64(0)
			if row.has("units." + id) {
				expected = row.int("units." + id)
			}
			if estimate.units[i] != expected {
				t.Errorf("%s unit %s: got %d want %d", row.str("id"), id, estimate.units[i], expected)
			}
		}
	}
}
