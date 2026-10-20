//go:build jed_specialization

package jed

import "testing"

func TestSpecializationScanSelectionAndClose(t *testing.T) {
	s := rowTable(t, 4)
	defer s.Close()
	for _, tt := range []struct {
		sql      string
		eligible bool
	}{
		{"SELECT v+1 FROM t ORDER BY id", true},
		{"SELECT coalesce(v,0),CASE WHEN v>1 THEN v ELSE 0 END FROM t WHERE v>0 AND v<4 ORDER BY id", true},
		{"SELECT v/0 FROM t ORDER BY id LIMIT 0", true},
		{"SELECT abs(v) FROM t ORDER BY id", false},
		{"SELECT v+1 FROM t WHERE abs(v)>0 ORDER BY id", false},
		{"SELECT v+1 FROM t WHERE id=2 ORDER BY id", specializationMode == "forced"},
	} {
		rows, err := s.queryValues(tt.sql, nil)
		if err != nil {
			t.Fatal(err)
		}
		_, selected := rows.cursor.(*kernelScanCursor)
		want := specializationScan && specializationMode != "ordinary" && tt.eligible
		if selected != want {
			t.Fatalf("%s selected=%v want=%v", tt.sql, selected, want)
		}
		c := specializationTestCursor(t, rows)
		// Selection/admission cannot have consumed input, including LIMIT 0's trap.
		if c.passed != 0 || c.produced != 0 {
			t.Fatal("eager scan")
		}
		rows.Close()
		cost := rows.Cost()
		if rows.Next() || rows.Cost() != cost {
			t.Fatal("closed cursor consumed input")
		}
		t.Logf("scan loop selected=%v: %s", selected, tt.sql)
	}
	// Saved output belongs to its caller and is not recycled as kernel scratch.
	rows, err := s.queryValues("SELECT CASE WHEN v>0 THEN v+1 ELSE 0 END FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() {
		t.Fatal(rows.Err())
	}
	saved := rows.Row()
	first := saved[0]
	for rows.Next() {
	}
	if rows.Err() != nil {
		t.Fatal(rows.Err())
	}
	rows.Close()
	if saved[0] != first {
		t.Fatal("output row was reused")
	}
}

// Only tests unwrap for meter injection. Rows still drives the selected loop.
func specializationUnwrapCursor(c cursor) cursor {
	if k, ok := c.(*kernelScanCursor); ok {
		return k.streamingCursor
	}
	return c
}
