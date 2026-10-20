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
		{"filter", "SELECT id FROM t WHERE v+1>2 ORDER BY id", 4},
		{"null", "SELECT v IS NULL FROM t ORDER BY id", 4},
		{"offset", "SELECT v+1 FROM t ORDER BY id LIMIT 1 OFFSET 2", 1},
		{"limit_zero", "SELECT v/0 FROM t ORDER BY id LIMIT 0", 0},
		{"case_fallback", "SELECT CASE WHEN v>0 THEN v ELSE v/0 END FROM t ORDER BY id", 0},
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
