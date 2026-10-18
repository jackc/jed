//go:build expression_vm

package jed

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

func expressionVMFixture(t *testing.T, name string) [][]string {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("..", "..", "spec", "experiments", "expression-vm", name))
	if err != nil {
		t.Fatal(err)
	}
	var rows [][]string
	for _, line := range strings.Split(strings.TrimSpace(string(data)), "\n") {
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		rows = append(rows, strings.Split(line, "\t"))
	}
	return rows
}

func TestExpressionVMDifferential(t *testing.T) {
	if !expressionVMEnabled {
		t.Skip("tree baseline excludes the expression VM hook")
	}
	expressionVMRun.Store(false)
	defer expressionVMRun.Store(false)
	resetExpressionVM()
	db := memDB()
	defer db.Close()
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t(id i32 PRIMARY KEY,a i32,b i32,p boolean,q boolean)")
	var rows []storedRow
	for _, cells := range expressionVMFixture(t, "rows.tsv") {
		row := make(storedRow, len(cells))
		for i, cell := range cells {
			switch cell {
			case "NULL":
				row[i] = NullValue()
			case "true":
				row[i] = BoolValue(true)
			case "false":
				row[i] = BoolValue(false)
			default:
				v, err := strconv.ParseInt(cell, 10, 64)
				if err != nil {
					t.Fatal(err)
				}
				row[i] = IntValue(v)
			}
		}
		rows = append(rows, row)
	}
	for _, cells := range expressionVMFixture(t, "cases.tsv") {
		t.Run(cells[0], func(t *testing.T) {
			stmt, err := s.engine.parse("SELECT " + cells[2] + " FROM t")
			if err != nil {
				t.Fatal(err)
			}
			plan, err := s.engine.planSelect(stmt.Select, nil, nil, &paramTypes{})
			if err != nil {
				t.Fatal(err)
			}
			e := plan.projections[0]
			filter := cells[1] == "filter"
			p := compileExpressionVM(e, filter)
			wantSupported := !strings.HasPrefix(cells[0], "fallback_")
			if p.supported != wantSupported {
				t.Fatalf("supported=%v want=%v", p.supported, wantSupported)
			}
			env := &evalEnv{exec: s.engine, params: []Value{IntValue(7), IntValue(3)}, rng: newStmtRng()}
			eval := func(row storedRow, m *costMeter) (Value, error) {
				if filter {
					keep, err := e.evalFilter(row, env, m)
					return BoolValue(keep), err
				}
				return e.eval(row, env, m)
			}
			for ri, row := range rows {
				compare := func(label string, treeMeter, vmMeter *costMeter) {
					t.Helper()
					expressionVMRun.Store(false)
					want, wantErr := eval(row, treeMeter)
					expressionVMRun.Store(true)
					got, gotErr := eval(row, vmMeter)
					if want.Kind != got.Kind || want.Render() != got.Render() || !reflect.DeepEqual(wantErr, gotErr) || treeMeter.Accrued != vmMeter.Accrued {
						t.Fatalf("row=%d %s tree=(%v,%v,cost=%d) vm=(%v,%v,cost=%d)", ri, label, want.Render(), wantErr, treeMeter.Accrued, got.Render(), gotErr, vmMeter.Accrued)
					}
				}
				expressionVMRun.Store(false)
				base := newMeter()
				guards := 0
				base.cancel = func() bool { guards++; return false }
				_, _ = eval(row, base)
				for limit := int64(0); limit <= base.Accrued+2; limit++ {
					treeMeter, vmMeter := newMeterWithLimit(limit), newMeterWithLimit(limit)
					compare(fmt.Sprintf("ceiling=%d", limit), treeMeter, vmMeter)
					treeLife, vmLife := int64(5), int64(5)
					treeMeter, vmMeter = newMeterWithLimit(limit+1), newMeterWithLimit(limit+1)
					treeMeter.lifetimeTotal, vmMeter.lifetimeTotal = &treeLife, &vmLife
					treeMeter.lifetimeLimit, vmMeter.lifetimeLimit = 5+limit, 5+limit
					compare(fmt.Sprintf("lifetime remaining=%d", limit), treeMeter, vmMeter)
					if treeLife != vmLife {
						t.Fatal("lifetime accrual diverged")
					}
				}
				for at := 1; at <= guards+1; at++ {
					treeMeter, vmMeter := newMeter(), newMeter()
					treePolls, vmPolls := 0, 0
					treeMeter.cancel = func() bool { treePolls++; return treePolls == at }
					vmMeter.cancel = func() bool { vmPolls++; return vmPolls == at }
					compare(fmt.Sprintf("cancel poll=%d", at), treeMeter, vmMeter)
					if treePolls != vmPolls {
						t.Fatalf("guard polling diverged: tree=%d vm=%d", treePolls, vmPolls)
					}
				}
			}
			// Immutable programs can be shared, while each execution owns its slots.
			// Exercise simultaneous cache misses as well as cache hits under -race.
			expressionVMRun.Store(false)
			want, wantErr := eval(rows[0], newMeter())
			resetExpressionVM()
			expressionVMRun.Store(true)
			var workers sync.WaitGroup
			for range 4 {
				workers.Go(func() {
					for range 16 {
						got, gotErr := eval(rows[0], newMeter())
						if got.Kind != want.Kind || got.Render() != want.Render() || !reflect.DeepEqual(gotErr, wantErr) {
							t.Errorf("concurrent VM result diverged")
						}
					}
				})
			}
			workers.Wait()
		})
	}
}

// This host-side test is the benchmark executable: the same engine binary supports
// both modes, setup/warmup excluded, every result consumed identically. It emits
// JSONL records alongside go test's normal PASS summary.
func TestExpressionVMBenchmark(t *testing.T) {
	mode := os.Getenv("JED_EXPR_VM_MODE")
	if mode == "" {
		t.Skip("set JED_EXPR_VM_MODE=tree or vm to run benchmark")
	}
	if mode != "tree" && mode != "vm" {
		t.Fatalf("invalid mode %q", mode)
	}
	expressionVMRun.Store(false)
	defer expressionVMRun.Store(false)
	storage := os.Getenv("JED_EXPR_VM_STORAGE")
	if storage == "" {
		storage = "memory"
	}
	options := CreateOptions{}
	if storage == "disk" {
		options.Path = filepath.Join(t.TempDir(), "expression-vm.jed")
	} else if storage != "memory" {
		t.Fatalf("invalid storage %q", storage)
	}
	db, err := CreateDatabase(options)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t(id i32 PRIMARY KEY,a i32,b i32,p boolean,q boolean)")
	mustExec(t, s, "BEGIN")
	for start := 1; start <= 20000; start += 500 {
		var b strings.Builder
		b.WriteString("INSERT INTO t VALUES ")
		for id := start; id < start+500; id++ {
			if id != start {
				b.WriteByte(',')
			}
			a, v := strconv.Itoa(id%97), strconv.Itoa(id%13+1)
			p, q := strconv.FormatBool(id%2 == 0), strconv.FormatBool(id%3 == 0)
			if id%17 == 0 {
				a = "NULL"
			}
			if id%19 == 0 {
				v = "NULL"
			}
			if id%23 == 0 {
				p = "NULL"
			}
			if id%29 == 0 {
				q = "NULL"
			}
			fmt.Fprintf(&b, "(%d,%s,%s,%s,%s)", id, a, v, p, q)
		}
		mustExec(t, s, b.String())
	}
	mustExec(t, s, "COMMIT")
	if storage == "disk" {
		s.Close()
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
		db, err = OpenDatabase(options.Path)
		if err != nil {
			t.Fatal(err)
		}
		defer db.Close()
		s = db.Session(SessionOptions{})
		defer s.Close()
	}
	for _, cells := range expressionVMFixture(t, "workloads.tsv") {
		if only := os.Getenv("JED_EXPR_VM_FILTER"); only != "" && !strings.Contains(cells[0], only) {
			continue
		}
		iterations, err := strconv.Atoi(cells[2])
		if err != nil {
			t.Fatal(err)
		}
		if n := os.Getenv("JED_EXPR_VM_ITERATIONS"); n != "" {
			iterations, err = strconv.Atoi(n)
			if err != nil || iterations < 1 {
				t.Fatal("invalid JED_EXPR_VM_ITERATIONS")
			}
		}
		expressionVMRun.Store(mode == "vm")
		resetExpressionVM()
		s.SetMaxCost(0)
		if cells[0] == "metered_scan" {
			s.SetMaxCost(1 << 60)
		}
		startPrepare := time.Now()
		var prepared *PreparedStatement
		if cells[1] == "prepared" {
			prepared, err = s.Prepare(cells[3])
			if err != nil {
				t.Fatal(err)
			}
		}
		prepareNS := time.Since(startPrepare).Nanoseconds()
		run := func(checksum bool) (uint64, int64, int64) {
			var rs *Rows
			var err error
			if prepared != nil {
				rs, err = s.queryStmt(prepared.ast, nil, &prepared.sc, &prepared.ic)
			} else {
				rs, err = s.queryValues(cells[3], nil)
			}
			if err != nil {
				t.Fatalf("%s: %v", cells[0], err)
			}
			hash := uint64(14695981039346656037)
			var consumed int64
			for rs.Next() {
				row := rs.Row()
				consumed += int64(1 + len(row))
				if checksum {
					for _, value := range row {
						for _, b := range []byte(value.Render()) {
							hash ^= uint64(b)
							hash *= 1099511628211
						}
						hash *= 1099511628211 // NUL cell separator
					}
					hash ^= 10
					hash *= 1099511628211
				}
			}
			if err := rs.Err(); err != nil {
				t.Fatalf("%s: %v", cells[0], err)
			}
			cost := rs.Cost()
			rs.Close()
			return hash, cost, consumed
		}
		startFirst := time.Now()
		_, wantCost, wantConsumed := run(false)
		firstNS := time.Since(startFirst).Nanoseconds()
		wantHash, _, _ := run(true)
		warmup := 3
		if strings.HasPrefix(cells[0], "point_") {
			warmup = min(100, iterations)
		}
		for range warmup {
			run(false)
		}
		runtime.GC()
		var before, after runtime.MemStats
		runtime.ReadMemStats(&before)
		start := time.Now()
		var cost int64
		var consumed int64
		for range iterations {
			_, cost, consumed = run(false)
			if consumed != wantConsumed || cost != wantCost {
				t.Fatal("unstable query result")
			}
		}
		elapsed := time.Since(start).Nanoseconds()
		runtime.ReadMemStats(&after)
		hash, _, _ := run(true)
		if hash != wantHash {
			t.Fatal("query checksum changed after timing")
		}
		var programBytes int64
		expressionVMCache.Range(func(_, v any) bool {
			p := v.(*expressionVMProgram)
			if p.supported {
				programBytes += int64(reflect.TypeFor[expressionVMProgram]().Size()) + int64(cap(p.code))*int64(reflect.TypeFor[expressionVMInstruction]().Size())
			}
			return true
		})
		record := map[string]any{"core": "go", "mode": mode, "storage": storage, "case": cells[0], "iterations": iterations, "elapsed_ns": elapsed, "checksum": fmt.Sprintf("%016x", hash), "consumed": consumed, "cost": cost, "compiled": expressionVMCompiled.Load(), "fallbacks": expressionVMFallbacks.Load(), "instructions": expressionVMInstructions.Load(), "program_bytes": programBytes, "prepare_ns": prepareNS, "first_execute_ns": firstNS, "allocations": after.Mallocs - before.Mallocs, "allocated_bytes": after.TotalAlloc - before.TotalAlloc}
		data, err := json.Marshal(record)
		if err != nil {
			t.Fatal(err)
		}
		fmt.Println(string(data))
	}
}

// Publishing the last result keeps compiler output observable and heap-allocated,
// as it is when stored in the experiment's program cache.
var expressionVMCompileSink *expressionVMProgram

// Compiler allocations run in a separate process from end-to-end measurements;
// probing a growing ad-hoc cache between lanes would perturb later controls.
func TestExpressionVMCompile(t *testing.T) {
	if !expressionVMEnabled || os.Getenv("JED_EXPR_VM_COMPILE") != "1" {
		t.Skip("set JED_EXPR_VM_COMPILE=1 in an expression_vm build")
	}
	expressionVMRun.Store(false)
	defer func() { expressionVMCompileSink = nil }()
	db := memDB()
	defer db.Close()
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE t(id i32 PRIMARY KEY,a i32,b i32,p boolean,q boolean)")
	for _, cells := range expressionVMFixture(t, "cases.tsv") {
		stmt, err := s.engine.parse("SELECT " + cells[2] + " FROM t")
		if err != nil {
			t.Fatal(err)
		}
		plan, err := s.engine.planSelect(stmt.Select, nil, nil, &paramTypes{})
		if err != nil {
			t.Fatal(err)
		}
		e, filter := plan.projections[0], cells[1] == "filter"
		p := compileExpressionVM(e, filter)
		if p.supported != !strings.HasPrefix(cells[0], "fallback_") {
			t.Fatalf("unexpected admission for %s", cells[0])
		}
		if !p.supported {
			continue
		}
		for range 100 {
			expressionVMCompileSink = compileExpressionVM(e, filter)
		}
		runtime.GC()
		var before, after runtime.MemStats
		runtime.ReadMemStats(&before)
		const iterations = 10_000
		start := time.Now()
		for range iterations {
			expressionVMCompileSink = compileExpressionVM(e, filter)
		}
		elapsed := time.Since(start).Nanoseconds()
		runtime.ReadMemStats(&after)
		programBytes := int64(reflect.TypeFor[expressionVMProgram]().Size()) + int64(cap(p.code))*int64(reflect.TypeFor[expressionVMInstruction]().Size())
		record := map[string]any{"core": "go", "phase": "compile", "case": cells[0], "iterations": iterations, "elapsed_ns": elapsed, "instructions": len(p.code), "program_bytes": programBytes, "allocations": after.Mallocs - before.Mallocs, "allocated_bytes": after.TotalAlloc - before.TotalAlloc}
		data, err := json.Marshal(record)
		if err != nil {
			t.Fatal(err)
		}
		fmt.Println(string(data))
	}
}
