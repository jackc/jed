package jed

import "testing"

func TestSingleValuesInsertEligibility(t *testing.T) {
	if !singleValuesInsertEligible(1, false) {
		t.Fatal("one plain VALUES candidate must use the single-row path")
	}
	for _, tc := range []struct {
		count       int
		hasConflict bool
	}{
		{count: 0},
		{count: 2},
		{count: 1, hasConflict: true},
	} {
		if singleValuesInsertEligible(tc.count, tc.hasConflict) {
			t.Fatalf("count=%d conflict=%v must use the batch fallback", tc.count, tc.hasConflict)
		}
	}
}
