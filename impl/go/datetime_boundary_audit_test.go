package jed

import "testing"

// jed accepts integer-family constructor arguments; PostgreSQL only accepts int4.
func TestTimestampFiniteBoundaryConstruction(t *testing.T) {
	t.Parallel()
	db := dbWith(t)
	for _, sql := range []string{
		"SELECT make_date((-9223372036854775808)::i64, 1, 1)",
		"SELECT make_timestamp((-9223372036854775808)::i64, 1, 1, 0, 0, 0::f64)",
	} {
		wantErr(t, db, sql, "22008")
	}
}
