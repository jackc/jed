package jed

// jed deliberately owns the full finite i64 timestamp range (timestamp.md §1),
// including BC dates outside PostgreSQL's range. These boundary cases therefore
// belong in per-core tests, alongside zone_shift_range_test.go.

import (
	"fmt"
	"math"
	"testing"
)

func TestTimestampFiniteBoundaryConstruction(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	for _, tc := range []struct {
		sql  string
		want int64
	}{
		{"SELECT timestamp '290309-12-21 19:59:05.224193 BC'", math.MinInt64 + 1},
		{"SELECT timestamptz '290309-12-21 19:59:05.224193+00 BC'", math.MinInt64 + 1},
		{"SELECT make_timestamp(-290309, 12, 21, 19, 59, 5.224193::f64)", math.MinInt64 + 1},
		{"SELECT make_timestamptz(-290309, 12, 21, 19, 59, 5.224193::f64)", math.MinInt64 + 1},
		{"SELECT timestamp '294247-01-10 04:00:54.775806'", math.MaxInt64 - 1},
	} {
		rows := query(t, db, tc.sql)
		if got := rows[0][0].Int; got != tc.want {
			t.Errorf("%s: got %d, want %d", tc.sql, got, tc.want)
		}
	}
	for _, sql := range []string{
		"SELECT timestamp '290309-12-21 19:59:05.224192 BC'",
		"SELECT timestamp '290309-12-21 19:59:05.224191 BC'",
		"SELECT timestamp '294247-01-10 04:00:54.775807'",
		"SELECT make_date((-9223372036854775808)::i64, 1, 1)",
		"SELECT make_timestamp((-9223372036854775808)::i64, 1, 1, 0, 0, 0::f64)",
	} {
		wantErr(t, db, sql, "22008")
	}
}

func TestDateTruncFiniteBoundary(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	for _, typ := range []string{"timestamp", "timestamptz"} {
		for _, unit := range []string{"milliseconds", "second", "minute", "hour", "day", "week", "month", "quarter", "year", "decade", "century", "millennium"} {
			sql := fmt.Sprintf("SELECT date_trunc('%s', %s '290309-12-21 19:59:05.224193 BC')", unit, typ)
			wantErr(t, db, sql, "22008")
		}
		// The midnight product itself is out of range on this day, but the
		// completed sub-day timestamp remains valid and must not be rejected.
		for _, unit := range []string{"microseconds", "milliseconds", "second"} {
			sql := fmt.Sprintf("SELECT date_trunc('%s', %s '290309-12-21 19:59:06 BC')", unit, typ)
			rows := query(t, db, sql)
			if got := rows[0][0].Int; got != math.MinInt64+775808 {
				t.Errorf("%s: got %d", sql, got)
			}
		}
	}
	wantErr(t, db, "SELECT date_trunc('day', timestamptz '290309-12-21 19:59:06 BC', 'UTC')", "22008")
}

func TestTimestampMonthShiftPreservesEarliestFiniteSecond(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	for _, typ := range []string{"timestamp", "timestamptz"} {
		for _, shift := range []string{"- interval '1 month'", "+ interval '-1 month'"} {
			sql := fmt.Sprintf("SELECT %s '290308-01-21 19:59:05.224193 BC' %s", typ, shift)
			rows := query(t, db, sql)
			if got := rows[0][0].Int; got != math.MinInt64+1 {
				t.Errorf("%s: got %d, want %d", sql, got, int64(math.MinInt64+1))
			}
		}
		wantErr(t, db, fmt.Sprintf("SELECT %s '290308-01-21 19:59:05.224192 BC' - interval '1 month'", typ), "22008")
	}
}

// PostgreSQL checks each completed calendar step before applying later fields.
// These tests use jed's own finite boundaries, which differ from PostgreSQL's.
func TestTimestampCalendarStepsRejectNonfiniteIntermediates(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	for _, typ := range []string{"timestamp", "timestamptz"} {
		for _, tc := range []struct {
			input, forward, reverse string
		}{
			{"294246-12-10 04:00:54.775807", "1 month -1 day", "-1 month 1 day"},
			{"294246-12-10 04:00:54.775808", "1 month -1 day", "-1 month 1 day"},
			{"290308-01-21 19:59:05.224192 BC", "-1 month 1 day", "1 month -1 day"},
			{"290308-01-21 19:59:05.224191 BC", "-1 month 1 day", "1 month -1 day"},
			{"294247-01-09 04:00:54.775807", "1 day -1 second", "-1 day 1 second"},
			{"294247-01-09 04:00:54.775808", "1 day -1 second", "-1 day 1 second"},
			{"290309-12-22 19:59:05.224192 BC", "-1 day 1 second", "1 day -1 second"},
			{"290309-12-22 19:59:05.224191 BC", "-1 day 1 second", "1 day -1 second"},
		} {
			wantErr(t, db, fmt.Sprintf("SELECT %s '%s' + interval '%s'", typ, tc.input, tc.forward), "22008")
			wantErr(t, db, fmt.Sprintf("SELECT %s '%s' - interval '%s'", typ, tc.input, tc.reverse), "22008")
		}
	}
}
