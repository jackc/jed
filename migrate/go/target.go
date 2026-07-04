package migrate

import (
	"fmt"
	"strconv"
	"strings"
)

// ResolveTargets resolves a tern-style destination spec into the ordered list of absolute
// target versions to migrate to (design.md §6/§9). current is the version presently
// recorded; n is the highest available sequence. The spec grammar:
//
//	"last" or ""  → migrate to N (the default; the dominant startup case)
//	"<integer>"   → migrate to that absolute version
//	"+N"          → migrate up N steps   (current + N)
//	"-N"          → migrate down N steps (current - N)
//	"-+N"         → redo the last N: migrate down N, then back up N ([current-N, current])
//
// Every resolved target is range-checked against 0 … N; an out-of-range result is an error.
// Most specs resolve to a single target; the redo form resolves to two, applied in order.
// Relative-grammar resolution is the caller's concern (design.md §9 — typically a CLI); the
// library's MigrateTo takes only an absolute target.
func ResolveTargets(spec string, current, n int) ([]int, error) {
	spec = strings.TrimSpace(spec)
	if spec == "" || spec == "last" {
		return []int{n}, nil
	}

	// Redo: "-+N" (down N, then up N).
	if rest, ok := strings.CutPrefix(spec, "-+"); ok {
		steps, err := strconv.Atoi(rest)
		if err != nil || steps < 0 {
			return nil, fmt.Errorf("bad destination %q: expected -+N", spec)
		}
		down := current - steps
		if err := checkRange(down, n); err != nil {
			return nil, err
		}
		return []int{down, current}, nil
	}

	// Relative up/down: "+N" / "-N".
	if strings.HasPrefix(spec, "+") || strings.HasPrefix(spec, "-") {
		delta, err := strconv.Atoi(spec) // Atoi handles the leading sign
		if err != nil {
			return nil, fmt.Errorf("bad destination %q: expected +N, -N, -+N, an integer, or last", spec)
		}
		target := current + delta
		if err := checkRange(target, n); err != nil {
			return nil, err
		}
		return []int{target}, nil
	}

	// Absolute integer.
	target, err := strconv.Atoi(spec)
	if err != nil {
		return nil, fmt.Errorf("bad destination %q: expected +N, -N, -+N, an integer, or last", spec)
	}
	if err := checkRange(target, n); err != nil {
		return nil, err
	}
	return []int{target}, nil
}

func checkRange(target, n int) error {
	if target < 0 || target > n {
		return &BadVersion{Version: target, N: n, Whence: "target"}
	}
	return nil
}
