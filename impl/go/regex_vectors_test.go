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

// compileRegexCase compiles a vector's pattern; flags "m" selects the line-anchor mode (regex.md §8d).
func compileRegexCase(pat, flags string) (*regexProgram, error) {
	if strings.Contains(flags, "m") {
		return compileRegexMode(pat, regexMode{nlAnchor: true})
	}
	return compileRegex(pat)
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
			pat = foldRegexPattern(pat, (*propertyTable)(nil))
		}
		prog, err := compileRegexCase(pat, tomlUnquote(c["flags"]))
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
			pat = foldRegexPattern(pat, (*propertyTable)(nil))
			subj = foldLowerSimple(subj, (*propertyTable)(nil))
		}
		prog, err := compileRegexCase(pat, tomlUnquote(c["flags"]))
		if err != nil {
			t.Fatalf("compile %q: %v", pattern, err)
		}
		m := newMeter()
		caps, ok, err := prog.run([]rune(subj), m)
		if err != nil {
			t.Fatalf("run %q/%q: %v", pattern, input, err)
		}
		wantMatched := c["matched"] == "true"
		if ok != wantMatched {
			t.Errorf("matched for %q/%q: got %v want %v", pattern, input, ok, wantMatched)
		}
		var wantCaps []int64
		if wantMatched {
			wantCaps = parsePairs(c["caps"])
		}
		var gotCaps []int64
		if ok {
			gotCaps = prog.slots(caps)
		}
		if !reflect.DeepEqual(gotCaps, wantCaps) {
			t.Errorf("caps for %q/%q: got %v want %v", pattern, input, gotCaps, wantCaps)
		}
		wantSteps, _ := strconv.ParseInt(c["steps"], 10, 64)
		if m.Accrued != wantSteps {
			t.Errorf("steps for %q/%q: got %d want %d", pattern, input, m.Accrued, wantSteps)
		}
	}
}

// TestRegexSimilarVectors checks the SIMILAR TO contract (regex.md §8f): the translation, the
// dot-all match, and the four-search substring, with their compile and step costs.
func TestRegexSimilarVectors(t *testing.T) {
	t.Parallel()
	blocks := parseCaseBlocks(t, specPath(t, "regex/similar_vectors.toml"))
	if len(blocks) < 15 {
		t.Fatalf("expected the full vector set, got %d", len(blocks))
	}
	code := func(err error) string {
		if e, ok := err.(*EngineError); ok {
			return e.Code()
		}
		return ""
	}
	for _, c := range blocks {
		pattern, input := tomlUnquote(c["pattern"]), tomlUnquote(c["input"])
		var escape *string
		if raw, ok := c["escape"]; ok {
			e := tomlUnquote(raw)
			escape = &e
		}
		what := pattern + " on " + input
		translated, err := similarToEscape(pattern, escape)
		if want, ok := c["error"]; ok {
			if code(err) != tomlUnquote(want) {
				t.Errorf("error for %q: got %v want %s", what, err, want)
			}
			continue
		}
		if err != nil {
			t.Fatalf("translate %q: %v", what, err)
		}
		if want := tomlUnquote(c["translation"]); translated != want {
			t.Errorf("translation for %q: got %q want %q", what, translated, want)
		}
		chars := []rune(input)
		prog, err := compileSimilar(translated)
		if err != nil {
			t.Fatalf("compile %q: %v", what, err)
		}
		m := newMeter()
		ok, err := prog.isMatch(chars, m)
		if err != nil {
			t.Fatalf("match %q: %v", what, err)
		}
		if want := c["similar"] == "true"; ok != want {
			t.Errorf("similar for %q: got %v want %v", what, ok, want)
		}
		if want, _ := strconv.Atoi(c["similar_compile"]); prog.ninst() != want {
			t.Errorf("similar_compile for %q: got %d want %d", what, prog.ninst(), want)
		}
		if want, _ := strconv.ParseInt(c["similar_steps"], 10, 64); m.Accrued != want {
			t.Errorf("similar_steps for %q: got %d want %d", what, m.Accrued, want)
		}
		esc := "\\"
		if escape != nil {
			esc = *escape
		}
		sm, err := compileSimilarMatcher(pattern, esc)
		if want, ok := c["substring_error"]; ok {
			if code(err) != tomlUnquote(want) {
				t.Errorf("substring_error for %q: got %v want %s", what, err, want)
			}
			continue
		}
		if err != nil {
			t.Fatalf("compile substring %q: %v", what, err)
		}
		m = newMeter()
		i, j, found, err := sm.substring(chars, m)
		if err != nil {
			t.Fatalf("substring %q: %v", what, err)
		}
		if want, isNull := c["substring"], c["substring_null"] == "true"; found == isNull || found && string(chars[i:j]) != tomlUnquote(want) {
			t.Errorf("substring for %q: got %v %q want %s (null %v)", what, found, string(chars[i:j]), want, isNull)
		}
		if want, _ := strconv.Atoi(c["substring_compile"]); sm.ninst() != want {
			t.Errorf("substring_compile for %q: got %d want %d", what, sm.ninst(), want)
		}
		if want, _ := strconv.ParseInt(c["substring_steps"], 10, 64); m.Accrued != want {
			t.Errorf("substring_steps for %q: got %d want %d", what, m.Accrued, want)
		}
	}
}
