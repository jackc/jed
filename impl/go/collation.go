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
	"sort"
	"strconv"
	"strings"
	"sync"
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
	return serializeEntries(coll.Singles, coll.Contractions)
}

// serializeEntries writes the §2 table-entry bytes — shared by the .coll artifact (a full table)
// and the JUCD bundle's root / tailoring sections (a full table or a sparse override, README §2/§5).
func serializeEntries(singles []singleEntry, contractions []contractionEntry) []byte {
	var out []byte
	out = append(out, 1) // layout_version
	out = appendU32(out, uint32(len(singles)))
	out = appendU32(out, uint32(len(contractions)))
	for _, s := range singles {
		out = appendU32(out, s.cp)
		out = append(out, byte(len(s.ces)))
		for _, c := range s.ces {
			out = pushCe(out, c)
		}
	}
	for _, c := range contractions {
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

// --- The engine-global loaded collation set (spec/design/collation.md §4/§9) --------------------
//
// The bare binary carries NO Unicode data — no embedded .coll, no casing tables (§9/§16, the SQLite
// model). All collations arrive at runtime: a host hands the engine a JUCD bundle's bytes via
// LoadUnicodeData (db.LoadUnicodeData), the engine merges its root + per-locale deltas (§5.1) and
// adds the resulting collations here. This set is PROCESS-GLOBAL — a property of the running engine,
// not of one Database handle (the spec's "loaded set available to any database on this handle",
// §4.2). Global is what lets a file REFERENCING a collation be opened after the bundle is loaded:
// open resolves the referenced table from here (format.go), and open mints the handle, so the data
// cannot live on the handle. "C" is never here (table-free, built in).
//
// The bytes are jed's OWN pinned tables (byte-identical across cores, §9/§10), so loading restores
// no nondeterminism — a use stays pure regardless of where the host sourced the bytes (file / fetch
// / compiled-in asset). The real version-pinned production bundle is
// spec/collation/fixtures/unicode.jucd (unicode = the CLDR-DUCET root, es = root + the Spanish ñ
// tailoring); the dev-* fixtures are not part of it (they only drive the cross-core vectors).

var (
	loadedMu   sync.RWMutex
	loadedColl = map[string]*Collation{}
)

// LoadUnicodeData loads a JUCD Unicode-data bundle's collations into the engine-global loaded set
// (spec/design/collation.md §4/§9): parse the bundle, merge the root + each per-locale delta (§5.1),
// and register every collation by name. ADDITIVE — a name already present is NOT replaced (the first
// bundle to provide it wins; resolution is by name in load order, §4.2), so re-loading the same
// bundle is an idempotent no-op. The property/casing section is parsed and validated but not yet
// consumed (casing lands in slice 3e). A malformed bundle is XX001 (data_corrupted).
//
// This is the engine primitive behind db.LoadUnicodeData. Because the set is process-global it may
// be called BEFORE opening any file (which is required: opening a file that references a collation
// resolves its table from this set). Privileged host op — the engine reads no file path and reaches
// no host data (§11); the host sources the bytes.
func LoadUnicodeData(data []byte) error {
	bundle, err := OpenBundle(data)
	if err != nil {
		return err
	}
	colls, _, err := LoadBundle(bundle)
	if err != nil {
		return err
	}
	loadedMu.Lock()
	defer loadedMu.Unlock()
	for _, c := range colls {
		if _, ok := loadedColl[c.Name]; !ok {
			loadedColl[c.Name] = c
		}
	}
	return nil
}

// LoadedCollation looks up a collation in the engine-global LOADED set by its exact (case-sensitive)
// name (spec/design/collation.md §4/§9). nil ⇒ no loaded bundle provides it. "C" is never here
// (table-free, built in). The resolver consults the database's referenced collations first, then
// this set.
func LoadedCollation(name string) *Collation {
	loadedMu.RLock()
	defer loadedMu.RUnlock()
	return loadedColl[name]
}

// loadedCollationTables returns every loaded collation, ascending by name — a deterministic order
// with no hash-iteration leak (CLAUDE.md §8). The raw tables; the public CollationInfo view is the
// Database.LoadedCollations method (executor.go).
func loadedCollationTables() []*Collation {
	loadedMu.RLock()
	defer loadedMu.RUnlock()
	out := make([]*Collation, 0, len(loadedColl))
	for _, c := range loadedColl {
		out = append(out, c)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
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

// cps reads a u8-length-prefixed run of u32 code points (the JUCD property section, README §5).
func (r *reader) cps() ([]uint32, error) {
	n, err := r.u8()
	if err != nil {
		return nil, err
	}
	cps := make([]uint32, 0, n)
	for j := byte(0); j < n; j++ {
		cp, err := r.u32()
		if err != nil {
			return nil, err
		}
		cps = append(cps, cp)
	}
	return cps, nil
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

// ============================================================================================
// The JUCD Unicode-data bundle (spec/collation/README.md §5) — the host-loaded container.
// A manifest-indexed container of sections: the Unicode property/casing tables, the shared DUCET
// root (a full §2 table, stored once, and itself a usable collation under its name), and per-locale
// tailoring sections (sparse overrides merged onto the root at load — §5.1). Mirror of
// impl/rust/src/collation.rs; byte-identical by construction (CLAUDE.md §8).
// ============================================================================================

// SimpleCase is a simple 1:1 case mapping (a field equal to Cp is the identity mapping).
type SimpleCase struct{ Cp, Upper, Lower, Title uint32 }

// SpecialCasing is a full (multi-code-point) case mapping (README §5). Conditional/locale context
// is reserved.
type SpecialCasing struct {
	Cp    uint32
	Upper []uint32
	Lower []uint32
	Title []uint32
}

// PropertyTable is the Unicode property/casing data (README §5). First cut: case mappings only
// (normalization is a reserved later sub-table). Simple is ascending by code point; Special likewise.
type PropertyTable struct {
	Simple  []SimpleCase
	Special []SpecialCasing
}

// bundleEntries is a §2 table — a full table for a root, a sparse override for a tailoring — plus
// the collation name the manifest records.
type bundleEntries struct {
	name         string
	singles      []singleEntry
	contractions []contractionEntry
}

const (
	sectionProperty  byte = 0
	sectionRoot      byte = 1
	sectionTailoring byte = 2
)

// Section is one bundle section: the property tables (Kind 0), the shared root (Kind 1), or a
// per-locale override (Kind 2). Exactly one of Property / Entries is set, per Kind.
type Section struct {
	Kind     byte
	Property *PropertyTable
	Entries  *bundleEntries
}

// Bundle is a parsed JUCD bundle (README §5): the shared header version axis + its sections.
type Bundle struct {
	UnicodeVersion string
	CldrVersion    string
	Description    string
	Sections       []Section
}

var bundleMagic = []byte{'J', 'U', 'C', 'D', 0, 0}

// SaveBundle serializes a JUCD bundle (README §5): header, manifest (a TOC with per-section
// offsets), the LZ4-compressed section bodies, and a trailing CRC-32 over everything before it.
func SaveBundle(b *Bundle) []byte {
	type packed struct {
		kind   byte
		name   string
		hash   uint32
		rawLen uint32
		comp   []byte
	}
	ps := make([]packed, 0, len(b.Sections))
	for _, s := range b.Sections {
		var kind byte
		var name string
		var raw []byte
		switch s.Kind {
		case sectionProperty:
			kind, name, raw = sectionProperty, "", serializeProperty(s.Property)
		case sectionRoot:
			kind, name, raw = sectionRoot, s.Entries.name, serializeEntries(s.Entries.singles, s.Entries.contractions)
		case sectionTailoring:
			kind, name, raw = sectionTailoring, s.Entries.name, serializeEntries(s.Entries.singles, s.Entries.contractions)
		}
		ps = append(ps, packed{kind, name, crc32IEEE(raw), uint32(len(raw)), lz4Compress(raw)})
	}

	var header []byte
	header = append(header, bundleMagic...)
	header = appendU16(header, 1) // format_version
	header = pushStr(header, b.UnicodeVersion)
	header = pushStr(header, b.CldrVersion)
	header = pushStr(header, b.Description)

	// Manifest length is fixed once the names are known (per entry: kind 1 + name 2+len + hash 4 +
	// raw_len 4 + comp_len 4 + offset 4), so the body offsets can be computed up front.
	manifestLen := 2
	for _, p := range ps {
		manifestLen += 1 + 2 + len(p.name) + 4 + 4 + 4 + 4
	}
	bodyStart := len(header) + manifestLen

	manifest := make([]byte, 0, manifestLen)
	manifest = appendU16(manifest, uint16(len(ps)))
	off := bodyStart
	for _, p := range ps {
		manifest = append(manifest, p.kind)
		manifest = pushStr(manifest, p.name)
		manifest = appendU32(manifest, p.hash)
		manifest = appendU32(manifest, p.rawLen)
		manifest = appendU32(manifest, uint32(len(p.comp)))
		manifest = appendU32(manifest, uint32(off))
		off += len(p.comp)
	}

	out := append(header, manifest...)
	for _, p := range ps {
		out = append(out, p.comp...)
	}
	out = appendU32(out, crc32IEEE(out))
	return out
}

// OpenBundle reads a JUCD bundle (README §5). Verifies the trailing CRC, the magic, the format
// version, and each section's content hash; a malformed bundle is XX001 (data_corrupted).
func OpenBundle(data []byte) (*Bundle, error) {
	if len(data) < 4 {
		return nil, corruptErr("bundle: truncated")
	}
	body := data[:len(data)-4]
	want := uint32(data[len(data)-4])<<24 | uint32(data[len(data)-3])<<16 | uint32(data[len(data)-2])<<8 | uint32(data[len(data)-1])
	if crc32IEEE(body) != want {
		return nil, corruptErr("bundle: trailer checksum mismatch")
	}

	r := &reader{b: data}
	magic, err := r.take(6)
	if err != nil {
		return nil, err
	}
	if string(magic) != "JUCD\x00\x00" {
		return nil, corruptErr("bundle: bad magic")
	}
	fmtVer, err := r.u16()
	if err != nil {
		return nil, err
	}
	if fmtVer != 1 {
		return nil, corruptErr("bundle: unsupported format_version %d", fmtVer)
	}
	uv, err := r.str()
	if err != nil {
		return nil, err
	}
	cldr, err := r.str()
	if err != nil {
		return nil, err
	}
	desc, err := r.str()
	if err != nil {
		return nil, err
	}
	count, err := r.u16()
	if err != nil {
		return nil, err
	}

	type entry struct {
		kind                    byte
		name                    string
		hash                    uint32
		rawLen, compLen, offset int
	}
	es := make([]entry, 0, count)
	for i := uint16(0); i < count; i++ {
		kind, err := r.u8()
		if err != nil {
			return nil, err
		}
		name, err := r.str()
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
		offset, err := r.u32()
		if err != nil {
			return nil, err
		}
		es = append(es, entry{kind, name, hash, int(rawLen), int(compLen), int(offset)})
	}

	sections := make([]Section, 0, count)
	for _, e := range es {
		if e.offset > len(body) || e.offset+e.compLen > len(body) {
			return nil, corruptErr("bundle: section body out of range")
		}
		raw, err := lz4Decompress(data[e.offset:e.offset+e.compLen], e.rawLen)
		if err != nil {
			return nil, err
		}
		if crc32IEEE(raw) != e.hash {
			return nil, corruptErr("bundle: section content hash mismatch")
		}
		switch e.kind {
		case sectionProperty:
			p, err := deserializeProperty(raw)
			if err != nil {
				return nil, err
			}
			sections = append(sections, Section{Kind: sectionProperty, Property: p})
		case sectionRoot, sectionTailoring:
			singles, contractions, err := deserializeTable(raw)
			if err != nil {
				return nil, err
			}
			sections = append(sections, Section{Kind: e.kind, Entries: &bundleEntries{e.name, singles, contractions}})
		default:
			return nil, corruptErr("bundle: unknown section kind %d", e.kind)
		}
	}
	return &Bundle{uv, cldr, desc, sections}, nil
}

// LoadBundle loads a bundle (README §5.1): the root section is a usable collation; each tailoring is
// merged onto the root (byte-identical to its fully-resolved .coll table). Every collation takes the
// bundle header's (unicode, cldr) version + description. Returns the collations and the optional
// property table.
func LoadBundle(b *Bundle) ([]*Collation, *PropertyTable, error) {
	var root *bundleEntries
	for i := range b.Sections {
		if b.Sections[i].Kind == sectionRoot {
			root = b.Sections[i].Entries
			break
		}
	}
	mk := func(name string, singles []singleEntry, contractions []contractionEntry) *Collation {
		return &Collation{
			Name: name, UnicodeVersion: b.UnicodeVersion, CldrVersion: b.CldrVersion,
			Description: b.Description, Singles: singles, Contractions: contractions,
		}
	}
	var colls []*Collation
	var property *PropertyTable
	if root != nil {
		colls = append(colls, mk(root.name, root.singles, root.contractions))
	}
	for i := range b.Sections {
		s := &b.Sections[i]
		switch s.Kind {
		case sectionProperty:
			property = s.Property
		case sectionTailoring:
			if root == nil {
				return nil, nil, corruptErr("bundle: tailoring without a root section")
			}
			singles, contractions := mergeOntoRoot(root, s.Entries)
			colls = append(colls, mk(s.Entries.name, singles, contractions))
		}
	}
	return colls, property, nil
}

// BuildBundle builds a JUCD bundle (README §5) from a root collation, per-locale tailorings (each
// diffed against the root into a sparse override), and an optional property table — the builder
// tool's core. The header (unicode, cldr) version is the root's.
func BuildBundle(root *Collation, tailorings []*Collation, property *PropertyTable, description string) *Bundle {
	var sections []Section
	if property != nil {
		sections = append(sections, Section{Kind: sectionProperty, Property: property})
	}
	sections = append(sections, Section{Kind: sectionRoot, Entries: &bundleEntries{root.Name, root.Singles, root.Contractions}})
	for _, t := range tailorings {
		singles, contractions := diffAgainstRoot(t, root)
		sections = append(sections, Section{Kind: sectionTailoring, Entries: &bundleEntries{t.Name, singles, contractions}})
	}
	return &Bundle{root.UnicodeVersion, root.CldrVersion, description, sections}
}

// mergeOntoRoot merges a tailoring's sparse override onto the root table (README §5.1): start from
// the root maps, replace-or-add each override by key, re-sort (ascending by code point / lexicographic
// by sequence — the §2 total order), so the result is byte-identical to the full .coll table.
func mergeOntoRoot(root, delta *bundleEntries) ([]singleEntry, []contractionEntry) {
	singleByCp := make(map[uint32][]Ce, len(root.singles)+len(delta.singles))
	for _, s := range root.singles {
		singleByCp[s.cp] = s.ces
	}
	for _, s := range delta.singles {
		singleByCp[s.cp] = s.ces
	}
	singles := make([]singleEntry, 0, len(singleByCp))
	for cp, ces := range singleByCp {
		singles = append(singles, singleEntry{cp, ces})
	}
	sort.Slice(singles, func(i, j int) bool { return singles[i].cp < singles[j].cp })

	contrByKey := make(map[string]contractionEntry, len(root.contractions)+len(delta.contractions))
	for _, c := range root.contractions {
		contrByKey[seqKey(c.seq)] = c
	}
	for _, c := range delta.contractions {
		contrByKey[seqKey(c.seq)] = c
	}
	contractions := make([]contractionEntry, 0, len(contrByKey))
	for _, c := range contrByKey {
		contractions = append(contractions, c)
	}
	sort.Slice(contractions, func(i, j int) bool { return seqLess(contractions[i].seq, contractions[j].seq) })
	return singles, contractions
}

// diffAgainstRoot is the sparse override (README §5): the full table's singles/contractions that the
// root lacks or maps differently. The current LDML subset only adds or replaces (no removals), so
// applying this back onto the root reproduces the full table (§5.1).
func diffAgainstRoot(full, root *Collation) ([]singleEntry, []contractionEntry) {
	rootSingles := make(map[uint32][]Ce, len(root.Singles))
	for _, s := range root.Singles {
		rootSingles[s.cp] = s.ces
	}
	var singles []singleEntry
	for _, s := range full.Singles {
		if rc, ok := rootSingles[s.cp]; !ok || !equalCes(rc, s.ces) {
			singles = append(singles, s)
		}
	}
	rootContr := make(map[string][]Ce, len(root.Contractions))
	for _, c := range root.Contractions {
		rootContr[seqKey(c.seq)] = c.ces
	}
	var contractions []contractionEntry
	for _, c := range full.Contractions {
		if rc, ok := rootContr[seqKey(c.seq)]; !ok || !equalCes(rc, c.ces) {
			contractions = append(contractions, c)
		}
	}
	return singles, contractions
}

func equalCes(a, b []Ce) bool {
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

func serializeProperty(p *PropertyTable) []byte {
	out := []byte{1} // layout_version
	out = appendU32(out, uint32(len(p.Simple)))
	for _, s := range p.Simple {
		out = appendU32(out, s.Cp)
		out = appendU32(out, s.Upper)
		out = appendU32(out, s.Lower)
		out = appendU32(out, s.Title)
	}
	out = appendU32(out, uint32(len(p.Special)))
	for _, sc := range p.Special {
		out = appendU32(out, sc.Cp)
		out = pushCps(out, sc.Upper)
		out = pushCps(out, sc.Lower)
		out = pushCps(out, sc.Title)
	}
	return out
}

func pushCps(out []byte, cps []uint32) []byte {
	out = append(out, byte(len(cps)))
	for _, cp := range cps {
		out = appendU32(out, cp)
	}
	return out
}

func deserializeProperty(raw []byte) (*PropertyTable, error) {
	r := &reader{b: raw}
	layout, err := r.u8()
	if err != nil {
		return nil, err
	}
	if layout != 1 {
		return nil, corruptErr("bundle: unsupported property layout_version %d", layout)
	}
	numSimple, err := r.u32()
	if err != nil {
		return nil, err
	}
	var simple []SimpleCase
	for n := uint32(0); n < numSimple; n++ {
		cp, err := r.u32()
		if err != nil {
			return nil, err
		}
		up, err := r.u32()
		if err != nil {
			return nil, err
		}
		lo, err := r.u32()
		if err != nil {
			return nil, err
		}
		ti, err := r.u32()
		if err != nil {
			return nil, err
		}
		simple = append(simple, SimpleCase{cp, up, lo, ti})
	}
	numSpecial, err := r.u32()
	if err != nil {
		return nil, err
	}
	var special []SpecialCasing
	for n := uint32(0); n < numSpecial; n++ {
		cp, err := r.u32()
		if err != nil {
			return nil, err
		}
		up, err := r.cps()
		if err != nil {
			return nil, err
		}
		lo, err := r.cps()
		if err != nil {
			return nil, err
		}
		ti, err := r.cps()
		if err != nil {
			return nil, err
		}
		special = append(special, SpecialCasing{cp, up, lo, ti})
	}
	if r.i != len(r.b) {
		return nil, corruptErr("bundle: trailing bytes after property table")
	}
	return &PropertyTable{simple, special}, nil
}
