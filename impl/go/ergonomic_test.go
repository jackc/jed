package jed

import (
	"fmt"
	"math"
	"strconv"
	"testing"
)

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
