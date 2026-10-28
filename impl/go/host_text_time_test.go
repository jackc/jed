package jed

// Host-API behavior the corpus cannot express (CLAUDE.md §10): what a Go host hands the engine as
// text and time. (1) Text the engine receives from outside — SQL text, a string parameter, a Value a
// host builds — must be valid UTF-8 without NUL (spec/design/types.md §11), rejected as PostgreSQL
// rejects it (22021 naming the first bad sequence) BEFORE it is stored, so a file never holds a text
// value a reopen would report as XX001. (2) A time.Time binds to timestamptz, timestamp, and date by
// the inferred parameter type (api.md §11.2), and an infinite timestamp / date has no time.Time form.

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"
)

func sqlStateOf(t *testing.T, err error) string {
	t.Helper()
	var ee *EngineError
	if !errors.As(err, &ee) {
		t.Fatalf("not an EngineError: %v", err)
	}
	return ee.Code()
}

func TestInvalidUTF8TextIsRejectedEverywhere(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	mustExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	ctx := context.Background()
	// The exact PostgreSQL message: the first invalid sequence's bytes, up to its expected length.
	cases := []struct {
		name, sql, msg string
		args           []any
	}{
		{"literal", "INSERT INTO t VALUES (1, 'a\xffb')", "0xff", nil},
		{"truncated literal", "INSERT INTO t VALUES (1, 'a\xe2\x82')", "0xe2 0x82 0x27", nil},
		{"NUL literal", "INSERT INTO t VALUES (1, 'a\x00b')", "0x00", nil},
		{"quoted identifier", "CREATE TABLE \"q\xff\" (x i32)", "0xff", nil},
		{"NUL identifier", "CREATE TABLE \"q\x00\" (x i32)", "0x00", nil},
		{"comment", "INSERT INTO t VALUES (1, 'ok') -- \xff", "0xff", nil},
		{"string param", "INSERT INTO t VALUES (1, $1)", "0xff", []any{"a\xffb"}},
		{"NUL string param", "INSERT INTO t VALUES (1, $1)", "0x00", []any{"a\x00b"}},
		{"string slice param", "SELECT $1::text[]", "0xff", []any{[]string{"ok", "\xff"}}},
		{"raw Value param", "INSERT INTO t VALUES (1, $1)", "0xff", []any{TextValue("\xff")}},
	}
	for _, c := range cases {
		_, err := s.Exec(ctx, c.sql, c.args...)
		if err == nil {
			t.Fatalf("%s: expected 22021", c.name)
		}
		if code := sqlStateOf(t, err); code != "22021" {
			t.Fatalf("%s: got %s (%v), want 22021", c.name, code, err)
		}
		want := `invalid byte sequence for encoding "UTF8": ` + c.msg
		if !strings.Contains(err.Error(), want) {
			t.Fatalf("%s: message %q does not contain %q", c.name, err.Error(), want)
		}
	}
	// Nothing was stored, and valid text (including non-ASCII) still binds.
	var n int64
	if err := s.QueryRow(ctx, "SELECT count(*) FROM t").Scan(&n); err != nil || n != 0 {
		t.Fatalf("count %d %v, want 0", n, err)
	}
	if _, err := s.Exec(ctx, "INSERT INTO t VALUES (1, $1), (2, 'hé')", "é\U0001F600"); err != nil {
		t.Fatal(err)
	}
	var v string
	if err := s.QueryRow(ctx, "SELECT v FROM t WHERE id = 1").Scan(&v); err != nil || v != "é\U0001F600" {
		t.Fatalf("got %q %v", v, err)
	}
}

func TestTimeBindsByInferredTemporalType(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	mustExec(t, s, "CREATE TABLE ev (id i32 PRIMARY KEY, at timestamptz, local timestamp, day date)")
	ctx := context.Background()
	// The calendar date differs between the time's zone and UTC only near midnight, so use such a
	// time: 2024-03-09 23:30 at UTC-5 is 2024-03-10 04:30 UTC. (A fixed offset: the test environment
	// loads no named-zone data.)
	tm := time.Date(2024, 3, 9, 23, 30, 15, 123456789, time.FixedZone("UTC-5", -5*3600))
	if _, err := s.Exec(ctx, "INSERT INTO ev VALUES (1, $1, $2, $3)", tm, tm, tm); err != nil {
		t.Fatal(err)
	}
	var at, local, day string
	if err := s.QueryRow(ctx, "SELECT at::text, local::text, day::text FROM ev WHERE id = 1").Scan(&at, &local, &day); err != nil {
		t.Fatal(err)
	}
	// timestamptz: the instant (rendered in the session zone, UTC by default); timestamp / date:
	// the time's own calendar fields (api.md §11.2), microsecond precision.
	if at != "2024-03-10 04:30:15.123456+00" {
		t.Fatalf("timestamptz: %q", at)
	}
	if local != "2024-03-09 23:30:15.123456" {
		t.Fatalf("timestamp: %q", local)
	}
	if day != "2024-03-09" {
		t.Fatalf("date: %q", day)
	}
	// An explicit cast is enough context: `$1::date` and `$1::timestamp` bind, as api.md §11.2 says.
	var d, ts string
	if err := s.QueryRow(ctx, "SELECT ($1::date)::text, ($2::timestamp)::text", tm, tm).Scan(&d, &ts); err != nil {
		t.Fatal(err)
	}
	if d != "2024-03-09" || ts != "2024-03-09 23:30:15.123456" {
		t.Fatalf("cast binding: %q %q", d, ts)
	}
	// A UTC-midnight date value — the common way to build a date in Go — binds as that date whatever
	// the session zone, and round-trips through Scan.
	if err := s.SetTimeZone("-05:00"); err != nil {
		t.Fatal(err)
	}
	utcDay := time.Date(2024, 1, 1, 0, 0, 0, 0, time.UTC)
	var back time.Time
	if err := s.QueryRow(ctx, "SELECT $1::date", utcDay).Scan(&back); err != nil {
		t.Fatal(err)
	}
	if !back.Equal(utcDay) {
		t.Fatalf("date round trip: %v, want %v", back, utcDay)
	}
	// A []time.Time binds as an array of the element's inferred temporal type.
	var cnt int64
	if err := s.QueryRow(ctx, "SELECT count(*) FROM ev WHERE day = ANY($1)", []time.Time{utcDay, time.Date(2024, 3, 9, 12, 0, 0, 0, time.UTC)}).Scan(&cnt); err != nil || cnt != 1 {
		t.Fatalf("date array: %d %v, want 1", cnt, err)
	}
	// Non-temporal targets still reject a time.Time as a family mismatch (42804), like a literal.
	if _, err := s.Exec(ctx, "SELECT $1::i32", tm); err == nil || sqlStateOf(t, err) != "42804" {
		t.Fatalf("time into i32: %v, want 42804", err)
	}
}

func TestInfiniteTemporalHasNoTimeForm(t *testing.T) {
	t.Parallel()
	s := memDB().Session(SessionOptions{})
	ctx := context.Background()
	for _, sql := range []string{
		"SELECT 'infinity'::timestamp", "SELECT '-infinity'::timestamp",
		"SELECT 'infinity'::timestamptz", "SELECT 'infinity'::date", "SELECT '-infinity'::date",
	} {
		var tm time.Time
		if err := s.QueryRow(ctx, sql).Scan(&tm); err == nil || !strings.Contains(err.Error(), "infinite") {
			t.Fatalf("%s: Scan into time.Time: %v, want an error naming the infinite value", sql, err)
		}
		// Values() hands the cell back as its raw Value instead of a bogus finite instant.
		rows, err := s.Query(ctx, sql)
		if err != nil {
			t.Fatal(err)
		}
		if !rows.Next() {
			t.Fatal("no row")
		}
		vals, err := rows.Values()
		if err != nil {
			t.Fatal(err)
		}
		v, ok := vals[0].(Value)
		if !ok || !isInfiniteTemporal(v) {
			t.Fatalf("%s: Values() gave %T %v, want the raw infinite Value", sql, vals[0], vals[0])
		}
		rows.Close()
	}
	// A finite value still scans.
	var tm time.Time
	if err := s.QueryRow(ctx, "SELECT '2024-01-02'::date").Scan(&tm); err != nil || !tm.Equal(time.Date(2024, 1, 2, 0, 0, 0, 0, time.UTC)) {
		t.Fatalf("finite date: %v %v", tm, err)
	}
}
