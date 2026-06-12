// bench-sqlite-cgo benchmarks SQLite via mattn/go-sqlite3 — the cgo C-SQLite baseline.
// cgo is confined to this binary; the Go core and every other bench binary stay pure Go
// (CLAUDE.md §14, spec/design/benchmarks.md §7).
package main

import (
	_ "github.com/mattn/go-sqlite3"

	"jed-bench/internal/bench"
)

func main() {
	bench.Main(bench.Config{
		Engine: "sqlite", Lang: "go", Variant: "mattn-cgo",
		Open: func(dataDir, dataset string) (bench.Engine, error) {
			return bench.OpenSQLite("sqlite3", dataDir, dataset)
		},
	})
}
