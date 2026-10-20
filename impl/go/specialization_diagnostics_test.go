//go:build jed_specialization && jed_specialization_diagnostics

package jed

import (
	"runtime"
	"testing"
	"weak"
)

func TestSpecializationInvocationCoverage(t *testing.T) {
	s := rowTable(t, 4)
	defer s.Close()
	for _, tt := range []struct {
		name, sql string
		calls     uint64
	}{
		{"arithmetic", "SELECT (v+1)*(v-1) FROM t ORDER BY id", 4},
		{"filter", "SELECT id FROM t WHERE v+1>2 ORDER BY id", 7},
		{"boolean", "SELECT v>0 AND (v<3 OR v IS NULL) FROM t ORDER BY id", 4},
		{"coalesce", "SELECT coalesce(v,v/0) FROM t ORDER BY id", 4},
		{"fallback", "SELECT coalesce(v,0,v/0) FROM t ORDER BY id", 0},
		{"null", "SELECT v IS NULL FROM t ORDER BY id", 4},
		{"offset", "SELECT v+1 FROM t ORDER BY id LIMIT 1 OFFSET 2", 1},
		{"limit_zero", "SELECT v/0 FROM t ORDER BY id LIMIT 0", 0},
		{"case", "SELECT CASE WHEN v>0 THEN v ELSE v/0 END FROM t ORDER BY id", 4},
	} {
		before := specializationInvocations.Load()
		if _, err := queryOutcome(s, tt.sql, nil); err != nil {
			t.Fatal(err)
		}
		got := specializationInvocations.Load() - before
		want := tt.calls
		if specializationMode == "ordinary" {
			want = 0
		}
		if got != want {
			t.Fatalf("%s invocations=%d want=%d", tt.name, got, want)
		}
		t.Logf("%s: %d optimized root invocations", tt.name, got)
	}
}

// Weak references inspect GC ownership without a finalizer/global cache or
// asserting a noisy process heap size. The statement pins only its current
// artifact; an active cursor independently pins its replaced artifact.
func TestSpecializationReplacedPlanLifetime(t *testing.T) {
	db := memDB()
	defer db.Close()
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	mustExec(t, s, "INSERT INTO t VALUES (1,7)")
	stmt, err := s.Prepare("SELECT v+1 FROM t ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	replace := func() weak.Pointer[scanKernels] {
		rows, err := s.queryStmt(stmt.ast, nil, &stmt.sc, &stmt.ic)
		if err != nil {
			t.Fatal(err)
		}
		old := weak.Make(stmt.sc.p.Load().sp.kernels)
		mustExec(t, s, "ALTER TABLE t ADD COLUMN added i32")
		drainQ(t, s, stmt)
		runtime.GC()
		if old.Value() == nil {
			t.Fatal("active cursor lost its artifact")
		}
		if !rows.Next() || rows.Row()[0].Int != 8 {
			t.Fatal("replaced cursor failed")
		}
		rows.Close()
		return old
	}
	old := replace()
	runtime.GC()
	if old.Value() != nil {
		t.Fatal("replaced artifact survived its last owner")
	}
	runtime.KeepAlive(stmt)
}

func TestSpecializationClosureGraphReleased(t *testing.T) {
	build := func() (kernelFunc, weak.Pointer[scalarKernel]) {
		e := &rExpr{kind: reCoalesce, sargs: []*rExpr{{kind: reConstNull}, {kind: reConstInt, cInt: 7}}}
		k, _ := bindComposedKernel(e, nil, nil)
		return closeKernel(k), weak.Make(k)
	}
	run, old := build()
	runtime.GC()
	if old.Value() != nil {
		t.Fatal("closure retained static graph")
	}
	v, err := run(nil, &evalEnv{}, newMeter(), false)
	if err != nil || v.Int != 7 {
		t.Fatal(v, err)
	}
	runtime.KeepAlive(run)
}
