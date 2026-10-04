package jed

import (
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestBlockingSpillMatchesResidentAndCleansUp(t *testing.T) {
	dir := t.TempDir()
	db, err := create(filepath.Join(dir, "blocking.jed"), databaseOptions{PageSize: DefaultPageSize, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	db.spillDir = dir
	mustExec(t, db, "CREATE TABLE a(id i32 PRIMARY KEY, k i32, v decimal, f f64)")
	mustExec(t, db, "CREATE TABLE b(id i32 PRIMARY KEY, k i32)")
	for i := 0; i < 80; i++ {
		k := fmt.Sprint(i % 9)
		if i%13 == 0 {
			k = "NULL"
		}
		mustExec(t, db, fmt.Sprintf("INSERT INTO a VALUES (%d,%s,%d.50,%d.25)", i, k, i, i))
		mustExec(t, db, fmt.Sprintf("INSERT INTO b VALUES (%d,%s)", i, k))
	}
	queries := []string{
		"SELECT a.id,b.id FROM a JOIN b ON a.k=b.k",
		"SELECT a.id,b.id FROM a LEFT JOIN b ON a.k=b.k ORDER BY a.id,b.id LIMIT 31 OFFSET 4",
		"SELECT count(*), sum(a.v), avg(a.f) FROM a JOIN b ON a.k=b.k",
		"SELECT k,count(*),sum(v),avg(v),sum(f),min(v),max(v) FROM a GROUP BY k",
		"SELECT k,sum(DISTINCT v),count(DISTINCT k) FROM a GROUP BY k ORDER BY sum(v)",
		"SELECT k,count(*) FROM a GROUP BY GROUPING SETS ((k),()) HAVING count(*)>0",
		"SELECT k+1,count(*) FROM a GROUP BY k+1 ORDER BY k+1",
		"SELECT DISTINCT k FROM a",
		"SELECT DISTINCT id FROM a ORDER BY id LIMIT 1",
		"SELECT DISTINCT k FROM a ORDER BY k DESC LIMIT 5 OFFSET 2",
		"SELECT DISTINCT count(*) FROM a GROUP BY k",
		"SELECT count(*),sum(v) FROM a WHERE id<0",
	}
	for _, q := range queries {
		t.Run(q, func(t *testing.T) {
			db.SetWorkMem(0)
			want, wc := runQuery(t, db, q)
			for _, budget := range []int{1 << 20, 128} {
				db.SetWorkMem(budget)
				got, gc := runQuery(t, db, q)
				if !reflect.DeepEqual(got, want) {
					t.Fatalf("budget %d rows differ\ngot %v\nwant %v", budget, got, want)
				}
				if gc != wc {
					t.Fatalf("budget %d cost got %d want %d", budget, gc, wc)
				}
				assertNoBlockingScratch(t, dir)
			}
		})
	}
}

func assertNoBlockingScratch(t *testing.T, dir string) {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range entries {
		if strings.HasPrefix(e.Name(), "jed-spill-") {
			t.Fatalf("scratch leaked: %s", e.Name())
		}
	}
}

func TestBlockingSpoolAndHashStateStayBounded(t *testing.T) {
	dir := t.TempDir()
	db := &engine{spillDir: dir, session: newSession()}
	db.session.workMem = 128
	spool := newRowSpool(db)
	defer spool.close()
	state := newBoundedMap(db)
	defer state.close()
	for i := 0; i < 500; i++ {
		row := storedRow{IntValue(int64(i)), TextValue(strings.Repeat("x", 40))}
		if err := spool.push(row); err != nil {
			t.Fatal(err)
		}
		if err := state.put(fmt.Sprint(i), row); err != nil {
			t.Fatal(err)
		}
		if spool.bytes > 128 || state.bytes > 128 {
			t.Fatal("resident state grew beyond budget")
		}
	}
	if spool.file == nil || state.disk == nil {
		t.Fatal("did not spill")
	}
	if len(spool.rows) != 0 || len(state.entries) != 0 {
		t.Fatal("spilled owners retained resident input")
	}
	for i := 0; i < 500; i++ {
		r, ok, err := state.get(fmt.Sprint(i))
		if err != nil || !ok || r[0].Int != int64(i) {
			t.Fatalf("state %d: %v %v %v", i, r, ok, err)
		}
	}
}

func TestBlockingSpillScratchFailure(t *testing.T) {
	dir := t.TempDir()
	db, err := create(filepath.Join(dir, "blocking.jed"), databaseOptions{PageSize: DefaultPageSize, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE a(id i32 PRIMARY KEY, k i32)")
	mustExec(t, db, "INSERT INTO a VALUES (1,1),(2,1),(3,2)")
	db.SetWorkMem(1)
	db.spillDir = filepath.Join(dir, "missing")
	_, err = queryOutcome(db, "SELECT k,count(*) FROM a GROUP BY k", nil)
	if err == nil || !strings.Contains(err.Error(), "58030") {
		t.Fatalf("scratch failure: %v", err)
	}
}

func TestBlockingVariableAggregatesSpill(t *testing.T) {
	dir := t.TempDir()
	db := &engine{spillDir: dir, session: newSession()}
	db.session.workMem = 128
	for _, plan := range []aggPlan{planPercentileDisc, planMode, planJsonbAgg, planJsonbObjectAgg} {
		t.Run(fmt.Sprint(plan), func(t *testing.T) {
			values := &boundedJoinTable{mem: make(map[uint64][]hashJoinEntry), budget: 128, dir: dir}
			defer values.close()
			seen := newBoundedMap(db)
			defer seen.close()
			spec := aggSpec{plan: plan}
			a := newAccFromSpec(spec)
			key := []byte("one group")
			for i := 0; i < 300; i++ {
				v := IntValue(int64(i))
				if plan == planJsonbObjectAgg {
					v = CompositeValue([]Value{TextValue(fmt.Sprint(i % 17)), v})
				}
				handled, err := blockingFoldCollection(a, spec, v, key, values, seen, &costMeter{})
				if err != nil || !handled {
					t.Fatalf("fold: %v %v", handled, err)
				}
			}
			if values.disk == nil || len(values.mem) != 0 {
				t.Fatal("one group retained its growing collection in memory")
			}
			fraction := Float64Value(.5)
			a.osaFrac = &fraction
			v, err := db.blockingFinalizeCollection(a, spec, key, values, seen, nil, &evalEnv{exec: db})
			if err != nil {
				t.Fatal(err)
			}
			if plan == planPercentileDisc && v.Int != 149 {
				t.Fatalf("percentile: %v", v)
			}
			if plan == planMode && v.Int != 0 {
				t.Fatalf("stable mode: %v", v)
			}
			values.close()
			seen.close()
			assertNoBlockingScratch(t, dir)
		})
	}
}

func TestBlockingSpillFailuresAndEarlyCursorClose(t *testing.T) {
	dir := t.TempDir()
	db, err := CreateDatabase(CreateOptions{Path: filepath.Join(dir, "cleanup.jed"), SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	db.setSpillDirForTest(dir)
	s := db.Session(SessionOptions{})
	defer s.Close()
	mustExec(t, s, "CREATE TABLE a(id i32 PRIMARY KEY,k i32)")
	for i := 0; i < 100; i++ {
		mustExec(t, s, fmt.Sprintf("INSERT INTO a VALUES (%d,%d)", i, i%7))
	}
	s.SetWorkMem(128)
	for _, q := range []string{"SELECT DISTINCT id FROM a", "SELECT id,percentile_cont(.5) WITHIN GROUP (ORDER BY k) FROM a GROUP BY id", "SELECT id,rank(3) WITHIN GROUP (ORDER BY k) FROM a GROUP BY id", "SELECT id,jsonb_agg(k) FROM a GROUP BY id"} {
		rows, err := s.queryValues(q, nil)
		if err != nil {
			t.Fatal(err)
		}
		if !rows.Next() {
			t.Fatalf("first row: %v", rows.Err())
		}
		files, err := filepath.Glob(filepath.Join(dir, "jed-spill-*"))
		if err != nil || len(files) == 0 {
			t.Fatalf("lazy result did not retain its spilling output: %v", err)
		}
		_ = rows.Close()
		assertNoBlockingScratch(t, dir)
	}
	// The PK-ordered DISTINCT cursor short-circuits correctly and independently
	// spills its growing membership set while the host pulls a prefix.
	ordered, err := s.queryValues("SELECT DISTINCT id FROM a ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 10; i++ {
		if !ordered.Next() {
			t.Fatalf("ordered DISTINCT prefix: %v", ordered.Err())
		}
	}
	files, err := filepath.Glob(filepath.Join(dir, "jed-spill-*"))
	if err != nil || len(files) == 0 {
		t.Fatal("streaming DISTINCT membership did not spill")
	}
	_ = ordered.Close()
	assertNoBlockingScratch(t, dir)
	for _, q := range []string{"SELECT DISTINCT id/(id-50) FROM a", "SELECT percentile_disc(2) WITHIN GROUP (ORDER BY k) FROM a", "SELECT jsonb_object_agg_unique(k,id) FROM a"} {
		_, err := queryOutcome(s, q, nil)
		if err == nil {
			t.Fatalf("expected evaluator error: %s", q)
		}
		assertNoBlockingScratch(t, dir)
	}
	limited := db.Session(SessionOptions{MaxCost: 50})
	defer limited.Close()
	limited.SetWorkMem(128)
	_, err = queryOutcome(limited, "SELECT DISTINCT id FROM a", nil)
	if err == nil || !strings.Contains(err.Error(), "54P01") {
		t.Fatalf("cost abort: %v", err)
	}
	assertNoBlockingScratch(t, dir)
}

func TestBlockingSortCompactsRunsAndSpillCollisionOrder(t *testing.T) {
	dir := t.TempDir()
	s := newSorter([]orderSlot{{idx: 0}}, 1, dir)
	defer s.close()
	for i := 0; i < 1000; i++ {
		if err := s.push(storedRow{IntValue(int64(i % 7)), IntValue(int64(i))}); err != nil {
			t.Fatal(err)
		}
		if len(s.runs) > 64 {
			t.Fatal("run metadata grew past fixed fan-in")
		}
	}
	rows, err := s.finish()
	if err != nil {
		t.Fatal(err)
	}
	defer rows.close()
	var previous int64 = -1
	key := int64(-1)
	for {
		r, ok, err := rows.next()
		if err != nil {
			t.Fatal(err)
		}
		if !ok {
			break
		}
		if r[0].Int == key && r[1].Int < previous {
			t.Fatal("compaction broke stable ties")
		}
		key, previous = r[0].Int, r[1].Int
	}
	d, err := newDiskBuckets(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer d.close()
	const collision uint64 = 9
	for i := 0; i < 30; i++ {
		if err := d.append(collision, []byte(fmt.Sprint(i%3)), storedRow{IntValue(int64(i))}); err != nil {
			t.Fatal(err)
		}
	}
	r, ok, err := d.lookup(collision, []byte("1"))
	if err != nil || !ok || r[0].Int != 28 {
		t.Fatalf("collision lookup %v %v %v", r, ok, err)
	}
	var index int64
	if err := d.bucket(collision, func(_ []byte, r storedRow) error {
		if r[0].Int != index {
			t.Fatalf("bucket order %v want %d", r, index)
		}
		index++
		return nil
	}); err != nil {
		t.Fatal(err)
	}
	d.close()
	rows.close()
	s.close()
	assertNoBlockingScratch(t, dir)
}
