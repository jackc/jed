package jed

import (
	"slices"
	"strconv"
	"unicode/utf8"
)

// Engine auto-names (spec/design/grammar.md §3.1): the names jed derives for an unnamed
// constraint, index, or owned sequence — <table>_pkey, <table>_<cols>_key, …_fkey, …_check,
// …_excl, …_idx, <table>_<col>_seq. A port of PostgreSQL's makeObjectName / ChooseRelationName /
// ChooseConstraintName / ChooseIndexNameAddition / ChooseIndexColumnNames (indexcmds.c,
// pg_constraint.c), so a derived name always fits the 63-byte identifier limit
// (maxIdentifierLength) and stays referenceable from SQL, byte for byte as PostgreSQL chooses it.
// Lengths are UTF-8 bytes; a cut never splits a character.

// clipName is the longest prefix of s that is at most limit bytes and ends on a character
// boundary (PostgreSQL's pg_mbcliplen over UTF-8).
func clipName(s string, limit int) string {
	if len(s) <= limit {
		return s
	}
	end := limit
	for end > 0 && !utf8.RuneStart(s[end]) {
		end--
	}
	return s[:end]
}

// makeObjectName is PostgreSQL's makeObjectName: name1[_name2]_label, shortening the longer of
// name1 / name2 one byte at a time until the whole fits in maxIdentifierLength bytes (each part
// then clipped back to a character boundary). The label is never shortened. hasName2 false is
// PG's NULL name2.
func makeObjectName(name1, name2 string, hasName2 bool, label string) string {
	overhead := len(label) + 1
	name1Len := len(name1)
	name2Len := 0
	if hasName2 {
		name2Len = len(name2)
		overhead++
	}
	avail := maxIdentifierLength - overhead
	for name1Len+name2Len > avail {
		if name1Len > name2Len {
			name1Len--
		} else {
			name2Len--
		}
	}
	out := clipName(name1, name1Len)
	if hasName2 {
		out += "_" + clipName(name2, name2Len)
	}
	return out + "_" + label
}

// chooseObjectName is PostgreSQL's ChooseRelationName / ChooseConstraintName loop: the first of
// makeObjectName(name1, name2, label), then with label1, label2, … — each re-truncated to fit —
// that taken reports free.
func chooseObjectName(name1, name2 string, hasName2 bool, label string, taken func(string) bool) string {
	name := makeObjectName(name1, name2, hasName2, label)
	for pass := 1; taken(name); pass++ {
		name = makeObjectName(name1, name2, hasName2, label+strconv.Itoa(pass))
	}
	return name
}

// nameAddition is PostgreSQL's ChooseIndexNameAddition (and the identical
// ChooseForeignKeyConstraintNameAddition): the column-name parts joined by "_", stopping once the
// result reaches 64 bytes (the makeObjectName cut does the rest).
func nameAddition(names []string) string {
	out := ""
	for _, name := range names {
		if out != "" {
			out += "_"
		}
		out += clipName(name, maxIdentifierLength)
		if len(out) > maxIdentifierLength {
			break
		}
	}
	return out
}

// indexColumnNames is PostgreSQL's ChooseIndexColumnNames de-duplication: a part repeating an
// earlier one becomes part1, part2, … (the part clipped so the suffixed form fits
// maxIdentifierLength) — (a, a) names a, a1.
func indexColumnNames(parts []string) []string {
	out := make([]string, 0, len(parts))
	for _, orig := range parts {
		cur := orig
		for i := 1; slices.Contains(out, cur); i++ {
			n := strconv.Itoa(i)
			cur = clipName(orig, maxIdentifierLength-len(n)) + n
		}
		out = append(out, cur)
	}
	return out
}

// columnNames is the exact names of table columns at the given ordinals, in order.
func columnNames(columns []catColumn, ordinals []int) []string {
	out := make([]string, len(ordinals))
	for i, o := range ordinals {
		out[i] = columns[o].Name
	}
	return out
}
