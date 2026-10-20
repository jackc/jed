//go:build jed_specialization

package jed

import (
	"fmt"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"testing"
)

// SQL semantics live in the shared corpus. These tests cover the private
// admission, cost checkpoints, ownership and cache publication mechanisms.
func TestSpecializationBudgetPrefixes(t *testing.T) {
	cols := []catColumn{{Name: "a", Type: dataType{Scalar: scalarInt64}}, {Name: "b", Type: dataType{Scalar: scalarInt64}}}
	leaf := func(i int) *rExpr { return &rExpr{kind: reColumn, index: i} }
	arith := func(op binaryOp, l, r *rExpr) *rExpr {
		return &rExpr{kind: reArith, op: op, result: scalarInt64, lhs: l, rhs: r}
	}
	roots := []*rExpr{}
	for _, op := range []binaryOp{opAdd, opSub, opMul, opDiv, opMod} {
		roots = append(roots, arith(op, leaf(0), leaf(1)), arith(op, leaf(0), arith(opDiv, leaf(0), leaf(1))))
	}
	for _, op := range []binaryOp{opEq, opNe, opLt, opGt, opLe, opGe} {
		roots = append(roots, &rExpr{kind: reCompare, op: op, lhs: arith(opAdd, leaf(0), leaf(1)), rhs: leaf(0)})
	}
	roots = append(roots, &rExpr{kind: reIsNull, operand: leaf(0)}, &rExpr{kind: reIsNull, operand: leaf(0), negated: true})
	vals := []Value{NullValue(), IntValue(0), IntValue(1), IntValue(-1), IntValue(math.MaxInt64), IntValue(math.MinInt64)}
	for _, e := range roots {
		k := bindScalarKernel(e, cols, nil)
		if k == nil {
			t.Fatal("eligible root rejected")
		}
		for _, a := range vals {
			for _, b := range vals {
				for limit := int64(0); limit <= 7; limit++ {
					for lifeLimit := int64(0); lifeLimit <= 10; lifeLimit++ {
						life1, life2 := int64(3), int64(3)
						m1, m2 := newMeterWithLimit(limit), newMeterWithLimit(limit)
						m1.lifetimeTotal, m2.lifetimeTotal = &life1, &life2
						m1.lifetimeLimit, m2.lifetimeLimit = lifeLimit, lifeLimit
						row := storedRow{a, b}
						v1, err1 := e.eval(row, &evalEnv{}, m1)
						v2, err2 := k.eval(row, &evalEnv{}, m2)
						if !reflect.DeepEqual(v1, v2) || !reflect.DeepEqual(err1, err2) || m1.Accrued != m2.Accrued || life1 != life2 || m1.ceilingHit != m2.ceilingHit {
							t.Fatalf("kind=%v op=%v row=%v limits=%d/%d: ordinary %v %v %d; kernel %v %v %d", e.kind, e.op, row, limit, lifeLimit, v1, err1, m1.Accrued, v2, err2, m2.Accrued)
						}
					}
				}
			}
		}
	}
}

func TestSpecializationAdmissionAndCache(t *testing.T) {
	db := memDB()
	defer db.Close()
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32, d date, flag boolean)")
	mustExec(t, s, "INSERT INTO t VALUES (1, 7, 3, '2020-01-01', true), (2, NULL, 0, '2020-01-02', NULL)")
	tests := []struct {
		expr     string
		admitted bool
	}{
		{"a + b", true},
		{"(a + b) * (a - b)", true},
		{"a / b", true},
		{"a % b", true},
		{"a < b", true},
		{"a IS NULL", true},
		{"NOT flag", true},
		{"d - d", false},
		{"(d - d) + a", false},
		{"a::i16", false},
		{"CASE WHEN flag THEN a ELSE b END", false},
		{"coalesce(a,b)", false},
		{"flag AND flag", false},
		{"flag OR flag", false},
		{"abs(a)", false},
		{"((a+b)+b)+b", false},
	}
	for _, tt := range tests {
		rows, err := s.queryValues("SELECT "+tt.expr+" FROM t ORDER BY id", nil)
		if err != nil {
			t.Fatal(err)
		}
		c, ok := rows.cursor.(*streamingCursor)
		if !ok || c.plan.kernels == nil {
			t.Fatalf("%s: missing scan artifacts", tt.expr)
		}
		if got := c.plan.kernels.project[0] != nil; got != tt.admitted {
			t.Fatalf("%s admission=%v", tt.expr, got)
		}
		rows.Close() // Admission must not evaluate the zero divisor.
	}
	stmt, err := s.Prepare("SELECT a + $1 FROM t ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	r1, _ := drainQ(t, s, stmt, IntValue(2))
	entry := stmt.sc.p.Load()
	kernels := entry.sp.kernels
	r2, _ := drainQ(t, s, stmt, IntValue(4))
	if kernels == nil || stmt.sc.p.Load().sp.kernels != kernels || r1[0][0] != 9 || r2[0][0] != 11 {
		t.Fatal("descriptor reuse / binding isolation")
	}
	// Both live cursors keep their pinned artifact and independent call arguments.
	a, _ := s.queryStmt(stmt.ast, []Value{IntValue(10)}, &stmt.sc, &stmt.ic)
	b, _ := s.queryStmt(stmt.ast, []Value{IntValue(20)}, &stmt.sc, &stmt.ic)
	if !a.Next() || !b.Next() || a.Row()[0].Int != 17 || b.Row()[0].Int != 27 {
		t.Fatal("interleaved cursor bindings")
	}
	a.Close()
	b.Close()
	mustExec(t, s, "ALTER TABLE t ADD COLUMN extra i32")
	drainQ(t, s, stmt, IntValue(4))
	if stmt.sc.p.Load().sp.kernels == kernels {
		t.Fatal("stale artifact after DDL")
	}
	// A complete unsupported root caches its negative result too.
	bad, _ := s.Prepare("SELECT coalesce(a,b) FROM t ORDER BY id")
	drainQ(t, s, bad)
	rejection := bad.sc.p.Load().sp.kernels
	drainQ(t, s, bad)
	if rejection == nil || rejection.project[0] != nil || rejection != bad.sc.p.Load().sp.kernels {
		t.Fatal("rejection not reused")
	}
	// Fixed grammar is tighter than every shared expression cap. Deep/wide roots
	// are rejected without traversing their descendants or allocating their tree.
	for _, n := range []int{32, 192, 1536} {
		e := &rExpr{kind: reColumn, index: 1}
		for i := 0; i < n; i++ {
			e = &rExpr{kind: reArith, op: opAdd, result: scalarInt32, lhs: e, rhs: &rExpr{kind: reConstInt, cInt: 1}}
		}
		if bindScalarKernel(e, []catColumn{{Type: dataType{Scalar: scalarInt32}}, {Type: dataType{Scalar: scalarInt32}}}, nil) != nil {
			t.Fatalf("deep input %d admitted", n)
		}
	}
	for _, n := range []int{7, 8, 9} {
		rows, err := s.queryValues("SELECT "+strings.TrimSuffix(strings.Repeat("a+1,", n), ",")+" FROM t ORDER BY id", nil)
		if err != nil {
			t.Fatal(err)
		}
		c := rows.cursor.(*streamingCursor)
		if (c.plan.kernels != nil) != (n <= 7) {
			t.Fatal("plan root cap")
		}
		rows.Close()
	}
}

func TestSpecializationSharedStatement(t *testing.T) {
	db := memDB()
	defer db.Close()
	seed := db.Session(SessionOptions{})
	mustExec(t, seed, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)")
	mustExec(t, seed, "INSERT INTO t VALUES (1,7)")
	stmt, err := seed.Prepare("SELECT a + $1 FROM t ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	seed.Close()
	var wg sync.WaitGroup
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			s := db.Session(SessionOptions{})
			defer s.Close()
			for j := 0; j < 30; j++ {
				rows, err := s.queryStmt(stmt.ast, []Value{IntValue(int64(i + j))}, &stmt.sc, &stmt.ic)
				if err != nil {
					t.Error(err)
					return
				}
				if !rows.Next() || rows.Row()[0].Int != int64(7+i+j) {
					t.Error("shared binding")
				}
				rows.Close()
			}
		}(i)
	}
	wg.Wait()
}

func TestSpecializationCursorBudgetAndCancellation(t *testing.T) {
	s := rowTable(t, 4)
	defer s.Close()
	for _, sql := range []string{
		"SELECT (v+1)*(v-1) FROM t ORDER BY id",
		"SELECT v/0 FROM t ORDER BY id LIMIT 0",
		"SELECT v/(v-1) FROM t ORDER BY id OFFSET 1",
		"SELECT v+1 FROM t WHERE v+1>2 ORDER BY id LIMIT 1 OFFSET 1",
	} {
		for limit := int64(0); limit < 35; limit++ {
			for _, mem := range []int64{0, 1, 64, 256} {
				run := func(ordinary bool) ([]Value, string, int64, int64) {
					s.SetMaxQueryMemoryBytes(mem)
					rows, err := s.queryValues(sql, nil)
					if err != nil {
						t.Fatal(err)
					}
					c := rows.cursor.(*streamingCursor)
					if ordinary {
						cp := *c.plan
						cp.kernels = nil
						c.plan = &cp
					}
					c.meter.Limit = limit
					c.meter.query.limit = mem
					var vals []Value
					for rows.Next() {
						vals = append(vals, rows.Row()...)
					}
					resultErr := fmt.Sprint(rows.Err())
					cost := rows.Cost()
					rows.Close()
					return vals, resultErr, cost, specializationBalance(c.meter.query)
				}
				a, ae, ac, am := run(true)
				b, be, bc, bm := run(false)
				if !reflect.DeepEqual(a, b) || ae != be || ac != bc || am != bm {
					t.Fatalf("%s limit=%d mem=%d: %v/%v %s/%s %d/%d %d/%d", sql, limit, mem, a, b, ae, be, ac, bc, am, bm)
				}
			}
		}
	}
	// Expression kernels must not introduce a cancellation poll. Cancellation is
	// sampled by the original row Step and Rows boundary.
	for _, ordinary := range []bool{true, false} {
		rows, err := s.queryValues("SELECT v+1 FROM t ORDER BY id", nil)
		if err != nil {
			t.Fatal(err)
		}
		c := rows.cursor.(*streamingCursor)
		if ordinary {
			cp := *c.plan
			cp.kernels = nil
			c.plan = &cp
		}
		done := make(chan struct{})
		close(done)
		c.meter.cancel = done
		_, _, err = c.nextRow()
		if cancelCode(t, err) != "57014" {
			t.Fatal(err)
		}
		rows.Close()
	}
}

func specializationBalance(a queryAccount) int64 {
	if a.used == nil {
		return 0
	}
	return *a.used
}

// This uses real file pages and an empty buffer pool, not a synthetic Value row.
// The second physical read can fail or cancel inside a single pull. Compare the
// complete read-offset trace, emitted prefix and cost with the ordinary cursor.
type specializationReadStore struct {
	blockStore
	offsets []int64
	done    chan struct{}
	fail    bool
}

func (s *specializationReadStore) readAt(off int64, n int) ([]byte, error) {
	s.offsets = append(s.offsets, off)
	if len(s.offsets) == 2 {
		if s.fail {
			return nil, newError(IoError, "specialization injected read failure")
		}
		close(s.done)
	}
	return s.blockStore.readAt(off, n)
}

func TestSpecializationFileFaultOrder(t *testing.T) {
	path := filepath.Join(t.TempDir(), "faults.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true, PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	mustExec(t, s, "INSERT INTO t SELECT i, i%7 FROM generate_series(1,600) AS g(i)")
	s.Close()
	db.Close()
	for _, fail := range []bool{true, false} {
		type result struct {
			values  []int64
			offsets []int64
			cost    int64
			code    string
		}
		run := func(ordinary bool, limit string) result {
			db, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, CacheBytes: 512})
			if err != nil {
				t.Fatal(err)
			}
			defer db.Close()
			s := db.Session(SessionOptions{})
			defer s.Close()
			done := make(chan struct{})
			if !fail {
				s.engine.session.cancel = done
			}
			rows, err := s.queryValues("SELECT (v+1)*(v-1) FROM t ORDER BY id"+limit, nil)
			if err != nil {
				t.Fatal(err)
			}
			c := rows.cursor.(*streamingCursor)
			if c.plan.kernels == nil || c.plan.kernels.project[0] == nil {
				t.Fatal("no file scan kernel")
			}
			if ordinary {
				cp := *c.plan
				cp.kernels = nil
				c.plan = &cp
			}
			paging := db.core.storage.paging
			paging.pool = newBufferPool(2)
			store := &specializationReadStore{blockStore: paging.pgr.store, done: done, fail: fail}
			paging.pgr.store = store
			var r result
			for rows.Next() {
				r.values = append(r.values, rows.Row()[0].Int)
			}
			r.cost = rows.Cost()
			r.code = fmt.Sprint(rows.Err())
			rows.Close()
			r.offsets = store.offsets
			paging.pgr.store = store.blockStore
			if rows.Next() {
				t.Fatal("closed cursor yielded a row")
			}
			return r
		}
		for _, limit := range []string{"", " LIMIT 0", " LIMIT 1"} {
			a, b := run(true, limit), run(false, limit)
			if !reflect.DeepEqual(a, b) {
				t.Fatalf("fault=%v %s: ordinary=%v kernel=%v", fail, limit, a, b)
			}
			if limit == "" && (len(b.offsets) < 2 || b.code == "<nil>") {
				t.Fatal("fault injection missed")
			}
			if limit == " LIMIT 0" && len(b.offsets) != 0 {
				t.Fatal("LIMIT 0 read a page")
			}
		}
	}
}

func TestSpecializationStructuralBounds(t *testing.T) {
	data, err := os.ReadFile(specPath(t, "experiments/execution-specialization/limits.toml"))
	if err != nil {
		t.Fatal(err)
	}
	limits := map[string]int{}
	for _, line := range strings.Split(string(data), "\n") {
		key, value, ok := strings.Cut(line, "=")
		if !ok {
			continue
		}
		n, err := strconv.Atoi(strings.TrimSpace(value))
		if err == nil {
			limits[strings.TrimSpace(key)] = n
		}
	}
	for key, used := range map[string]int{"max_nodes": 7, "max_depth": 3, "max_lowered_units": 7, "max_pool_entries": 0, "max_scratch_slots": 0, "max_artifacts": specializationMaxRoots} {
		if limit, ok := limits[key]; !ok || used > limit {
			t.Fatalf("%s: representation uses %d exceeds shared limit %d", key, used, limit)
		}
	}
	t.Logf("retained descriptor bytes: root=%d plan=%d (plus existing source tree); pool=0 scratch=0", reflect.TypeFor[scalarKernel]().Size(), reflect.TypeFor[scanKernels]().Size())
	// No transient allocation on the per-row path, including three checked ops.
	e := &rExpr{kind: reArith, op: opAdd, result: scalarInt32, lhs: &rExpr{kind: reConstInt, cInt: 1}, rhs: &rExpr{kind: reConstInt, cInt: 2}}
	k := bindScalarKernel(e, nil, nil)
	m := newMeter()
	env := &evalEnv{}
	if got := testing.AllocsPerRun(100, func() { _, _ = k.eval(nil, env, m) }); got != 0 {
		t.Fatalf("kernel allocates %g per row", got)
	}
}
