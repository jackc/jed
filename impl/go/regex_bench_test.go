package jed

import (
	"regexp"
	"strings"
	"testing"
)

// Subjects are already in each VM's native representation. Compilation and
// UTF-8 to rune conversion are outside the timed predicate call.
func BenchmarkRegexPredicate(b *testing.B) {
	var class strings.Builder
	class.WriteByte('[')
	for i := range 1024 {
		class.WriteRune(rune(0x1000 + 2*i))
	}
	class.WriteByte(']')
	for _, tc := range []struct {
		name, pattern, subject string
		want                   bool
	}{
		{"filter", "[aeiou]{2}", "invoice received", true},
		{"late_literal", "needle", strings.Repeat("x", 2048) + "needle", true},
		{"missing_literal", "needle", strings.Repeat("x", 2048), false},
		{"false_starts", "needle[0-9]+", strings.Repeat("nxxxxx", 341) + "needle42", true},
		{"unicode_prefix", "猫[a-z]+", strings.Repeat("界", 2048) + "猫abc", true},
		{"dense_starts", "aaab", strings.Repeat("a", 2048), false},
		{"dense_starts_long", "aaab", strings.Repeat("a", 65536), false},
		{"dense_starts_late_hit", "aaab", strings.Repeat("a", 65536) + "b", true},
		{"groups", strings.Repeat("(a)", 12), strings.Repeat("a", 12), true},
		{"wide_class", class.String(), strings.Repeat("界", 256), false},
	} {
		b.Run(tc.name, func(b *testing.B) {
			p, err := compileRegex(tc.pattern)
			if err != nil {
				b.Fatal(err)
			}
			input := []rune(tc.subject)
			re := regexp.MustCompile(tc.pattern)
			b.Run("jed", func(b *testing.B) {
				var lifetime int64
				m := costMeter{lifetimeTotal: &lifetime}
				for b.Loop() {
					ok, err := p.isMatch(input, &m)
					if err != nil || ok != tc.want {
						b.Fatalf("match = %v, err = %v", ok, err)
					}
				}
				b.ReportMetric(float64(m.Accrued)/float64(b.N), "cost/op")
			})
			b.Run("stdlib", func(b *testing.B) {
				for b.Loop() {
					if re.MatchString(tc.subject) != tc.want {
						b.Fatal("unexpected match result")
					}
				}
			})
		})
	}
}
