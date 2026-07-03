package jed

// Track-A gate benchmark (throwaway; not part of the suite's contract).
//
// Question: on a wide, all-fixed-width table, does a scan that TOUCHES ONE COLUMN
// waste time/memory reconstructing the UNTOUCHED columns? That reconstruction cost
// is exactly what packed-leaf S3 (row_at_masked) + Stage-2 execVectorizedScan would
// remove. If per-row ns/B grow with table width despite touching one column, the
// dividend is real; if flat, it isn't.
//
// Methodology: must be FILE-BACKED and REOPENED. Pure in-memory DBs stay fully
// decoded (from_image) and never pack leaves, so they would show ~zero slope and
// falsely kill the gate. After create+insert+close+reopen, leaves fault back as
// Packed and reconstruct on every scan via row_at -> decode_record_lazy.
//
// Run: mise exec -- go test -run '^$' -bench BenchmarkTrackAWideScan -benchmem ./...

import (
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

const trackARows = 50_000

// buildWideTable creates a file-backed table `t` with an i32 PK `id` plus `w` i32
// data columns c0..c{w-1}, populated with trackARows rows, then closes it so a fresh
// OpenDatabase faults Packed leaves. Returns the file path.
func buildWideTable(b *testing.B, w int) string {
	b.Helper()
	path := filepath.Join(b.TempDir(), fmt.Sprintf("wide_w%d.jed", w))
	db, err := CreateDatabase(CreateOptions{Path: path})
	if err != nil {
		b.Fatal(err)
	}

	cols := make([]string, 0, w+1)
	cols = append(cols, "id i32 PRIMARY KEY")
	for j := 0; j < w; j++ {
		cols = append(cols, fmt.Sprintf("c%d i32", j))
	}
	if _, err := db.Execute("CREATE TABLE t ("+strings.Join(cols, ", ")+")", nil); err != nil {
		b.Fatal(err)
	}

	const chunk = 1000
	for start := 0; start < trackARows; start += chunk {
		var sb strings.Builder
		sb.WriteString("INSERT INTO t VALUES ")
		for i := start; i < start+chunk && i < trackARows; i++ {
			if i > start {
				sb.WriteByte(',')
			}
			sb.WriteByte('(')
			sb.WriteString(fmt.Sprintf("%d", i)) // id
			for j := 0; j < w; j++ {
				// small deterministic values; sum(c0) stays well within i64
				sb.WriteString(fmt.Sprintf(",%d", (i+j)%1000))
			}
			sb.WriteByte(')')
		}
		if _, err := db.Execute(sb.String(), nil); err != nil {
			b.Fatal(err)
		}
	}
	if err := db.Close(); err != nil {
		b.Fatal(err)
	}
	return path
}

func BenchmarkTrackAWideScan(b *testing.B) {
	widths := []int{1, 4, 16, 64}
	queries := []struct {
		name string
		sql  string
	}{
		{"sum_c0", "SELECT sum(c0) FROM t"},      // vectorized agg, touches 1 col
		{"count_star", "SELECT count(*) FROM t"}, // touches 0 cols (scan-feed control)
		{"project_c0", "SELECT c0 FROM t"},       // projection scan, touches 1 col, emits N
		// A3 filter vectorization: a WHERE predicate over the lanes (selection vector) instead of the
		// full-width row path. ~50% selectivity so both branches do real work.
		{"sum_c0_filt", "SELECT sum(c0) FROM t WHERE c0 > 500"}, // filtered agg, touches 1 col
		{"project_c0_filt", "SELECT c0 FROM t WHERE c0 > 500"},  // filtered projection, touches 1 col
	}

	for _, w := range widths {
		path := buildWideTable(b, w)
		db, err := OpenDatabase(path)
		if err != nil {
			b.Fatal(err)
		}
		sess := db.Session(SessionOptions{})

		for _, q := range queries {
			b.Run(fmt.Sprintf("w%02d/%s", w, q.name), func(b *testing.B) {
				b.ReportAllocs()
				for i := 0; i < b.N; i++ {
					rows, err := sess.Query(q.sql, nil)
					if err != nil {
						b.Fatal(err)
					}
					for rows.Next() {
						_ = rows.Row()
					}
					if err := rows.Err(); err != nil {
						b.Fatal(err)
					}
					rows.Close()
				}
			})
		}
		db.Close()
	}
}
