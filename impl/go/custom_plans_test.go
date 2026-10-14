package jed

// Parameter-sensitive ("custom") plans (spec/design/estimator.md §6.1). That a bound statement plans,
// estimates, and costs exactly like its literal twin is pinned for every core by the shared corpus
// (suites/params/custom_plans.test). These white-box tests pin the plan cache: a value-blind statement
// keeps one cached plan, a sensitive one re-plans as its value moves between statistics classes, a
// memo hit equals fresh planning, and the cache is safe under concurrent sessions.

import (
	"reflect"
	"sync"
	"testing"
)

// runValues runs sql ad hoc (no statement cache) with params, fully drained: (rows, final cost).
func runValues(t *testing.T, s *Session, sql string, params ...Value) ([][]Value, int64) {
	t.Helper()
	stmt, err := s.Prepare(sql)
	if err != nil {
		t.Fatalf("prepare %s: %v", sql, err)
	}
	return drainValues(t, s, stmt, nil, params...)
}

// drainValues runs a prepared statement (through its cache when sc is non-nil), fully drained.
func drainValues(t *testing.T, s *Session, stmt *PreparedStatement, sc *stmtCache, params ...Value) ([][]Value, int64) {
	t.Helper()
	rows, err := s.queryStmt(stmt.ast, params, sc, nil)
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	defer rows.Close()
	var out [][]Value
	for rows.Next() {
		out = append(out, append([]Value(nil), rows.Row()...))
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("drain: %v", err)
	}
	return out, rows.Cost()
}

// seedItems: 1000 items, cat is 1 for nine rows in ten (an MCV) and unique otherwise; tags holds the
// common tag 'a' on every row and the rare tag 'r<i>' on one. Both are indexed and analyzed.
func seedItems(t *testing.T, s *Session) {
	t.Helper()
	mustExec(t, s, "CREATE TABLE items (id i32 PRIMARY KEY, cat i32, tags text[])")
	mustExec(t, s, "INSERT INTO items SELECT CAST(g AS i32), "+
		"CAST(CASE WHEN g % 10 = 0 THEN g ELSE 1 END AS i32), "+
		"ARRAY['a', 'r' || CAST(g AS text)] FROM generate_series(1, 1000) AS g")
	mustExec(t, s, "CREATE INDEX items_cat ON items (cat)")
	mustExec(t, s, "CREATE INDEX items_tags ON items USING gin (tags)")
	mustExec(t, s, "ANALYZE items")
}

func TestCustomPlanKeyLookupPlannedOnce(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	seedItems(t, s)
	stmt, err := s.Prepare("SELECT cat FROM items WHERE id = $1")
	if err != nil {
		t.Fatal(err)
	}
	_, c1 := drainValues(t, s, stmt, &stmt.sc, IntValue(5))
	entry := stmt.sc.p.Load()
	if entry == nil || entry.sensitive {
		t.Fatal("a full-key equality is value-blind and cached")
	}
	r2, c2 := drainValues(t, s, stmt, &stmt.sc, IntValue(900))
	if len(r2) != 1 || r2[0][0].Int != 900 || c2 != c1 {
		t.Fatalf("second lookup: %v cost %d (first %d)", r2, c2, c1)
	}
	if stmt.sc.p.Load() != entry {
		t.Fatal("the value-blind plan is reused")
	}
	// A NULL binding takes its own custom plan (a known NULL proves no row) without disturbing the
	// cached one.
	r3, _ := drainValues(t, s, stmt, &stmt.sc, NullValue())
	if len(r3) != 0 || stmt.sc.p.Load() != entry {
		t.Fatalf("NULL lookup: %v", r3)
	}
}

func TestCustomPlanSensitiveReplansAndMemoizes(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	seedItems(t, s)
	stmt, err := s.Prepare("SELECT id FROM items WHERE cat = $1")
	if err != nil {
		t.Fatal(err)
	}
	commonRows, commonCost := drainValues(t, s, stmt, &stmt.sc, IntValue(1))
	entry := stmt.sc.p.Load()
	if len(commonRows) != 900 || entry == nil || !entry.sensitive || entry.memo.len() != 1 {
		t.Fatalf("common: %d rows, entry %+v", len(commonRows), entry)
	}
	rareRows, rareCost := drainValues(t, s, stmt, &stmt.sc, IntValue(70))
	if len(rareRows) != 1 || entry.memo.len() != 2 {
		t.Fatalf("rare: %v, memo %d", rareRows, entry.memo.len())
	}
	if _, c := runValues(t, s, "SELECT id FROM items WHERE cat = 1"); c != commonCost {
		t.Fatalf("common cost %d, literal %d", commonCost, c)
	}
	if _, c := runValues(t, s, "SELECT id FROM items WHERE cat = 70"); c != rareCost {
		t.Fatalf("rare cost %d, literal %d", rareCost, c)
	}
	// A memo hit equals fresh planning.
	againRows, againCost := drainValues(t, s, stmt, &stmt.sc, IntValue(70))
	if !reflect.DeepEqual(againRows, rareRows) || againCost != rareCost || entry.memo.len() != 2 {
		t.Fatal("a memo hit equals fresh planning and adds nothing")
	}
	for v := int64(0); v < 20; v++ {
		drainValues(t, s, stmt, &stmt.sc, IntValue(1000+v))
	}
	if entry.memo.len() > planMemoCapacity {
		t.Fatal("the memo is bounded")
	}
	// A relevant estimator-input change replaces the entry and its memo.
	mustExec(t, s, "INSERT INTO items VALUES (5000, 70, ARRAY['a'])")
	after, _ := drainValues(t, s, stmt, &stmt.sc, IntValue(70))
	if len(after) != 2 || stmt.sc.p.Load() == entry || stmt.sc.p.Load().memo.len() != 1 {
		t.Fatal("the refreshed entry starts a new memo")
	}
}

// The classification's soundness (estimator.md §6.1, "never the reverse"): a statement classified
// value-blind has one plan for every non-NULL binding, so its unbound EXPLAIN equals its EXPLAIN with
// values, estimates included.
func TestCustomPlanValueBlindClassificationIsSound(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	seedItems(t, s)
	mustExec(t, s, "CREATE TABLE t2 (id i32 PRIMARY KEY, v i32, w text)")
	mustExec(t, s, "INSERT INTO t2 SELECT CAST(g AS i32), CAST(g % 7 AS i32), CAST(g AS text) FROM generate_series(1, 50) AS g")
	mustExec(t, s, "CREATE INDEX t2_v ON t2 (v)")
	yes, no := true, false
	cases := []struct {
		sql    string
		params []Value
		expect *bool
	}{
		{"SELECT cat FROM items WHERE id = $1", []Value{IntValue(5)}, &no},
		{"SELECT id FROM items WHERE cat = $1", []Value{IntValue(70)}, &yes},
		{"SELECT id FROM items WHERE id > $1", []Value{IntValue(990)}, &yes},
		{"SELECT count(*) FROM items i JOIN items j ON i.id = j.cat WHERE i.id = $1", []Value{IntValue(70)}, nil},
		{"SELECT * FROM t2 WHERE v = $1", []Value{IntValue(3)}, &no},
		{"SELECT * FROM t2 WHERE v > $1", []Value{IntValue(3)}, &no},
		{"SELECT * FROM t2 WHERE v > $1 AND v < $2", []Value{IntValue(5), IntValue(1)}, &yes},
		{"SELECT * FROM t2 WHERE w = $1 OR v = $2", []Value{TextValue("4"), IntValue(2)}, nil},
		{"SELECT * FROM t2 WHERE id = $1 AND v = $2", []Value{IntValue(4), IntValue(4)}, nil},
		{"SELECT * FROM t2 WHERE v = $1 OR v = $2", []Value{IntValue(4), IntValue(4)}, nil},
		{"SELECT CAST($1 AS i32) + id FROM t2 WHERE id = 3", []Value{IntValue(9)}, &no},
		{"SELECT * FROM t2 WHERE v = 2 AND $1 = TRUE", []Value{BoolValue(false)}, &yes},
		{"SELECT * FROM t2 WHERE v BETWEEN $1 AND $2", []Value{IntValue(2), IntValue(3)}, nil},
	}
	for _, c := range cases {
		stmt, err := s.Prepare(c.sql)
		if err != nil {
			t.Fatal(err)
		}
		drainValues(t, s, stmt, &stmt.sc, c.params...)
		entry := stmt.sc.p.Load()
		if entry == nil {
			t.Fatalf("%s: no cache entry", c.sql)
		}
		if c.expect != nil && entry.sensitive != *c.expect {
			t.Fatalf("classification of %s: %v", c.sql, entry.sensitive)
		}
		if !entry.sensitive {
			unbound, _ := runValues(t, s, "EXPLAIN "+c.sql)
			bound, _ := runValues(t, s, "EXPLAIN "+c.sql, c.params...)
			if !reflect.DeepEqual(unbound, bound) {
				t.Fatalf("%s is classified value-blind:\n%v\n%v", c.sql, unbound, bound)
			}
		}
	}
}

// A parameter-sensitive statement shared by goroutines on separate sessions: each execution gets its
// own custom plan's rows while the shared memo fills concurrently (run under -race).
func TestCustomPlanSensitiveConcurrent(t *testing.T) {
	t.Parallel()
	db := memDB()
	s := db.Session(SessionOptions{})
	seedItems(t, s)
	stmt, err := s.Prepare("SELECT id FROM items WHERE cat = $1")
	if err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			sess := db.Session(SessionOptions{})
			for i := 0; i < 40; i++ {
				v, want := int64(1), 900
				if (g+i)%2 == 0 {
					v, want = int64(10*(1+(g+i)%12)), 1
				}
				rows, err := sess.queryStmt(stmt.ast, []Value{IntValue(v)}, &stmt.sc, nil)
				if err != nil {
					t.Error(err)
					return
				}
				n := 0
				for rows.Next() {
					n++
				}
				if err := rows.Err(); err != nil || n != want {
					t.Errorf("cat = %d: %d rows (%v), want %d", v, n, err, want)
				}
				rows.Close()
			}
		}(g)
	}
	wg.Wait()
}
