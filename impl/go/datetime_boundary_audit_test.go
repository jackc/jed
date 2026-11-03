package jed

// jed deliberately owns the full finite i64 timestamp range (timestamp.md §1),
// including BC dates outside PostgreSQL's range. These boundary cases therefore
// belong in per-core tests, alongside zone_shift_range_test.go.

import (
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
