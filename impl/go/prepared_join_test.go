package jed

import (
	"encoding/json"
	"os"
	"reflect"
	"strings"
	"testing"
)

// Reuse the investigation's SQL inventory: these white-box checks test cache reuse,
// which the SQL corpus cannot observe, against freshly planned results and costs.
func TestPreparedJoinReuse(t *testing.T) {
	var plan struct{ Workloads []struct{ Name, SQL string } }
	data, err := os.ReadFile("../../bench/prepared-join/plan.json")
	if err != nil {
		t.Fatal(err)
	}
	if err = json.Unmarshal(data, &plan); err != nil {
		t.Fatal(err)
	}
	s := memDB().Session(SessionOptions{})
	mustExec(t, s, "CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)")
	mustExec(t, s, "INSERT INTO items SELECT CAST(g AS i32), CAST(g%10 AS i32), CAST((g*37)%10000 AS i32), 'row'||CAST(g AS text) FROM generate_series(0,999) AS g")
	for _, w := range plan.Workloads {
		t.Run(w.Name, func(t *testing.T) {
			stmt, err := s.Prepare(w.SQL)
			if err != nil {
				t.Fatal(err)
			}
			var entry *scanCache
			for i := 0; i < 24; i++ {
				value := IntValue(int64(i * 17))
				got, cost := drainValues(t, s, stmt, &stmt.sc, value)
				fresh, freshCost := runValues(t, s, w.SQL, value)
				if !reflect.DeepEqual(got, fresh) || cost != freshCost {
					t.Fatalf("binding %d differs from fresh planning", i)
				}
				c := stmt.sc.p.Load()
				if c == nil {
					t.Fatal("not admitted")
				}
				if i == 0 {
					entry = c
					t.Logf("sensitive=%v nullRead=%v", c.sensitive, c.nullRead)
				}
				if c.sensitive || c.memo.len() != 0 || c != entry {
					t.Fatal("value-blind join replanned")
				}
			}
			got, cost := drainValues(t, s, stmt, &stmt.sc, NullValue())
			fresh, freshCost := runValues(t, s, w.SQL, NullValue())
			if !reflect.DeepEqual(got, fresh) || cost != freshCost || stmt.sc.p.Load() != entry {
				t.Fatal("NULL fallback changed results/cost/cache")
			}
		})
	}
	mustExec(t, s, "ANALYZE items")
	for _, w := range plan.Workloads {
		if w.Name != "indexed_join" && !strings.HasPrefix(w.Name, "join_text") && w.Name != "join_integer_outputs" {
			continue
		}
		stmt, err := s.Prepare(w.SQL)
		if err != nil {
			t.Fatal(err)
		}
		for _, v := range []Value{IntValue(17), IntValue(51), IntValue(999), IntValue(1000)} {
			got, cost := drainValues(t, s, stmt, &stmt.sc, v)
			fresh, freshCost := runValues(t, s, w.SQL, v)
			if !reflect.DeepEqual(got, fresh) || cost != freshCost || stmt.sc.p.Load().sensitive {
				t.Fatalf("analyzed %s differs or replans", w.Name)
			}
		}
	}
}

// Diagnostic-only workload: coverage -covermode=count measures actual planning/resolution calls
// without adding counters to the production hot path. Run separately from timing samples.
func TestPreparedJoinCounters(t *testing.T) {
	if os.Getenv("JOIN_COUNTERS") == "" {
		t.Skip("opt-in benchmark diagnostics")
	}
	var plan struct {
		Workloads []struct {
			Name, SQL                              string
			Iterations, Warmup, Offset, Step, Span int
		}
	}
	data, err := os.ReadFile("../../bench/prepared-join/plan.json")
	if err != nil {
		t.Fatal(err)
	}
	if err = json.Unmarshal(data, &plan); err != nil {
		t.Fatal(err)
	}
	s := memDB().Session(SessionOptions{})
	mustExec(t, s, "CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)")
	mustExec(t, s, "INSERT INTO items SELECT CAST(g AS i32), CAST(g%10 AS i32), CAST((g*37)%10000 AS i32), 'row'||CAST(g AS text) FROM generate_series(0,999) AS g")
	for _, w := range plan.Workloads {
		stmt, err := s.Prepare(w.SQL)
		if err != nil {
			t.Fatal(err)
		}
		value := func(i int) Value { return IntValue(int64(w.Offset + i*w.Step%w.Span)) }
		for i := 0; i < w.Warmup; i++ {
			drainValues(t, s, stmt, &stmt.sc, value(i))
		}
		hits, misses := 0, 0
		for i := 0; i < w.Iterations*5; i++ {
			v := value(i % w.Iterations)
			c := stmt.sc.p.Load()
			if c != nil && (!c.sensitive || c.memo.get(planMemoKey([]Value{v})) != nil) {
				hits++
			} else {
				misses++
			}
			drainValues(t, s, stmt, &stmt.sc, v)
		}
		c := stmt.sc.p.Load()
		t.Logf("%s sensitive=%v memo=%d hits=%d misses=%d", w.Name, c.sensitive, c.memo.len(), hits, misses)
	}
}

func TestRuntimeBoundDoesNotReadPlanParameter(t *testing.T) {
	for _, kind := range []rExprKind{reColumn, reOuterColumn} {
		frame := &planFrame{}
		parameter := &rExpr{kind: reParam, index: 0, plan: frame}
		runtime := &rExpr{kind: kind, index: 0}
		if estimatorBoundsContradict(opEq, parameter, opEq, runtime) || estimatorBoundsContradict(opEq, runtime, opEq, parameter) || frame.sensitive {
			t.Fatal("runtime bound read a parameter value")
		}
		other := &rExpr{kind: reParam, index: 1, plan: frame}
		estimatorBoundsContradict(opEq, parameter, opEq, other)
		if !frame.sensitive {
			t.Fatal("two parameters must remain sensitive")
		}
		frame.custom = true
		frame.values = []Value{IntValue(1), IntValue(2)}
		if !estimatorBoundsContradict(opEq, parameter, opEq, other) {
			t.Fatal("lost bound contradiction")
		}
	}
}
