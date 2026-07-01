package jed

// Expression / query nesting-depth limit (CLAUDE.md §13; spec/design/cost.md §7). The §13
// native-stack-safety gate: the recursive-descent parser and the resolve/eval walks recurse to a
// statement's nesting depth, so deeply-nested untrusted input would overflow the call stack BEFORE
// the cost meter runs — 54P01 cannot catch it. A fixed maxExprDepth checked in the parser aborts
// such input with 54001 (statement_too_complex) instead. The conformance corpus
// (spec/conformance/suites/resource/depth_limit.test) pins the cross-core boundary; this exercises
// the per-vector boundary and that the abort is independent of max_cost.

import (
	"fmt"
	"strings"
	"testing"
)

func depthDB(t *testing.T) *Session {
	t.Helper()
	return dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t VALUES (1, 1)")
}

// codeOf returns the SQLSTATE of running sql, or "ok" if it succeeded.
func codeOf(db dbHandle, sql string) string {
	_, err := db.Execute(sql, nil)
	if err == nil {
		return "ok"
	}
	if ee, ok := err.(*EngineError); ok {
		return ee.Code()
	}
	return "non-engine-error"
}

// depthChain builds `1 + 1 + … + 1` with n `+` operators over one row (the canonical `1+1+…`
// vector); its parsed depth is n+1.
func depthChain(n int) string {
	return "SELECT " + strings.Repeat("1 + ", n) + "1 FROM t"
}

func TestDepthLimitIsGenerous(t *testing.T) {
	// Far above any realistic query, so ordinary SQL is never rejected (spec/design/cost.md §7).
	if maxExprDepth != 256 {
		t.Fatalf("maxExprDepth = %d, want 256", maxExprDepth)
	}
}

func TestDepthDeepOperatorChainAborts(t *testing.T) {
	db := depthDB(t)
	// One level past the limit aborts at parse time (the additive loop's counter, O(1) stack).
	if c := codeOf(db, depthChain(maxExprDepth)); c != "54001" {
		t.Fatalf("chain(maxExprDepth) = %s, want 54001", c)
	}
	if c := codeOf(db, depthChain(maxExprDepth*4)); c != "54001" {
		t.Fatalf("chain(4×) = %s, want 54001", c)
	}
	// A moderately-nested expression still evaluates end to end.
	if c := codeOf(db, depthChain(64)); c != "ok" {
		t.Fatalf("chain(64) = %s, want ok", c)
	}
}

func TestDepthExactBoundary(t *testing.T) {
	// Pin the precise accept/reject boundary at the parser (where the 54001 is raised): a `1+1+…`
	// chain parses with O(1) parser stack, so maxExprDepth-1 levels parse fine and maxExprDepth is
	// the first rejected depth. This is the cross-core contract the corpus mirrors.
	if _, err := parseSQL(depthChain(maxExprDepth - 1)); err != nil {
		t.Fatalf("chain(maxExprDepth-1) should parse, got %v", err)
	}
	_, err := parseSQL(depthChain(maxExprDepth))
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54001" {
		t.Fatalf("chain(maxExprDepth) = %v, want 54001", err)
	}
}

func TestDepthAbortIndependentOfMaxCost(t *testing.T) {
	// The overflow this guards strikes during PARSE, before the meter runs — so even an unlimited
	// (or tiny) ceiling cannot let a stack-busting statement through (CLAUDE.md §13).
	db := depthDB(t)
	db.SetMaxCost(0) // unlimited
	if c := codeOf(db, depthChain(maxExprDepth*8)); c != "54001" {
		t.Fatalf("unlimited: %s, want 54001", c)
	}
	db.SetMaxCost(1) // tightest possible ceiling
	if c := codeOf(db, depthChain(maxExprDepth*8)); c != "54001" {
		t.Fatalf("max_cost=1: %s, want 54001", c)
	}
}

func TestDepthEveryVectorAbortsNotCrashes(t *testing.T) {
	// Each recursion vector — nested parens, ARRAY, NOT, unary minus, scalar subqueries, postfix
	// casts, and UNION chains — is bounded by the same counter and returns 54001 deterministically
	// rather than overflowing the native stack. n well past the limit for each.
	db := depthDB(t)
	n := maxExprDepth * 2
	vectors := []string{
		fmt.Sprintf("SELECT %s1%s FROM t", strings.Repeat("(", n), strings.Repeat(")", n)),
		fmt.Sprintf("SELECT %s1%s FROM t", strings.Repeat("ARRAY[", n), strings.Repeat("]", n)),
		fmt.Sprintf("SELECT %strue FROM t", strings.Repeat("NOT ", n)),
		fmt.Sprintf("SELECT %s1 FROM t", strings.Repeat("- ", n)),
		fmt.Sprintf("SELECT %s1%s FROM t", strings.Repeat("(SELECT ", n), strings.Repeat(")", n)),
		fmt.Sprintf("SELECT 1%s FROM t", strings.Repeat("::int4", n)),
		fmt.Sprintf("SELECT 1%s", strings.Repeat(" UNION ALL SELECT 1", n)),
	}
	for _, sql := range vectors {
		if c := codeOf(db, sql); c != "54001" {
			t.Errorf("a %d-deep vector = %s, want 54001 (%.40q)", n, c, sql)
		}
	}
}

func TestDepthNestingInWhereAndCheckIsBounded(t *testing.T) {
	// The guard sits in the parser, so it protects every clause holding an expression — WHERE and a
	// CHECK constraint included (these reach the pre-resolve structural walks, which the parser
	// bound keeps shallow).
	db := depthDB(t)
	pred := strings.Repeat("1 + ", maxExprDepth+2) + "1"
	if c := codeOf(db, fmt.Sprintf("SELECT v FROM t WHERE %s = 0", pred)); c != "54001" {
		t.Fatalf("deep WHERE = %s, want 54001", c)
	}
	if c := codeOf(db, fmt.Sprintf("CREATE TABLE u (a i32 CHECK (%s > 0))", pred)); c != "54001" {
		t.Fatalf("deep CHECK = %s, want 54001", c)
	}
}
