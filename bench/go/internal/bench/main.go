package bench

import (
	"encoding/json"
	"fmt"
	"os"
)

// Main is the uniform binary entrypoint (spec/design/benchmarks.md §6):
//
//	bench-<engine> <corpus_dir> <data_dir> <out_path> [name_filter_substring]
//
// Progress goes to stderr; one JSONL Result per completed bench goes to out_path
// (truncated on open). Exits non-zero on any failure.
func Main(cfg Config) {
	if len(os.Args) < 4 || len(os.Args) > 5 {
		fmt.Fprintf(os.Stderr, "usage: %s <corpus_dir> <data_dir> <out_path> [name_filter]\n", os.Args[0])
		os.Exit(2)
	}
	corpusDir, dataDir, outPath := os.Args[1], os.Args[2], os.Args[3]
	filter := ""
	if len(os.Args) == 5 {
		filter = os.Args[4]
	}

	results, err := Run(cfg, corpusDir, dataDir, filter)
	if err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}

	out, err := os.Create(outPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
	defer out.Close()
	for _, r := range results {
		line, err := json.Marshal(r)
		if err != nil {
			fmt.Fprintf(os.Stderr, "error: %v\n", err)
			os.Exit(1)
		}
		fmt.Fprintf(out, "%s\n", line)
	}
}
