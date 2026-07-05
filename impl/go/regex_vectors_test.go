package jed

// Cross-core regex compile-determinism + execution check (spec/design/regex.md §9). Reads the
// authored spec/regex/{program,match}_vectors.toml and asserts this core compiles each pattern to
// the exact instruction listing + class table + count (= regex_compile cost), and runs the VM to
// the exact match result, capture spans, and regex_step count. The Rust and TS cores run the
// equivalent check against the SAME files, pinning the three engines identical (CLAUDE.md §2/§8 —
// the byte-level contract the SQL conformance corpus cannot express, §10).

import (
	"os"
	"reflect"
	"strconv"
	"strings"
	"testing"
)

// caseFields holds the parsed key→value of one [[case]] block, with array values kept raw.
type caseFields map[string]string

// parseCaseBlocks splits a fixture into [[case]] blocks of key→raw-value, skipping comments/blanks.
func parseCaseBlocks(t *testing.T, path string) []caseFields {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	var blocks []caseFields
	var cur caseFields
	for _, raw := range strings.Split(string(data), "\n") {
		line := strings.TrimSpace(raw)
		if line == "[[case]]" {
			cur = caseFields{}
			blocks = append(blocks, cur)
			continue
		}
		if line == "" || strings.HasPrefix(line, "#") || cur == nil {
			continue
		}
		key, val, ok := strings.Cut(line, "=")
		if !ok {
			continue
		}
		cur[strings.TrimSpace(key)] = strings.TrimSpace(val)
	}
	return blocks
}

// tomlUnquote strips surrounding quotes and applies TOML basic-string \\ / \" / \n / \t unescaping.
func tomlUnquote(s string) string {
	s = strings.TrimSpace(s)
	if len(s) < 2 {
		return s
	}
	inner := s[1 : len(s)-1]
	var b strings.Builder
	for i := 0; i < len(inner); i++ {
		if inner[i] == '\\' && i+1 < len(inner) {
			i++
			switch inner[i] {
			case '\\':
				b.WriteByte('\\')
			case '"':
				b.WriteByte('"')
			case 'n':
				b.WriteByte('\n')
			case 't':
				b.WriteByte('\t')
			default:
				b.WriteByte('\\')
				b.WriteByte(inner[i])
			}
		} else {
			b.WriteByte(inner[i])
		}
	}
	return b.String()
}

// parseStrArray parses `["a", "b"]` (or `[]`) of quoted strings.
func parseStrArray(val string) []string {
	val = strings.TrimSpace(val)
	inner := strings.TrimSuffix(strings.TrimPrefix(val, "["), "]")
	if strings.TrimSpace(inner) == "" {
		return nil
	}
	parts := strings.Split(inner, ",")
	out := make([]string, len(parts))
	for i, p := range parts {
		out[i] = tomlUnquote(strings.TrimSpace(p))
	}
	return out
}

// parsePairs parses `[[0, 1], [2, 5]]` (or `[]`) into flat [start,end,...] int64s.
func parsePairs(val string) []int64 {
	var out []int64
	var num strings.Builder
	flush := func() {
		if num.Len() > 0 {
			n, _ := strconv.ParseInt(num.String(), 10, 64)
			out = append(out, n)
			num.Reset()
		}
	}
	for _, ch := range val {
		switch {
		case ch >= '0' && ch <= '9' || ch == '-':
			num.WriteRune(ch)
		default:
			flush()
		}
	}
	flush()
	return out
}

func TestRegexProgramVectors(t *testing.T) {
	t.Parallel()
	blocks := parseCaseBlocks(t, specPath(t, "regex/program_vectors.toml"))
	if len(blocks) < 25 {
		t.Fatalf("expected the full vector set, got %d", len(blocks))
	}
	for _, c := range blocks {
		pattern := tomlUnquote(c["pattern"])
		pat := pattern
		if strings.Contains(tomlUnquote(c["flags"]), "i") {
			pat = foldLowerSimple(pat, loadedProperty())
		}
		prog, err := compileRegex(pat)
		if err != nil {
			t.Fatalf("compile %q: %v", pattern, err)
		}
		wantProg := parseStrArray(c["prog"])
		if !reflect.DeepEqual(prog.listing(), wantProg) {
			t.Errorf("program for %q:\n got %v\nwant %v", pattern, prog.listing(), wantProg)
		}
		wantClasses := parseStrArray(c["classes"])
		got := prog.classListing()
		if len(got) == 0 {
			got = nil
		}
		if !reflect.DeepEqual(got, wantClasses) {
			t.Errorf("classes for %q: got %v want %v", pattern, got, wantClasses)
		}
		wantCount, _ := strconv.Atoi(c["count"])
		if prog.ninst() != wantCount {
			t.Errorf("count for %q: got %d want %d", pattern, prog.ninst(), wantCount)
		}
	}
}

func TestRegexMatchVectors(t *testing.T) {
	t.Parallel()
	blocks := parseCaseBlocks(t, specPath(t, "regex/match_vectors.toml"))
	if len(blocks) < 25 {
		t.Fatalf("expected the full vector set, got %d", len(blocks))
	}
	for _, c := range blocks {
		pattern := tomlUnquote(c["pattern"])
		input := tomlUnquote(c["input"])
		pat, subj := pattern, input
		if strings.Contains(tomlUnquote(c["flags"]), "i") {
			pat = foldLowerSimple(pat, loadedProperty())
			subj = foldLowerSimple(subj, loadedProperty())
		}
		prog, err := compileRegex(pat)
		if err != nil {
			t.Fatalf("compile %q: %v", pattern, err)
		}
		m := newMeter()
		caps, err := prog.run([]rune(subj), m)
		if err != nil {
			t.Fatalf("run %q/%q: %v", pattern, input, err)
		}
		wantMatched := c["matched"] == "true"
		if (caps != nil) != wantMatched {
			t.Errorf("matched for %q/%q: got %v want %v", pattern, input, caps != nil, wantMatched)
		}
		var wantCaps []int64
		if wantMatched {
			wantCaps = parsePairs(c["caps"])
		}
		if caps == nil {
			caps = nil
		}
		if !reflect.DeepEqual(caps, wantCaps) {
			t.Errorf("caps for %q/%q: got %v want %v", pattern, input, caps, wantCaps)
		}
		wantSteps, _ := strconv.ParseInt(c["steps"], 10, 64)
		if m.Accrued != wantSteps {
			t.Errorf("steps for %q/%q: got %d want %d", pattern, input, m.Accrued, wantSteps)
		}
	}
}
