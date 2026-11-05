package jed

import (
	"context"
	"fmt"
	"math"
	"strconv"
	"strings"
	"testing"
)

// Struct tags are a Go host API contract, outside the SQL conformance corpus.
func TestRowToStructByNameSkipTag(t *testing.T) {
	type record struct {
		ID      int64 `db:"key"`
		Name    string
		Ignored string `db:"-"`
	}
	db := memDB()
	for _, tc := range []struct {
		name, sql string
		wantError bool
	}{
		{"eligible fields", "SELECT 7 AS key, 'mapped' AS name", false},
		{"excluded field name", "SELECT 'excluded' AS ignored", true},
		{"excluded tag name", "SELECT 'excluded' AS \"-\"", true},
	} {
		t.Run(tc.name, func(t *testing.T) {
			rows, err := db.Query(context.Background(), tc.sql)
			if err != nil {
				t.Fatal(err)
			}
			defer rows.Close()
			if !rows.Next() {
				t.Fatalf("missing row: %v", rows.Err())
			}
			got, err := RowToStructByName[record](rows)
			if tc.wantError {
				if err == nil || !strings.Contains(err.Error(), "has no matching field") {
					t.Fatalf("expected unmatched column error, got %v", err)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if got != (record{ID: 7, Name: "mapped"}) {
				t.Fatalf("unexpected mapping: %+v", got)
			}
		})
	}
}

// Native int width is a host API concern, outside the SQL conformance corpus.
func TestScanIntBounds(t *testing.T) {
	for _, n := range []int64{math.MinInt64, math.MinInt32 - 1, math.MinInt32, -1, 0, math.MaxInt32, math.MaxInt32 + 1, math.MaxInt64} {
		t.Run(fmt.Sprint(n), func(t *testing.T) {
			got := 42
			err := scanOne(IntValue(n), &got)
			overflows := strconv.IntSize == 32 && (n < math.MinInt32 || n > math.MaxInt32)
			if overflows {
				if err == nil || err.Error() != fmt.Sprintf("value %d overflows int", n) {
					t.Fatalf("Scan(%d) error = %v, want integer overflow", n, err)
				}
				if got != 42 {
					t.Fatalf("failed Scan changed destination to %d", got)
				}
			} else if err != nil || int64(got) != n {
				t.Fatalf("Scan(%d) = %d, %v", n, got, err)
			}
		})
	}
}
