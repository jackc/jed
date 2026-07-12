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
