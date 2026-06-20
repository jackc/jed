package jed

// Collation: a hand-written Unicode Collation Algorithm (UTS #10) — a compiler (a canonical
// definition → jed's own compiled table) and an executor (table + string → a memcmp-ordered sort
// key), plus the portable .coll artifact codec. The cross-core contract for linguistic ordering
// (spec/design/collation.md §2/§6): both routines are hand-written per core (CLAUDE.md §5 forbids
// codegenning them) and byte-identical given identical input, pinned by spec/collation/vectors/.
// Byte formats: spec/collation/README.md (definition §1, table §2, artifact §3, sort key §4).
// Mirrors impl/rust/src/collation.rs and impl/ts/src/collation.ts.
//
// Slice 1b: host-free — CompileCollation (DUCET allkeys root + LDML tailoring), SortKey (the
// executor), and SaveCollation/OpenCollation (the artifact round-trip). No SQL surface, no
// persistence, no host seam. Only deterministic collations and non-ignorable variable weighting.

import (
	"fmt"
	"strconv"
	"strings"
)

// Ce is one collation element — a weight triple plus a flags byte. 7 bytes on disk
// (spec/collation/README.md §2): flags u8, l1 u16, l2 u16, l3 u16 (big-endian). A 0x0000 weight is
// ignorable at that level (skipped in the sort key, §4).
type Ce struct {
	Flags byte // bit0 = variable; bits 1-7 reserved (0). non-ignorable in slice 1 (§6).
	L1    uint16
	L2    uint16
	L3    uint16
}

const ceVariable byte = 0x01 // Flags bit0 — a variable collation element (DUCET marker '*').

func ce(l1, l2, l3 uint16) Ce { return Ce{0, l1, l2, l3} }

type singleEntry struct {
	cp  uint32
	ces []Ce
}
type contractionEntry struct {
	seq []uint32
	ces []Ce
}

// Collation is a compiled, fully-resolved collation: jed's own table plus its metadata,
// database-independent (collation.md §4). The arrays are kept sorted (the §2 contract) so the
// serialized bytes are deterministic.
type Collation struct {
	Name           string
	UnicodeVersion string // from the definition's @version record ("" if none)
	CldrVersion    string // "" — root-only / unset in slice 1b
	// Description is optional human-readable provenance; excluded from the content hash (§3).
	// Only ExtractHostCollation (a later slice) generates one; CompileCollation leaves it "".
	Description  string
	Singles      []singleEntry      // ascending by code point
	Contractions []contractionEntry // lexicographic by code-point sequence
}

// the dev tailoring weight allocator (spec/collation/README.md §1.2) — fixed constants so every
// core allocates identical weights from identical rules. A real ICU-faithful allocator replaces
// this with the version-pinned DUCET follow-on (collation.md §14).
const (
	baseL2       uint16 = 0x0020
	baseL3       uint16 = 0x0002
	primaryGap   uint16 = 0x0200
	secondaryGap uint16 = 0x0020
	tertiaryGap  uint16 = 0x0006
)

func featureErr(format string, a ...any) error {
	return NewError(FeatureNotSupported, fmt.Sprintf(format, a...))
}

func syntaxErr(format string, a ...any) error {
	return NewError(SyntaxError, fmt.Sprintf(format, a...))
}

func corruptErr(format string, a ...any) error {
	return NewError(DataCorrupted, fmt.Sprintf(format, a...))
}

// ============================================================================================
// Compiler: a canonical definition → a Collation.
// ============================================================================================

// CompileCollation parses a collation definition (spec/collation/README.md §1) and compiles it
// into a jed table. The definition is a single stream, line-dispatched: @… records, allkeys
// mapping lines (codepoints ; elements), and LDML rule lines (&anchor < x …). Host-free.
func CompileCollation(name, definition string) (*Collation, error) {
	unicodeVersion := ""
	// working map: code-point sequence → CEs, in insertion order.
	var keys [][]uint32
	cesByKey := map[string][]Ce{}

	setMapping := func(seq []uint32, ces []Ce, replace bool) error {
		k := seqKey(seq)
		if _, ok := cesByKey[k]; ok {
			if !replace {
				return syntaxErr("collation: duplicate mapping for %v", seq)
			}
			cesByKey[k] = ces
			return nil
		}
		keys = append(keys, seq)
		cesByKey[k] = ces
		return nil
	}

	for _, raw := range strings.Split(definition, "\n") {
		line := strings.TrimSpace(stripLineComment(raw))
		if line == "" {
			continue
		}
		if strings.HasPrefix(line, "@") {
			fields := strings.Fields(line[1:])
			if len(fields) >= 2 && fields[0] == "version" {
				unicodeVersion = fields[1]
			}
			// other records ignored in slice 1b.
			continue
		}
		if strings.HasPrefix(line, "&") {
			if err := applyTailoring(keys, cesByKey, setMapping, line); err != nil {
				return nil, err
			}
			continue
		}
		if err := parseMapping(setMapping, line); err != nil {
			return nil, err
		}
	}

	var singles []singleEntry
	var contractions []contractionEntry
	for _, seq := range keys {
		ces := cesByKey[seqKey(seq)]
		if len(seq) == 1 {
			singles = append(singles, singleEntry{seq[0], ces})
		} else {
			contractions = append(contractions, contractionEntry{seq, ces})
		}
	}
	sortSingles(singles)
	sortContractions(contractions)

	return &Collation{
		Name:           name,
		UnicodeVersion: unicodeVersion,
		Singles:        singles,
		Contractions:   contractions,
	}, nil
}

func seqKey(seq []uint32) string {
	var b strings.Builder
	for _, c := range seq {
		fmt.Fprintf(&b, "%d,", c)
	}
	return b.String()
}

func stripLineComment(line string) string {
	if i := strings.IndexByte(line, '#'); i >= 0 {
		return line[:i]
	}
	return line
}

type setMappingFn func(seq []uint32, ces []Ce, replace bool) error

func parseMapping(set setMappingFn, line string) error {
	i := strings.IndexByte(line, ';')
	if i < 0 {
		return syntaxErr("collation: mapping line has no ';': %s", line)
	}
	var seq []uint32
	for _, tok := range strings.Fields(line[:i]) {
		cp, err := parseHex(tok)
		if err != nil {
			return err
		}
		seq = append(seq, cp)
	}
	if len(seq) == 0 {
		return syntaxErr("collation: mapping with no code point: %s", line)
	}
	ces, err := parseElements(strings.TrimSpace(line[i+1:]))
	if err != nil {
		return err
	}
	if len(ces) == 0 {
		return syntaxErr("collation: mapping with no element: %s", line)
	}
	return set(seq, ces, false)
}

// parseElements parses [*0209.0020.0002][.0000.0047.0002]… into collation elements.
func parseElements(s string) ([]Ce, error) {
	var ces []Ce
	i := 0
	for i < len(s) {
		if s[i] == ' ' || s[i] == '\t' {
			i++
			continue
		}
		if s[i] != '[' {
			return nil, syntaxErr("collation: expected '[' in elements: %s", s)
		}
		end := strings.IndexByte(s[i:], ']')
		if end < 0 {
			return nil, syntaxErr("collation: unterminated element: %s", s)
		}
		end += i
		inner := s[i+1 : end]
		if inner == "" {
			return nil, syntaxErr("collation: empty element: %s", s)
		}
		var flags byte
		switch inner[0] {
		case '.':
		case '*':
			flags |= ceVariable
		default:
			return nil, syntaxErr("collation: bad element marker: %s", inner)
		}
		parts := strings.Split(inner[1:], ".")
		l1, err := parseHex16(at(parts, 0))
		if err != nil {
			return nil, err
		}
		l2, err := parseHex16(at(parts, 1))
		if err != nil {
			return nil, err
		}
		l3, err := parseHex16(at(parts, 2))
		if err != nil {
			return nil, err
		}
		ces = append(ces, Ce{flags, l1, l2, l3})
		i = end + 1
	}
	return ces, nil
}

func at(parts []string, i int) string {
	if i < len(parts) {
		return parts[i]
	}
	return ""
}

// --- LDML tailoring ---------------------------------------------------------------------------

type rel int

const (
	relPrimary rel = iota
	relSecondary
	relTertiary
	relIdentical
)

type tok struct {
	isOp bool
	cp   uint32
	op   rel
}

// applyTailoring applies one LDML rule line: &anchor REL target (REL target)*. Single-character
// anchor/targets only in slice 1b.
func applyTailoring(keys [][]uint32, cesByKey map[string][]Ce, set setMappingFn, line string) error {
	body := strings.TrimSpace(line[1:])
	toks, err := tokenizeRule(body)
	if err != nil {
		return err
	}
	if len(toks) == 0 || toks[0].isOp {
		return syntaxErr("collation: rule must start with an anchor: %s", line)
	}
	cur, ok := singleCe(cesByKey, toks[0].cp)
	if !ok {
		return syntaxErr("collation: rule anchor U+%04X not a single element", toks[0].cp)
	}
	i := 1
	for i < len(toks) {
		if !toks[i].isOp {
			return syntaxErr("collation: expected a relation operator: %s", line)
		}
		op := toks[i].op
		i++
		if i >= len(toks) || toks[i].isOp {
			return syntaxErr("collation: relation needs a target: %s", line)
		}
		target := toks[i].cp
		i++
		newCe, err := allocAfter(keys, cesByKey, cur, op)
		if err != nil {
			return err
		}
		if err := set([]uint32{target}, []Ce{newCe}, true); err != nil {
			return err
		}
		cur = newCe
	}
	return nil
}

func tokenizeRule(s string) ([]tok, error) {
	var out []tok
	runes := []rune(s)
	i := 0
	for i < len(runes) {
		c := runes[i]
		switch {
		case c == ' ' || c == '\t':
			i++
		case c == '<':
			n := 0
			for i < len(runes) && runes[i] == '<' {
				n++
				i++
			}
			switch n {
			case 1:
				out = append(out, tok{isOp: true, op: relPrimary})
			case 2:
				out = append(out, tok{isOp: true, op: relSecondary})
			case 3:
				out = append(out, tok{isOp: true, op: relTertiary})
			default:
				return nil, syntaxErr("collation: '<<<<' (quaternary) not supported")
			}
		case c == '=':
			out = append(out, tok{isOp: true, op: relIdentical})
			i++
		default:
			out = append(out, tok{cp: uint32(c)})
			i++
		}
	}
	return out, nil
}

// singleCe returns the CE of a single-code-point mapping with exactly one element.
func singleCe(cesByKey map[string][]Ce, cp uint32) (Ce, bool) {
	ces, ok := cesByKey[seqKey([]uint32{cp})]
	if !ok || len(ces) != 1 {
		return Ce{}, false
	}
	return ces[0], true
}

// allocAfter allocates a fresh CE placed after cur at the given relation level (the dev allocator).
func allocAfter(keys [][]uint32, cesByKey map[string][]Ce, cur Ce, r rel) (Ce, error) {
	switch r {
	case relIdentical:
		return cur, nil
	case relPrimary:
		succ, has := minWeightAbove(keys, cesByKey, func(c Ce) (uint16, bool) {
			return c.L1, c.L1 > cur.L1
		})
		l1, err := freshGap(cur.L1, succ, has, primaryGap)
		if err != nil {
			return Ce{}, err
		}
		return ce(l1, baseL2, baseL3), nil
	case relSecondary:
		succ, has := minWeightAbove(keys, cesByKey, func(c Ce) (uint16, bool) {
			return c.L2, c.L1 == cur.L1 && c.L2 > cur.L2
		})
		l2, err := freshGap(cur.L2, succ, has, secondaryGap)
		if err != nil {
			return Ce{}, err
		}
		return ce(cur.L1, l2, baseL3), nil
	default: // relTertiary
		succ, has := minWeightAbove(keys, cesByKey, func(c Ce) (uint16, bool) {
			return c.L3, c.L1 == cur.L1 && c.L2 == cur.L2 && c.L3 > cur.L3
		})
		l3, err := freshGap(cur.L3, succ, has, tertiaryGap)
		if err != nil {
			return Ce{}, err
		}
		return ce(cur.L1, cur.L2, l3), nil
	}
}

// minWeightAbove returns the smallest weight matching pred across every CE in the table.
func minWeightAbove(keys [][]uint32, cesByKey map[string][]Ce, pred func(Ce) (uint16, bool)) (uint16, bool) {
	var best uint16
	has := false
	for _, seq := range keys {
		for _, c := range cesByKey[seqKey(seq)] {
			if w, ok := pred(c); ok {
				if !has || w < best {
					best = w
					has = true
				}
			}
		}
	}
	return best, has
}

// freshGap: the midpoint to succ if one exists (needs room ≥ 2), else lo + gap (append).
func freshGap(lo, succ uint16, hasSucc bool, gap uint16) (uint16, error) {
	if hasSucc {
		if succ-lo < 2 {
			return 0, featureErr("collation: tailoring weight space exhausted (dense-insertion allocator deferred)")
		}
		return lo + (succ-lo)/2, nil
	}
	if uint32(lo)+uint32(gap) > 0xFFFF {
		return 0, featureErr("collation: tailoring weight overflow (allocator deferred)")
	}
	return lo + gap, nil
}

func parseHex(s string) (uint32, error) {
	v, err := strconv.ParseUint(strings.TrimSpace(s), 16, 32)
	if err != nil {
		return 0, syntaxErr("collation: bad code point hex: %q", s)
	}
	return uint32(v), nil
}

func parseHex16(s string) (uint16, error) {
	v, err := strconv.ParseUint(strings.TrimSpace(s), 16, 16)
	if err != nil {
		return 0, syntaxErr("collation: bad weight hex: %q", s)
	}
	return uint16(v), nil
}

// ============================================================================================
// Executor: (Collation, string) → sort key (spec/collation/README.md §4).
// ============================================================================================

// SortKey is the byte string whose memcmp order equals the collation's logical order:
// L1-weights ‖ 0x0000 ‖ L2-weights ‖ 0x0000 ‖ L3-weights ‖ 0x0000 ‖ Ckey(original).
func SortKey(coll *Collation, s string) ([]byte, error) {
	var cps []uint32
	for _, r := range s {
		cps = append(cps, uint32(r))
	}
	ces, err := collationElements(coll, cps)
	if err != nil {
		return nil, err
	}

	var key []byte
	for _, c := range ces {
		if c.L1 != 0 {
			key = appendU16(key, c.L1)
		}
	}
	key = append(key, 0, 0)
	for _, c := range ces {
		if c.L2 != 0 {
			key = appendU16(key, c.L2)
		}
	}
	key = append(key, 0, 0)
	for _, c := range ces {
		if c.L3 != 0 {
			key = appendU16(key, c.L3)
		}
	}
	key = append(key, 0, 0)
	// identical level: the §2.4 C-key of the original UTF-8 string.
	key = append(key, EncodeTerminated([]byte(s))...)
	return key, nil
}

func collationElements(coll *Collation, cps []uint32) ([]Ce, error) {
	maxContraction := 0
	for _, c := range coll.Contractions {
		if len(c.seq) > maxContraction {
			maxContraction = len(c.seq)
		}
	}
	var out []Ce
	i := 0
	for i < len(cps) {
		matched := false
		clen := maxContraction
		if rem := len(cps) - i; clen > rem {
			clen = rem
		}
		for clen >= 2 {
			if ces, ok := lookupContraction(coll, cps[i:i+clen]); ok {
				out = append(out, ces...)
				i += clen
				matched = true
				break
			}
			clen--
		}
		if matched {
			continue
		}
		if ces, ok := lookupSingle(coll, cps[i]); ok {
			out = append(out, ces...)
			i++
			continue
		}
		return nil, featureErr("collation: code point U+%04X has no mapping (implicit weights deferred)", cps[i])
	}
	return out, nil
}

func lookupSingle(coll *Collation, cp uint32) ([]Ce, bool) {
	for _, s := range coll.Singles {
		if s.cp == cp {
			return s.ces, true
		}
	}
	return nil, false
}

func lookupContraction(coll *Collation, seq []uint32) ([]Ce, bool) {
	for _, c := range coll.Contractions {
		if equalSeq(c.seq, seq) {
			return c.ces, true
		}
	}
	return nil, false
}

func equalSeq(a, b []uint32) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// (appendU16 / appendU32 are shared helpers in format.go.)

// ============================================================================================
// Compiled-table bytes (spec/collation/README.md §2) and the .coll artifact (§3).
// ============================================================================================

// SerializeTable serializes the compiled table (§2) — the bytes the content hash covers.
func SerializeTable(coll *Collation) []byte {
	var out []byte
	out = append(out, 1) // layout_version
	out = appendU32(out, uint32(len(coll.Singles)))
	out = appendU32(out, uint32(len(coll.Contractions)))
	for _, s := range coll.Singles {
		out = appendU32(out, s.cp)
		out = append(out, byte(len(s.ces)))
		for _, c := range s.ces {
			out = pushCe(out, c)
		}
	}
	for _, c := range coll.Contractions {
		out = append(out, byte(len(c.seq)))
		for _, cp := range c.seq {
			out = appendU32(out, cp)
		}
		out = append(out, byte(len(c.ces)))
		for _, e := range c.ces {
			out = pushCe(out, e)
		}
	}
	return out
}

func pushCe(out []byte, c Ce) []byte {
	out = append(out, c.Flags)
	out = appendU16(out, c.L1)
	out = appendU16(out, c.L2)
	out = appendU16(out, c.L3)
	return out
}

// SaveCollation writes the portable .coll artifact (§3): magic + metadata + provenance + CRC-32 +
// the LZ4-compressed table. OpenCollation is its exact inverse; the round-trip is byte-identical
// on every core (collation.md §10).
func SaveCollation(coll *Collation) []byte {
	table := SerializeTable(coll)
	hash := crc32IEEE(table)
	comp := lz4Compress(table)

	var out []byte
	out = append(out, 'J', 'C', 'O', 'L', 'L', 0) // 6-byte magic
	out = appendU16(out, 1)                       // format_version
	out = pushStr(out, coll.Name)
	out = pushStr(out, coll.UnicodeVersion)
	out = pushStr(out, coll.CldrVersion)
	out = pushStr(out, coll.Description)
	out = appendU32(out, hash)
	out = appendU32(out, uint32(len(table)))
	out = appendU32(out, uint32(len(comp)))
	out = append(out, comp...)
	return out
}

func pushStr(out []byte, s string) []byte {
	out = appendU16(out, uint16(len(s)))
	return append(out, s...)
}

// OpenCollation reads a .coll artifact (§3) back into a Collation. Verifies the magic, the format
// version, and the content hash; a malformed or tampered artifact is XX001 (data_corrupted).
func OpenCollation(bytes []byte) (*Collation, error) {
	r := &reader{b: bytes}
	magic, err := r.take(6)
	if err != nil {
		return nil, err
	}
	if string(magic) != "JCOLL\x00" {
		return nil, corruptErr("collation: bad artifact magic")
	}
	fmtVer, err := r.u16()
	if err != nil {
		return nil, err
	}
	if fmtVer != 1 {
		return nil, corruptErr("collation: unsupported artifact format_version %d", fmtVer)
	}
	name, err := r.str()
	if err != nil {
		return nil, err
	}
	unicodeVersion, err := r.str()
	if err != nil {
		return nil, err
	}
	cldrVersion, err := r.str()
	if err != nil {
		return nil, err
	}
	description, err := r.str()
	if err != nil {
		return nil, err
	}
	hash, err := r.u32()
	if err != nil {
		return nil, err
	}
	rawLen, err := r.u32()
	if err != nil {
		return nil, err
	}
	compLen, err := r.u32()
	if err != nil {
		return nil, err
	}
	comp, err := r.take(int(compLen))
	if err != nil {
		return nil, err
	}
	if r.i != len(r.b) {
		return nil, corruptErr("collation: trailing bytes after artifact")
	}
	table, err := lz4Decompress(comp, int(rawLen))
	if err != nil {
		return nil, err
	}
	if crc32IEEE(table) != hash {
		return nil, corruptErr("collation: artifact content hash mismatch")
	}
	singles, contractions, err := deserializeTable(table)
	if err != nil {
		return nil, err
	}
	return &Collation{
		Name:           name,
		UnicodeVersion: unicodeVersion,
		CldrVersion:    cldrVersion,
		Description:    description,
		Singles:        singles,
		Contractions:   contractions,
	}, nil
}

func deserializeTable(table []byte) ([]singleEntry, []contractionEntry, error) {
	r := &reader{b: table}
	layout, err := r.u8()
	if err != nil {
		return nil, nil, err
	}
	if layout != 1 {
		return nil, nil, corruptErr("collation: unsupported table layout_version %d", layout)
	}
	numSingles, err := r.u32()
	if err != nil {
		return nil, nil, err
	}
	numContractions, err := r.u32()
	if err != nil {
		return nil, nil, err
	}
	var singles []singleEntry // nil-when-empty, matching the compiler (for DeepEqual parity)
	for n := uint32(0); n < numSingles; n++ {
		cp, err := r.u32()
		if err != nil {
			return nil, nil, err
		}
		ces, err := r.ces()
		if err != nil {
			return nil, nil, err
		}
		singles = append(singles, singleEntry{cp, ces})
	}
	var contractions []contractionEntry // nil-when-empty, matching the compiler
	for n := uint32(0); n < numContractions; n++ {
		seqLen, err := r.u8()
		if err != nil {
			return nil, nil, err
		}
		seq := make([]uint32, 0, seqLen)
		for j := byte(0); j < seqLen; j++ {
			cp, err := r.u32()
			if err != nil {
				return nil, nil, err
			}
			seq = append(seq, cp)
		}
		ces, err := r.ces()
		if err != nil {
			return nil, nil, err
		}
		contractions = append(contractions, contractionEntry{seq, ces})
	}
	if r.i != len(r.b) {
		return nil, nil, corruptErr("collation: trailing bytes after table")
	}
	return singles, contractions, nil
}

type reader struct {
	b []byte
	i int
}

func (r *reader) take(n int) ([]byte, error) {
	if r.i+n > len(r.b) {
		return nil, corruptErr("collation: artifact truncated")
	}
	s := r.b[r.i : r.i+n]
	r.i += n
	return s, nil
}

func (r *reader) u8() (byte, error) {
	s, err := r.take(1)
	if err != nil {
		return 0, err
	}
	return s[0], nil
}

func (r *reader) u16() (uint16, error) {
	s, err := r.take(2)
	if err != nil {
		return 0, err
	}
	return uint16(s[0])<<8 | uint16(s[1]), nil
}

func (r *reader) u32() (uint32, error) {
	s, err := r.take(4)
	if err != nil {
		return 0, err
	}
	return uint32(s[0])<<24 | uint32(s[1])<<16 | uint32(s[2])<<8 | uint32(s[3]), nil
}

func (r *reader) ces() ([]Ce, error) {
	n, err := r.u8()
	if err != nil {
		return nil, err
	}
	ces := make([]Ce, 0, n)
	for j := byte(0); j < n; j++ {
		flags, err := r.u8()
		if err != nil {
			return nil, err
		}
		l1, err := r.u16()
		if err != nil {
			return nil, err
		}
		l2, err := r.u16()
		if err != nil {
			return nil, err
		}
		l3, err := r.u16()
		if err != nil {
			return nil, err
		}
		ces = append(ces, Ce{flags, l1, l2, l3})
	}
	return ces, nil
}

func (r *reader) str() (string, error) {
	n, err := r.u16()
	if err != nil {
		return "", err
	}
	s, err := r.take(int(n))
	if err != nil {
		return "", err
	}
	return string(s), nil
}

// --- sorting (insertion sort; the dev tables are tiny — a real index/lookup is a follow-on) ---

func sortSingles(s []singleEntry) {
	for i := 1; i < len(s); i++ {
		for j := i; j > 0 && s[j-1].cp > s[j].cp; j-- {
			s[j-1], s[j] = s[j], s[j-1]
		}
	}
}

func sortContractions(c []contractionEntry) {
	for i := 1; i < len(c); i++ {
		for j := i; j > 0 && seqLess(c[j].seq, c[j-1].seq); j-- {
			c[j-1], c[j] = c[j], c[j-1]
		}
	}
}

func seqLess(a, b []uint32) bool {
	for i := 0; i < len(a) && i < len(b); i++ {
		if a[i] != b[i] {
			return a[i] < b[i]
		}
	}
	return len(a) < len(b)
}
