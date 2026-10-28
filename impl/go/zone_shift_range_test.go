package jed

// Zone-shift range guard (timezones.md §6): every path that moves a finite timestamp across a UT
// offset — the cross-family casts, AT TIME ZONE, date_trunc/EXTRACT/date_part in a zone,
// make_timestamptz, and the timestamptz → text render — traps 22008 when the shifted reading leaves
// jed's int64-µs range or lands exactly on a ±infinity sentinel (MinInt64/MaxInt64). jed's timestamp
// range (≈ 290309 BC … 294247 AD) is far wider than PostgreSQL's, so none of these extremes are
// oracle-checkable; this file pins them per core. Mirrors impl/rust/tests/zone_shift_range.rs and
// impl/ts/tests/zone_shift_range.test.ts.

import (
	"math"
	"testing"
)

// MaxInt64 µs is 294247-01-10 04:00:54.775807 UTC; MinInt64 µs is 290309-12-21 19:59:05.224192 BC.
// Each seeded value sits within a few hours of an end. Session zones use the POSIX fixed-offset
// spelling, where a positive offset is WEST of UTC ('+05:00' = UTC-5, '-05:00' = UTC+5).
const (
	zsTsHi     = "294247-01-10 04:00:00"           // shifts past MaxInt64 in a UTC-5 reading
	zsTsLo     = "290309-12-22 00:00:00 BC"        // shifts past MinInt64 in a UTC+5 reading
	zsTsLoEdge = "290309-12-22 00:59:05.224192 BC" // MinInt64 + 5h exactly
	zsTzHi     = "294247-01-10 04:00:00+00"        // + 5h overflows
	zsTzHiEdge = "294247-01-09 23:00:54.775807+00" // MaxInt64 − 5h exactly
	zsTzLo     = "290309-12-22 00:00:00+00 BC"     // − 5h overflows
)

// zoneShiftDB is t(id i32 pk, ts timestamp, tz timestamptz, d date) with the near-range rows.
func zoneShiftDB(t *testing.T) *Session {
	t.Helper()
	return dbWith(t,
		"CREATE TABLE t (id i32 PRIMARY KEY, ts timestamp, tz timestamptz, d date)",
		"INSERT INTO t VALUES (1, '"+zsTsHi+"', '"+zsTzHi+"', '294247-01-10')",
		"INSERT INTO t VALUES (2, '"+zsTsLo+"', '"+zsTzLo+"', '290309-12-22 BC')",
		"INSERT INTO t VALUES (3, '"+zsTsLoEdge+"', '"+zsTzHiEdge+"', '300000-01-01')",
	)
}

func zoneShiftTraps(t *testing.T, db *Session, zone, expr string, id int) {
	t.Helper()
	if err := db.SetTimeZone(zone); err != nil {
		t.Fatal(err)
	}
	sql := "SELECT " + expr + " FROM t WHERE id = " + itoaTest(id)
	out, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("%s %s (row %d): expected 22008, got %v", zone, expr, id, out.Rows[0][0])
	}
	if code := errCodeOf(err); code != "22008" {
		t.Fatalf("%s %s (row %d): got %v, want 22008", zone, expr, id, err)
	}
}

func TestZoneShiftPastMaxInt64Traps(t *testing.T) {
	t.Parallel()
	db := zoneShiftDB(t)
	// A wall clock read in a UTC-5 zone moves +5h.
	for _, expr := range []string{
		"ts :: timestamptz",
		"ts AT TIME ZONE '+05:00'",
		"make_timestamptz(294247, 1, 10, 4, 0, 0)",
		"d :: timestamptz", // midnight, read in the session zone
	} {
		zoneShiftTraps(t, db, "+05:00", expr, 1)
	}
	// make_timestamptz reads its explicit zone with the ISO sign (functions.md §11): '-05:00' is UTC-5.
	zoneShiftTraps(t, db, "UTC", "make_timestamptz(294247, 1, 10, 4, 0, 0, '-05:00')", 1)
	// An instant rendered in a UTC+5 zone moves +5h.
	for _, expr := range []string{
		"tz :: timestamp",
		"tz :: date",
		"tz :: text",
		"tz AT TIME ZONE '-05:00'",
		"date_trunc('day', tz)",
		"EXTRACT(hour FROM tz)",
		"date_part('hour', tz)",
	} {
		zoneShiftTraps(t, db, "-05:00", expr, 1)
	}
	zoneShiftTraps(t, db, "UTC", "date_trunc('day', tz, '-05:00')", 1)
}

func TestZoneShiftPastMinInt64Traps(t *testing.T) {
	t.Parallel()
	db := zoneShiftDB(t)
	// A wall clock read in a UTC+5 zone moves −5h.
	for _, expr := range []string{"ts :: timestamptz", "ts AT TIME ZONE '-05:00'", "d :: timestamptz"} {
		zoneShiftTraps(t, db, "-05:00", expr, 2)
	}
	// An instant rendered in a UTC-5 zone moves −5h.
	for _, expr := range []string{
		"tz :: timestamp",
		"tz :: date",
		"tz :: text",
		"tz AT TIME ZONE '+05:00'",
		"date_trunc('day', tz)",
		"EXTRACT(hour FROM tz)",
		"date_part('hour', tz)",
	} {
		zoneShiftTraps(t, db, "+05:00", expr, 2)
	}
}

func TestZoneShiftOntoInfinitySentinelTraps(t *testing.T) {
	t.Parallel()
	db := zoneShiftDB(t)
	// Landing EXACTLY on MinInt64 / MaxInt64 would silently become ±infinity.
	zoneShiftTraps(t, db, "-05:00", "ts :: timestamptz", 3)
	zoneShiftTraps(t, db, "UTC", "ts AT TIME ZONE '-05:00'", 3)
	zoneShiftTraps(t, db, "-05:00", "tz :: timestamp", 3)
	zoneShiftTraps(t, db, "-05:00", "tz :: text", 3)
	zoneShiftTraps(t, db, "UTC", "tz AT TIME ZONE '-05:00'", 3)
	// One microsecond inside the range is representable.
	if err := db.SetTimeZone("-05:00"); err != nil {
		t.Fatal(err)
	}
	v := castAt(t, db, "(tz - INTERVAL '1 microsecond') :: timestamp", 3)
	if v.Kind != ValTimestamp || v.Int != math.MaxInt64-1 {
		t.Fatalf("got %v, want timestamp MaxInt64-1", v)
	}
}

func TestDateWideningPastTimestampRangeTraps(t *testing.T) {
	t.Parallel()
	// A date is wider than the timestamp range (date.md §1): its midnight widening must trap, not
	// wrap the day × µs/day product.
	db := zoneShiftDB(t)
	zoneShiftTraps(t, db, "UTC", "d :: timestamp", 3)
	zoneShiftTraps(t, db, "UTC", "d :: timestamptz", 3)
}

func TestZoneShiftInwardSucceeds(t *testing.T) {
	t.Parallel()
	// The guard rejects only an out-of-range result: the opposite-signed zone moves each value
	// toward the epoch and succeeds.
	db := zoneShiftDB(t)
	check := func(zone, expr string, id int, want ValueKind) {
		t.Helper()
		if err := db.SetTimeZone(zone); err != nil {
			t.Fatal(err)
		}
		if v := castAt(t, db, expr, id); v.Kind != want {
			t.Fatalf("%s %s (row %d): got %v", zone, expr, id, v)
		}
	}
	check("-05:00", "ts :: timestamptz", 1, ValTimestamptz)
	check("-05:00", "tz :: text", 2, ValText)
	check("+05:00", "tz :: timestamp", 1, ValTimestamp)
	check("+05:00", "ts :: timestamptz", 2, ValTimestamptz)
}
