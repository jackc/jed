// bench-sqlite benchmarks SQLite via modernc.org/sqlite — the pure-Go port
// (spec/design/benchmarks.md §7). bench-sqlite-cgo is the same harness over the cgo
// driver; the shared adapter lives in internal/bench (which imports no driver).
package main

import (
	_ "modernc.org/sqlite"

	"jed-bench/internal/bench"
)

func main() {
	bench.Main(bench.Config{
		Engine: "sqlite", Lang: "go", Variant: "modernc",
		Open: func(dataDir, dataset string) (bench.Engine, error) {
			return bench.OpenSQLite("sqlite", dataDir, dataset)
		},
	})
}
