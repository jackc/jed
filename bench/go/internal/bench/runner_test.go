package bench

import "testing"

func TestWriteTable(t *testing.T) {
	tests := map[string]string{
		"INSERT INTO orders VALUES ($1)": "orders",
		"UPDATE orders SET v = $1":       "orders",
		"DELETE FROM orders WHERE id=$1": "orders",
	}
	for sql, want := range tests {
		if got := writeTable(sql); got != want {
			t.Errorf("writeTable(%q) = %q, want %q", sql, got, want)
		}
	}
}

func TestPercentile(t *testing.T) {
	samples := []int64{0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10}
	for pct, want := range map[int]int64{50: 5, 90: 9, 99: 9} {
		if got := percentile(samples, pct); got != want {
			t.Errorf("percentile(samples, %d) = %d, want %d", pct, got, want)
		}
	}
}
