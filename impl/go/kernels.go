package jed

import (
	"fmt"
	"math"
	"math/big"
	"strconv"
	"strings"
)

// Scalar value kernels: string/text builtins, numeric helpers, and literal parsing/coercion. This
// file holds the string/text function kernels (padChars/trimChars/translateChars/splitPart, the
// base64/hex encode+decode, quoteLiteral/quoteIdent, substr/left/right), the width_bucket and
// numeric/temporal arithmetic-result helpers, and the literal-to-value coercion path
// (coerceStringLiteral, parseIntLiteral/parseFloatLiteral/parseDecimalLiteral, promote). These are
// pure value→value kernels invoked from eval.go and the resolver.

// === string / text function kernels (spec/design/string-functions.md) ===============

// maxResultChars is the character-count cap for the result-amplifying string functions (lpad /
// rpad / repeat): PostgreSQL's MaxAllocSize (0x3FFFFFFF). A requested length above it traps 54000
// (program_limit_exceeded), bounding the allocation an untrusted query can request (CLAUDE.md §13).
const maxResultChars int64 = 0x3FFFFFFF

// padChars is lpad/rpad over CODE POINTS (string-functions.md §3): pad s to length characters with
// fill (cyclically), on the left if left else the right; a string longer than length is truncated
// to its first length characters; an empty fill cannot pad (returns the truncated string); a
// length ≤ 0 is empty. A length above maxResultChars traps 54000. Matches PostgreSQL's lpad/rpad.
func padChars(s string, length int64, fill string, left bool) (string, error) {
	if length > maxResultChars {
		return "", newError(ProgramLimitExceeded, "requested length too large")
	}
	if length <= 0 {
		return "", nil
	}
	runes := []rune(s)
	slen := int64(len(runes))
	if slen >= length {
		return string(runes[:length]), nil
	}
	frunes := []rune(fill)
	if len(frunes) == 0 {
		return s, nil
	}
	need := int(length - slen)
	flen := len(frunes)
	var b strings.Builder
	for i := 0; i < need; i++ {
		b.WriteRune(frunes[i%flen])
	}
	pad := b.String()
	if left {
		return pad + s, nil
	}
	return s + pad, nil
}

// trimChars is btrim/ltrim/rtrim over CODE POINTS (string-functions.md §3): remove from the chosen
// end(s) the longest run of characters each present in the set (a SET of code points, not a
// substring; default a single space). An empty set trims nothing. Matches PostgreSQL's *trim.
func trimChars(s, set string, doLeft, doRight bool) string {
	inSet := make(map[rune]struct{})
	for _, r := range set {
		inSet[r] = struct{}{}
	}
	runes := []rune(s)
	start, end := 0, len(runes)
	if doLeft {
		for start < end {
			if _, ok := inSet[runes[start]]; !ok {
				break
			}
			start++
		}
	}
	if doRight {
		for end > start {
			if _, ok := inSet[runes[end-1]]; !ok {
				break
			}
			end--
		}
	}
	return string(runes[start:end])
}

// translateChars is translate(s, from, to) over CODE POINTS (string-functions.md §3): each
// character of s that occurs in from is replaced by the character at the same position in to, or
// DELETED if to is shorter; a character's FIRST occurrence in from wins. Matches PostgreSQL.
func translateChars(s, from, to string) string {
	torunes := []rune(to)
	type repl struct {
		ch  rune
		del bool
	}
	m := make(map[rune]repl)
	for i, c := range []rune(from) {
		if _, ok := m[c]; ok {
			continue // first occurrence wins
		}
		if i < len(torunes) {
			m[c] = repl{ch: torunes[i]}
		} else {
			m[c] = repl{del: true}
		}
	}
	var b strings.Builder
	for _, c := range s {
		if r, ok := m[c]; ok {
			if !r.del {
				b.WriteRune(r.ch)
			}
		} else {
			b.WriteRune(c)
		}
	}
	return b.String()
}

// repeatText is repeat(s, n) (string-functions.md §3): concatenate s n times; n ≤ 0 is empty. The
// result's byte size is bounded at maxResultChars (PG's MaxAllocSize) — an over-large n·len(s) traps
// 54000 (program_limit_exceeded). Matches PostgreSQL's repeat.
func repeatText(s string, n int64) (string, error) {
	if n <= 0 || len(s) == 0 {
		return "", nil
	}
	if n > maxResultChars/int64(len(s)) {
		return "", newError(ProgramLimitExceeded, "requested length too large")
	}
	return strings.Repeat(s, int(n)), nil
}

// splitPart is split_part(s, delim, n) (string-functions.md §3): split s on the substring delim and
// return the n-th field (1-based; a negative n counts from the end). An out-of-range field is empty;
// n = 0 traps 22023. An EMPTY delim treats the whole string as one field (strings.Split would
// otherwise split into characters — a cross-core trap). Matches PostgreSQL's split_part.
func splitPart(s, delim string, n int64) (string, error) {
	if n == 0 {
		return "", newError(InvalidParameterValue, "field position must not be zero")
	}
	var fields []string
	if delim == "" {
		fields = []string{s}
	} else {
		fields = strings.Split(s, delim)
	}
	length := int64(len(fields))
	var idx int64
	if n > 0 {
		idx = n - 1
	} else {
		idx = length + n
	}
	if idx < 0 || idx >= length {
		return "", nil
	}
	return fields[idx], nil
}

// chrText is chr(n) (string-functions.md §3): the one-character string for the Unicode code point n.
// PostgreSQL's error split: a negative n traps 22023; 0, a value above U+10FFFF, and a UTF-16
// surrogate (U+D800..U+DFFF) trap 54000.
func chrText(n int64) (string, error) {
	if n < 0 {
		return "", newError(InvalidParameterValue, "character number must be positive")
	}
	if n == 0 {
		return "", newError(ProgramLimitExceeded, "null character not permitted")
	}
	if n > 0x10FFFF {
		return "", newError(ProgramLimitExceeded, fmt.Sprintf("requested character too large for encoding: %d", n))
	}
	if n >= 0xD800 && n <= 0xDFFF {
		return "", newError(ProgramLimitExceeded, fmt.Sprintf("requested character not valid for encoding: %d", n))
	}
	return string(rune(n)), nil
}

// base64Alphabet is the standard RFC 4648 base64 alphabet (string-functions.md §3, encode/decode).
const base64Alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

// encodeBytea is encode(bytes, format) (string-functions.md §3): render binary as text. hex = two
// lowercase hex digits per byte; base64 = RFC 4648 wrapped at 76 chars with \n (PostgreSQL's style);
// escape = printable bytes verbatim, 0x00 → \000, backslash doubled, high-bit bytes → \nnn octal. An
// unrecognized format traps 22023.
func encodeBytea(bytes []byte, format string) (string, error) {
	switch format {
	case "hex":
		var b strings.Builder
		for _, c := range bytes {
			fmt.Fprintf(&b, "%02x", c)
		}
		return b.String(), nil
	case "escape":
		var b strings.Builder
		for _, c := range bytes {
			switch {
			case c == 0x00:
				b.WriteString("\\000")
			case c == 0x5c:
				b.WriteString("\\\\")
			case c >= 0x80:
				fmt.Fprintf(&b, "\\%03o", c)
			default:
				b.WriteByte(c)
			}
		}
		return b.String(), nil
	case "base64":
		return base64EncodeWrapped(bytes), nil
	default:
		return "", newError(InvalidParameterValue, fmt.Sprintf("unrecognized encoding: %q", format))
	}
}

// base64EncodeWrapped is RFC 4648 base64 wrapped at 76 chars with \n (no trailing newline).
func base64EncodeWrapped(bytes []byte) string {
	var b64 []byte
	for i := 0; i < len(bytes); i += 3 {
		n := uint32(bytes[i]) << 16
		l := 1
		if i+1 < len(bytes) {
			n |= uint32(bytes[i+1]) << 8
			l = 2
		}
		if i+2 < len(bytes) {
			n |= uint32(bytes[i+2])
			l = 3
		}
		b64 = append(b64, base64Alphabet[(n>>18)&63], base64Alphabet[(n>>12)&63])
		if l > 1 {
			b64 = append(b64, base64Alphabet[(n>>6)&63])
		} else {
			b64 = append(b64, '=')
		}
		if l > 2 {
			b64 = append(b64, base64Alphabet[n&63])
		} else {
			b64 = append(b64, '=')
		}
	}
	var out strings.Builder
	for i, c := range b64 {
		if i > 0 && i%76 == 0 {
			out.WriteByte('\n')
		}
		out.WriteByte(c)
	}
	return out.String()
}

// quoteLiteralText is quote_literal(s) (string-functions.md §3): wrap s as a SQL string literal —
// single-quoted, each internal ' doubled; if s contains a backslash, each \ is doubled and the
// literal is E-prefixed (matching PostgreSQL). Shared by quote_literal and quote_nullable.
func quoteLiteralText(s string) string {
	hasBackslash := strings.ContainsRune(s, '\\')
	var inner strings.Builder
	for _, c := range s {
		switch c {
		case '\'':
			inner.WriteString("''")
		case '\\':
			inner.WriteString("\\\\")
		default:
			inner.WriteRune(c)
		}
	}
	if hasBackslash {
		return "E'" + inner.String() + "'"
	}
	return "'" + inner.String() + "'"
}

// quoteIdentText is quote_ident(s) (string-functions.md §3): wrap s as a SQL identifier — returned
// unchanged if it is already a safe unquoted identifier (^[a-z_][a-z0-9_]*$), else double-quoted with
// each internal " doubled. jed quotes by the LEXICAL pattern only — no reserved-keyword quoting (jed
// has no enumerated keyword set), a documented divergence from PostgreSQL.
func quoteIdentText(s string) string {
	safe := len(s) > 0
	for i := 0; i < len(s) && safe; i++ {
		b := s[i]
		lower := b == '_' || (b >= 'a' && b <= 'z')
		if i == 0 {
			safe = lower
		} else {
			safe = lower || (b >= '0' && b <= '9')
		}
	}
	if safe {
		return s
	}
	var out strings.Builder
	out.WriteByte('"')
	for _, c := range s {
		if c == '"' {
			out.WriteString("\"\"")
		} else {
			out.WriteRune(c)
		}
	}
	out.WriteByte('"')
	return out.String()
}

// decodeText is decode(s, format) (string-functions.md §3): the inverse of encode. hex and base64
// ignore whitespace; a malformed hex/base64 string traps 22023; a malformed escape sequence traps
// 22P02 (PostgreSQL's split). An unrecognized format traps 22023.
func decodeText(s, format string) ([]byte, error) {
	switch format {
	case "hex":
		return decodeHex([]byte(s))
	case "base64":
		return decodeBase64([]byte(s))
	case "escape":
		return decodeEscape([]byte(s))
	default:
		return nil, newError(InvalidParameterValue, fmt.Sprintf("unrecognized encoding: %q", format))
	}
}

func hexNibble(c byte) (byte, bool) {
	switch {
	case c >= '0' && c <= '9':
		return c - '0', true
	case c >= 'a' && c <= 'f':
		return c - 'a' + 10, true
	case c >= 'A' && c <= 'F':
		return c - 'A' + 10, true
	default:
		return 0, false
	}
}

func isASCIIWhitespace(c byte) bool {
	return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '\f'
}

// decodeHex: pairs of hex digits (case-insensitive); whitespace is ignored; a non-hex byte or an odd
// digit count traps 22023.
func decodeHex(bytes []byte) ([]byte, error) {
	var nibbles []byte
	for _, b := range bytes {
		if isASCIIWhitespace(b) {
			continue
		}
		v, ok := hexNibble(b)
		if !ok {
			return nil, newError(InvalidParameterValue, "invalid hexadecimal digit")
		}
		nibbles = append(nibbles, v)
	}
	if len(nibbles)%2 != 0 {
		return nil, newError(InvalidParameterValue, "invalid hexadecimal data: odd number of digits")
	}
	out := make([]byte, 0, len(nibbles)/2)
	for i := 0; i < len(nibbles); i += 2 {
		out = append(out, nibbles[i]<<4|nibbles[i+1])
	}
	return out, nil
}

// decodeBase64 (RFC 4648); whitespace is ignored; an out-of-alphabet byte (or data after the =
// padding) traps 22023. Bit-accumulation emits a byte per full 8 bits.
func decodeBase64(bytes []byte) ([]byte, error) {
	bad := func() error { return newError(InvalidParameterValue, "invalid base64 end sequence") }
	var out []byte
	var acc uint32
	nbits := 0
	padded := false
	for _, b := range bytes {
		if isASCIIWhitespace(b) {
			continue
		}
		if b == '=' {
			padded = true
			continue
		}
		if padded {
			return nil, bad()
		}
		var v byte
		switch {
		case b >= 'A' && b <= 'Z':
			v = b - 'A'
		case b >= 'a' && b <= 'z':
			v = b - 'a' + 26
		case b >= '0' && b <= '9':
			v = b - '0' + 52
		case b == '+':
			v = 62
		case b == '/':
			v = 63
		default:
			return nil, bad()
		}
		acc = acc<<6 | uint32(v)
		nbits += 6
		if nbits >= 8 {
			nbits -= 8
			out = append(out, byte(acc>>uint(nbits)))
		}
	}
	return out, nil
}

// decodeEscape (on the input's UTF-8 bytes): \\ → backslash, \nnn (exactly 3 octal digits ≤ 255) →
// that byte, any other byte → itself. A lone/short backslash or an octal > 255 traps 22P02.
func decodeEscape(bytes []byte) ([]byte, error) {
	bad := func() error { return newError(InvalidTextRepresentation, "invalid input syntax for type bytea") }
	oct := func(c byte) (byte, bool) {
		if c >= '0' && c <= '7' {
			return c - '0', true
		}
		return 0, false
	}
	var out []byte
	i := 0
	for i < len(bytes) {
		if bytes[i] != '\\' {
			out = append(out, bytes[i])
			i++
			continue
		}
		if i+1 < len(bytes) && bytes[i+1] == '\\' {
			out = append(out, '\\')
			i += 2
		} else if i+3 < len(bytes) {
			a, aok := oct(bytes[i+1])
			b, bok := oct(bytes[i+2])
			c, cok := oct(bytes[i+3])
			if !aok || !bok || !cok {
				return nil, bad()
			}
			v := uint16(a)*64 + uint16(b)*8 + uint16(c)
			if v > 255 {
				return nil, bad()
			}
			out = append(out, byte(v))
			i += 4
		} else {
			return nil, bad()
		}
	}
	return out, nil
}

// initcapASCII is initcap(s) (string-functions.md §3): uppercase the first character of each word
// and lowercase the rest, where a word is a maximal run of ASCII alphanumerics. jed classifies word
// boundaries by ASCII alphanumerics and folds ASCII case only — deterministic and cross-core
// (full Unicode word classification would risk the cross-core Unicode-version trap). PostgreSQL
// agrees for ASCII input; a non-ASCII letter is treated as a word boundary (a documented divergence).
func initcapASCII(s string) string {
	var b strings.Builder
	wordStart := true
	for _, c := range s {
		switch {
		case c >= '0' && c <= '9':
			b.WriteRune(c)
			wordStart = false
		case c >= 'A' && c <= 'Z':
			if wordStart {
				b.WriteRune(c)
			} else {
				b.WriteRune(c + 32)
			}
			wordStart = false
		case c >= 'a' && c <= 'z':
			if wordStart {
				b.WriteRune(c - 32)
			} else {
				b.WriteRune(c)
			}
			wordStart = false
		default:
			b.WriteRune(c)
			wordStart = true
		}
	}
	return b.String()
}

// satAddInt64 is a + b saturated to the int64 range (only positive overflow can arise where it is
// used, the right operand being non-negative).
func satAddInt64(a, b int64) int64 {
	s := a + b
	if a > 0 && b > 0 && s < 0 {
		return math.MaxInt64
	}
	return s
}

// substrChars is substr(s, start[, count]) over CODE POINTS (string-functions.md §3): 1-based; the
// window [start, start+count) (or [start, ∞) for the 2-arg form) intersected with [1, n]. A start
// ≤ 0 / past the end clips; a NEGATIVE count traps 22011. Matches PostgreSQL's text substr.
func substrChars(s string, start int64, count *int64) (string, error) {
	runes := []rune(s)
	n := int64(len(runes))
	var to int64
	if count != nil {
		if *count < 0 {
			return "", newError(SubstringError, "negative substring length not allowed")
		}
		to = satAddInt64(start, *count)
		if to > n+1 {
			to = n + 1
		}
	} else {
		to = n + 1
	}
	from := start
	if from < 1 {
		from = 1
	}
	if to <= from {
		return "", nil
	}
	return string(runes[from-1 : to-1]), nil
}

// leftChars is left(s, n) over CODE POINTS (string-functions.md §3): the first n characters; a
// negative n returns all but the last |n|. Matches PostgreSQL's left.
func leftChars(s string, n int64) string {
	runes := []rune(s)
	length := int64(len(runes))
	var end int64
	if n < 0 {
		end = satAddInt64(length, n)
		if end < 0 {
			end = 0
		}
	} else {
		end = n
		if end > length {
			end = length
		}
	}
	return string(runes[:end])
}

// rightChars is right(s, n) over CODE POINTS (string-functions.md §3): the last n characters; a
// negative n returns all but the first |n|. Matches PostgreSQL's right.
func rightChars(s string, n int64) string {
	runes := []rune(s)
	length := int64(len(runes))
	var start int64
	if n < 0 {
		if n == math.MinInt64 {
			start = length // |n| ≥ length ⇒ skip everything
		} else {
			start = -n
			if start > length {
				start = length
			}
		}
	} else {
		start = length - n
		if start < 0 {
			start = 0
		}
	}
	return string(runes[start:])
}

// widthBucketErr is the 2201G raised by width_bucket for a bad count / equal-or-nonfinite bounds.
func widthBucketErr(detail string) error {
	return newError(InvalidArgumentForWidthBucketFunction, detail)
}

// minScaleOf is the minimum scale that represents d exactly — its display scale minus trailing
// fractional zeros (decimal.md, the shared engine of min_scale/trim_scale). RoundToScale(t-1)
// equals the value iff the digit at scale t is zero (otherwise it rounds, changing the value), so
// the loop stops at the first non-zero fractional digit. Zero → 0.
func minScaleOf(d Decimal) uint32 {
	if d.IsZero() {
		return 0
	}
	t := d.Scale
	for t > 0 && d.RoundToScale(t-1).CmpValue(d) == 0 {
		t--
	}
	return t
}

// widthBucketNumeric is width_bucket over numerics: floor((operand−low)·count/(high−low)) + 1, with
// 0 below low / count+1 at-or-above high, and the reversed (low > high) range. The bucket is an EXACT
// truncated decimal quotient (all-positive in range, so trunc == floor). Returns the raw index (the
// caller range-checks it to int4). count > 0 is checked by the caller.
func widthBucketNumeric(op, low, high Decimal, count int64) (int64, error) {
	cmpBounds := low.CmpValue(high)
	if cmpBounds == 0 {
		return 0, widthBucketErr("lower bound cannot equal upper bound")
	}
	countDec := decimalFromInt64(count)
	bucket := func(hiNum, loNum, hiDen, loDen Decimal) (int64, error) {
		diff, err := hiNum.Sub(loNum)
		if err != nil {
			return 0, err
		}
		num, err := diff.Mul(countDec)
		if err != nil {
			return 0, err
		}
		den, err := hiDen.Sub(loDen)
		if err != nil {
			return 0, err
		}
		r, err := num.Rem(den)
		if err != nil {
			return 0, err
		}
		numMinusR, err := num.Sub(r)
		if err != nil {
			return 0, err
		}
		q, err := numMinusR.Div(den)
		if err != nil {
			return 0, err
		}
		b, ok := q.RoundToScale(0).ToInt64Round()
		if !ok {
			return 0, overflowErr(scalarInt32)
		}
		return satAdd1(b), nil
	}
	if cmpBounds < 0 { // ascending low < high
		if op.CmpValue(low) < 0 {
			return 0, nil
		}
		if op.CmpValue(high) >= 0 {
			return satAdd1(count), nil
		}
		return bucket(op, low, high, low)
	}
	// descending low > high
	if op.CmpValue(low) > 0 {
		return 0, nil
	}
	if op.CmpValue(high) <= 0 {
		return satAdd1(count), nil
	}
	return bucket(low, op, low, high)
}

// widthBucketFloat is width_bucket over f64: the same index in binary64 (a single correctly-rounded
// chain, so cross-core identical). A NaN operand/bound → 2201G; a non-finite bound → 2201G (the
// operand may be ±Inf, handled by the comparisons). Returns the raw index.
func widthBucketFloat(op, low, high float64, count int64) (int64, error) {
	if math.IsNaN(op) || math.IsNaN(low) || math.IsNaN(high) {
		return 0, widthBucketErr("operand, lower bound, and upper bound cannot be NaN")
	}
	if math.IsInf(low, 0) || math.IsInf(high, 0) {
		return 0, widthBucketErr("lower and upper bounds must be finite")
	}
	if low == high {
		return 0, widthBucketErr("lower bound cannot equal upper bound")
	}
	cf := float64(count)
	if low < high {
		if op < low {
			return 0, nil
		}
		if op >= high {
			return satAdd1(count), nil
		}
		return int64(math.Floor((op-low)/(high-low)*cf)) + 1, nil
	}
	if op > low {
		return 0, nil
	}
	if op <= high {
		return satAdd1(count), nil
	}
	return int64(math.Floor((low-op)/(low-high)*cf)) + 1, nil
}

// requireNumericOperand requires that an arithmetic operand is numeric (integer or decimal,
// or NULL); a boolean or text operand is a 42804 type error.
func requireNumericOperand(t resolvedType) error {
	if t.kind == rtBool || t.kind == rtText || t.kind == rtBytea || t.kind == rtUuid ||
		t.kind == rtTimestamp || t.kind == rtTimestamptz || t.kind == rtInterval || t.kind == rtDate ||
		t.kind == rtRange || t.kind == rtComposite || t.kind == rtArray ||
		t.kind == rtJson || t.kind == rtJsonb || t.kind == rtJsonPath ||
		isFloatKind(t.kind) {
		// float is handled by the dedicated float branch in resolveBinary BEFORE this is reached;
		// reject here too so any other caller treats it as a non-(int/decimal) operand. A range/
		// composite/array operand is likewise non-numeric (range arithmetic + * - lands in RF4).
		return typeError("arithmetic operators require numeric operands")
	}
	return nil
}

// intervalScaleResult gives the result type of an interval ×÷ number (spec/design/interval.md §5):
// interval * number, number * interval (commute), interval / number → interval. isScale is false
// when no interval is involved (or the op is not * / /). number / interval and interval × interval
// return false and fall to the ±-only temporal rule (which reports the 42804).
func intervalScaleResult(op binaryOp, lt, rt rtKind) (st scalarType, isScale bool) {
	lIv, rIv := lt == rtInterval, rt == rtInterval
	if !lIv && !rIv {
		return 0, false
	}
	numeric := func(k rtKind) bool { return k == rtInt || k == rtDecimal || k == rtNull }
	switch op {
	case opMul:
		if (lIv && numeric(rt)) || (rIv && numeric(lt)) {
			return scalarInterval, true
		}
	case opDiv:
		if lIv && numeric(rt) {
			return scalarInterval, true
		}
	}
	return 0, false
}

// factorToFraction returns a numeric factor value as an exact fraction (num, den) with den > 0.
func factorToFraction(v Value) (*big.Int, *big.Int, error) {
	if v.Kind == ValInt {
		return big.NewInt(v.Int), big.NewInt(1), nil
	}
	return parseFactorDecimal(v.decimal().Render())
}

// temporalArithResult gives the result type of a temporal +/- (spec/design/interval.md §5).
// isTemporal is false when neither operand is temporal (then arithmetic falls through to the
// numeric path); true with a non-nil error is a temporal operand in an unsupported combination
// (42804). A NULL operand adopts the other side's temporal type (so `timestamp ± NULL` types as
// timestamp and evaluates to NULL).
func temporalArithResult(op binaryOp, lt, rt rtKind) (st scalarType, isTemporal bool, err error) {
	temporal := func(k rtKind) bool { return k == rtInterval || k == rtTimestamp || k == rtTimestamptz }
	if !temporal(lt) && !temporal(rt) {
		return 0, false, nil
	}
	l, r := lt, rt
	if l == rtNull {
		l = rt
	}
	if r == rtNull {
		r = lt
	}
	switch {
	case (op == opAdd || op == opSub) && l == rtInterval && r == rtInterval:
		return scalarInterval, true, nil
	case op == opAdd && l == rtTimestamp && r == rtInterval,
		op == opAdd && l == rtInterval && r == rtTimestamp,
		op == opSub && l == rtTimestamp && r == rtInterval:
		return scalarTimestamp, true, nil
	case op == opAdd && l == rtTimestamptz && r == rtInterval,
		op == opAdd && l == rtInterval && r == rtTimestamptz,
		op == opSub && l == rtTimestamptz && r == rtInterval:
		return scalarTimestamptz, true, nil
	case op == opSub && l == rtTimestamp && r == rtTimestamp,
		op == opSub && l == rtTimestamptz && r == rtTimestamptz:
		return scalarInterval, true, nil
	default:
		return 0, true, typeError("unsupported operand types for temporal arithmetic")
	}
}

// dateArithResult settles the result type of a date arithmetic operator (spec/design/date.md §6):
// date ± integer → date, integer + date → date (Add commutes; an integer of any width — the
// family covers i16/i32/i64), date − date → i32 (the count of days between, PG's int4), and
// date ± interval → timestamp (the date widens to midnight, then the timestamp ± interval calendar
// shift — PG: date + interval is a timestamp, not a date). interval + date commutes (Add only);
// there is no integer − date nor interval − date. Any other combination involving a date is a
// 42804 (PG reports 42883; jed uses its datatype-mismatch code, like the interval rule). A bare
// untyped NULL partner is NOT adopted — date ± NULL is a 42804 (PG rejects the ambiguous form too).
func dateArithResult(op binaryOp, lt, rt rtKind) (scalarType, error) {
	switch {
	case op == opAdd && lt == rtDate && rt == rtInt,
		op == opAdd && lt == rtInt && rt == rtDate,
		op == opSub && lt == rtDate && rt == rtInt:
		return scalarDate, nil
	case op == opSub && lt == rtDate && rt == rtDate:
		return scalarInt32, nil
	case op == opAdd && lt == rtDate && rt == rtInterval,
		op == opAdd && lt == rtInterval && rt == rtDate,
		op == opSub && lt == rtDate && rt == rtInterval:
		return scalarTimestamp, nil
	default:
		return 0, typeError("unsupported operand types for date arithmetic")
	}
}

// classifyComparable requires that a comparison operand pair is comparable
// (spec/types/compare.toml): both numeric (integer and/or decimal — the integer promotes to
// decimal), both text, or both boolean (NULL counts as either). A mixed numeric/text pair, or
// a boolean with a non-boolean, is a 42804 type error — comparison is overloaded across these
// families but never compares across them.
func classifyComparable(lt, rt resolvedType) error {
	// json is NOT comparable: PostgreSQL ships no btree/hash operator class for `json`, so jed
	// matches it (spec/design/json.md §5). ANY json comparison — even json × json, json × jsonb,
	// or json × a bare NULL — is 42883 (operator does not exist), distinct from the cross-family
	// 42804 other types use. Must precede the jsonb arms so json × jsonb is 42883.
	if lt.kind == rtJson || rt.kind == rtJson {
		return newError(UndefinedFunction, "operator does not exist: json is not comparable")
	}
	// jsonpath is likewise NOT comparable (PG ships no opclass — jsonpath.md §1): every comparison is
	// 42883.
	if lt.kind == rtJsonPath || rt.kind == rtJsonPath {
		return newError(UndefinedFunction, "operator does not exist: jsonpath is not comparable")
	}
	// jsonb IS comparable — PostgreSQL's total btree order (spec/design/json.md §5) — but only with
	// another jsonb (or a bare NULL). jsonb vs any other family is 42804 (jed's cross-family
	// convention, like uuid/bytea/range; a documented divergence from PG's 42883).
	jsonbL, jsonbR := lt.kind == rtJsonb, rt.kind == rtJsonb
	if jsonbL || jsonbR {
		if (jsonbL && jsonbR) || (jsonbL && rt.kind == rtNull) || (lt.kind == rtNull && jsonbR) {
			return nil
		}
		return typeError("cannot compare a jsonb value with a value of a different type")
	}
	// Composite comparison is element-wise row comparison (spec/design/composite.md §5): two
	// composites are comparable iff they have the SAME field count and each corresponding field
	// pair is itself comparable (recursively — a nested composite recurses here, an anonymous
	// ROW(...) compares against a same-shape named type). A bare NULL is always comparable (the
	// comparison is unknown). A composite vs any non-composite, or a row-size mismatch, or an
	// incomparable field pair, is a 42804.
	compL, compR := lt.kind == rtComposite, rt.kind == rtComposite
	switch {
	case compL && rt.kind == rtNull, lt.kind == rtNull && compR:
		return nil
	case compL && compR:
		a, b := lt.comp.fields, rt.comp.fields
		if len(a) != len(b) {
			return typeError("cannot compare rows of different sizes")
		}
		for i := range a {
			if err := classifyComparable(a[i].ty, b[i].ty); err != nil {
				return err
			}
		}
		return nil
	case compL || compR:
		return typeError("cannot compare a composite value with a value of a different type")
	}
	// Array comparison is element-wise (spec/design/array.md §5): two arrays are comparable iff
	// their element types are comparable (recursively). A bare NULL is always comparable; an array
	// vs any non-array is 42804.
	arrL, arrR := lt.kind == rtArray, rt.kind == rtArray
	switch {
	case arrL && rt.kind == rtNull, lt.kind == rtNull && arrR:
		return nil
	case arrL && arrR:
		return classifyComparable(*lt.elem, *rt.elem)
	case arrL || arrR:
		return typeError("cannot compare an array value with a value of a different type")
	}
	// Boolean compares only with boolean (or NULL); boolean with a number/text is a mismatch.
	boolL, boolR := lt.kind == rtBool, rt.kind == rtBool
	if boolL != boolR && (lt.kind != rtNull && rt.kind != rtNull) {
		return typeError("cannot compare a boolean value with a non-boolean value")
	}
	lNum := lt.kind == rtInt || lt.kind == rtDecimal
	rNum := rt.kind == rtInt || rt.kind == rtDecimal
	if (lNum && rt.kind == rtText) || (lt.kind == rtText && rNum) {
		return typeError("cannot compare a text value with a numeric value")
	}
	// bytea compares only with bytea (or NULL); bytea with a number or text is a mismatch.
	byteaL, byteaR := lt.kind == rtBytea, rt.kind == rtBytea
	if byteaL != byteaR && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare a bytea value with a non-bytea value")
	}
	// uuid compares only with uuid (or NULL); uuid with anything else is a mismatch.
	uuidL, uuidR := lt.kind == rtUuid, rt.kind == rtUuid
	if uuidL != uuidR && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare a uuid value with a non-uuid value")
	}
	// timestamp / timestamptz compare only within their own family (or with NULL). A mixed
	// timestamp × timestamptz pair, or a datetime vs any other family, would need a zone, so
	// it is a 42804 type error (spec/design/timestamp.md §5).
	tsL := lt.kind == rtTimestamp || lt.kind == rtTimestamptz
	tsR := rt.kind == rtTimestamp || rt.kind == rtTimestamptz
	if (tsL || tsR) && lt.kind != rt.kind && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare a timestamp value with a value of a different type")
	}
	// interval compares only with itself (or NULL); interval vs any other family is a 42804.
	ivL, ivR := lt.kind == rtInterval, rt.kind == rtInterval
	if ivL != ivR && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare an interval value with a value of a different type")
	}
	// date compares only within its own family (or with NULL); date vs any other family —
	// including timestamp, which would need a cast — is a 42804 (date is a strict island,
	// spec/design/date.md §4, a documented PG divergence).
	dateL, dateR := lt.kind == rtDate, rt.kind == rtDate
	if dateL != dateR && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare a date value with a value of a different type")
	}
	// float compares only with float (either width promotes — §2) or NULL; float vs any other
	// family (incl. integer/decimal) is a 42804 — float is a strict island requiring an explicit
	// cast (spec/design/float.md §3/§6, a documented PG divergence).
	flL, flR := isFloatKind(lt.kind), isFloatKind(rt.kind)
	if flL != flR && lt.kind != rtNull && rt.kind != rtNull {
		return typeError("cannot compare a float value with a value of a different type")
	}
	// Range comparison is the PG range_cmp total order (spec/design/ranges.md §6). Two ranges are
	// comparable iff they are over the SAME element type — i32range × i32range only, never i32range ×
	// i64range or i32range × i32 (no implicit cross-element range comparison this slice; stricter than
	// the int↔bigint scalar case, so the element resolvedTypes must be EQUAL, not merely comparable). A
	// bare NULL is always comparable.
	rangeL, rangeR := lt.kind == rtRange, rt.kind == rtRange
	switch {
	case rangeL && rt.kind == rtNull, lt.kind == rtNull && rangeR:
		return nil
	case rangeL && rangeR:
		if resolvedTypeEqual(*lt.elem, *rt.elem) {
			return nil
		}
		return typeError("cannot compare ranges of different element types")
	case rangeL || rangeR:
		return typeError("cannot compare a range value with a value of a different type")
	}
	return nil
}

// isAdaptableOperand reports whether e is an adaptable operand — one that takes its type from
// its sibling: an integer or string literal, or a bind parameter $N (spec/design/api.md §5).
// NULL, boolean, and decimal literals do not take a sibling's context here.
func isAdaptableOperand(e exprNode) bool {
	if e.Kind == exprParam {
		return true
	}
	return e.Kind == exprLiteral && (e.Literal.Kind == literalInt || e.Literal.Kind == literalText)
}

// decodeByteaLiteral decodes a single-quoted literal's content as a bytea value via the hex
// input form (ParseByteaHex), mapping malformed hex to a 22P02 (invalid_text_representation).
// Used when a string literal adapts to a bytea context (types.md §6/§13); the trap is
// deterministic and fires at resolve time, before any scan.
func decodeByteaLiteral(s string) ([]byte, error) {
	b, reason := parseByteaHex(s)
	if reason != "" {
		return nil, newError(InvalidTextRepresentation, "invalid input syntax for type bytea: "+reason)
	}
	return b, nil
}

// decodeUUIDLiteral decodes a single-quoted literal's content as a uuid value via the
// PG-flexible input (ParseUUID), mapping malformed input to a 22P02. Used when a string literal
// adapts to a uuid context (types.md §6/§14); deterministic, fires at resolve time before any scan.
func decodeUUIDLiteral(s string) ([]byte, error) {
	b, reason := parseUUID(s)
	if reason != "" {
		return nil, newError(InvalidTextRepresentation, "invalid input syntax for type uuid: "+reason)
	}
	return b, nil
}

// litWhitespace is the ASCII whitespace set trimmed by the int/decimal/bool string coercions —
// EXACTLY Rust's is_ascii_whitespace (space, tab, LF, FF, CR; NO vertical tab), so the three cores
// trim byte-identically (a §8 determinism surface — a Unicode-aware trim would diverge).
const litWhitespace = " \t\n\f\r"

// floatConstFromDecimal builds a float constant from a decimal literal adapting to a float context
// (spec/design/float.md §4): the nearest binary value at the context width, round-ties-to-even. A
// magnitude beyond the width's range traps 22003 at resolve (the §3 finite-overflow rule). The
// decimal is NOT cap-checked first — it is converted directly (a huge literal traps via overflow).
func floatConstFromDecimal(d Decimal, ctx scalarType) (*rExpr, resolvedType, error) {
	if ctx.IsFloat32() {
		f, err := decimalToFloat32(d)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstFloat32, cFloat: float64(f)}, resolvedType{kind: rtFloat32}, nil
	}
	f, err := decimalToFloat64(d)
	if err != nil {
		return nil, resolvedType{}, err
	}
	return &rExpr{kind: reConstFloat64, cFloat: f}, resolvedType{kind: rtFloat64}, nil
}

// coerceStringToComposite coerces a string literal to a named composite type via record_in
// (spec/design/composite.md §8) — the shared engine of `'(…)'::addr` and the `addr '(…)'` typed
// literal. The text is tokenized by parseRecordTokens (nil or a wrong field count → 22P02
// invalid_text_representation, a "malformed record literal" message); then each token is coerced to
// its field's declared type: a NULL token is a typed NULL, a scalar field reuses coerceStringLiteral
// (the same string-literal coercion as a typed literal), and a composite field recurses. The folded
// result is an reRow of the coerced const field nodes, typed as the named composite — the inverse of
// recordOut.
func coerceStringToComposite(text string, ct *compositeType, catalog *engine) (*rExpr, resolvedType, error) {
	snap := catalog.readSnap()
	malformed := func() error {
		return newError(InvalidTextRepresentation,
			fmt.Sprintf("malformed record literal: %q for type %s", text, ct.Name))
	}
	tokens, ok := parseRecordTokens(text)
	if !ok || len(tokens) != len(ct.Fields) {
		return nil, resolvedType{}, malformed()
	}
	nodes := make([]*rExpr, len(tokens))
	fields := make([]compositeRField, len(tokens))
	for i := range tokens {
		f := ct.Fields[i]
		switch {
		case tokens[i] == nil:
			// A NULL field: a NULL value, typed by the field's declared type.
			nodes[i] = &rExpr{kind: reConstNull}
			fields[i] = compositeRField{name: f.Name, ty: resolvedTypeOfCol(f.Type, snap)}
		case f.Type.Comp != nil:
			// A nested composite field: the token is its own quoted record literal — recurse.
			nested := snap.compositeType(f.Type.Comp.Name)
			node, ty, err := coerceStringToComposite(*tokens[i], nested, catalog)
			if err != nil {
				return nil, resolvedType{}, err
			}
			nodes[i] = node
			fields[i] = compositeRField{name: f.Name, ty: ty}
		case f.Type.Array != nil:
			// An array-typed field (spec/design/array.md §12): the token is an array text literal,
			// coerced through array_in against the element type — the same path a bare `'{…}'::T[]`
			// cast uses, one level down. Folds to a constant array.
			elemCol := resolveColType(*f.Type.Array, snap.types)
			val, err := coerceStringToArray(*tokens[i], elemCol)
			if err != nil {
				return nil, resolvedType{}, err
			}
			nodes[i] = valueToRExpr(val)
			fields[i] = compositeRField{name: f.Name, ty: resolvedTypeOfCol(f.Type, snap)}
		default:
			node, ty, err := coerceStringLiteral(*tokens[i], f.Type.Scalar, f.Decimal, f.VarcharLen)
			if err != nil {
				return nil, resolvedType{}, err
			}
			nodes[i] = node
			fields[i] = compositeRField{name: f.Name, ty: ty}
		}
	}
	return &rExpr{kind: reRow, sargs: nodes},
		resolvedType{kind: rtComposite, comp: &compositeRType{named: true, name: ct.Name, fields: fields}}, nil
}

// coerceStringLiteral coerces a string literal's content to the named scalar target at resolve —
// the shared engine of the `type 'string'` typed literal and CAST(<string literal> AS target)
// (spec/design/grammar.md §36, types.md §5). Every scalar is reachable: the string-native types
// parse by their own input, text is identity, and int/decimal/boolean are the cast from text
// admitted only for a literal operand. 22P02 malformed / 22003 out of range / the type's parse
// code. typmod (decimal only) re-scales the result.
// coerceStringToRangeExpr coerces a range text literal to a constant range expression
// ('[1,5)'::i32range / i32range '[1,5)'): parse, coerce each bound to the element type, then
// canonicalize (spec/design/ranges.md §4/§5). Folds to a reConstRange. Malformed → 22P02;
// lower>upper → 22000; a canonicalize overflow → 22003.
// resolveContainerAssign resolves an UPDATE assignment RHS against a RANGE or ARRAY column (the
// caller has already rejected composite — 0A000). It mirrors INSERT's value adaptation
// (ranges.md §5 / array.md §7): a bare string literal adapts to the container via range_in /
// array_in, a bare NULL is the typed NULL, and any other expression must resolve to the SAME
// container type (matching element) else 42804. A top-level $N parameter is deferred (0A000) —
// INSERT's param-to-container handling is special and not generalized to the assignment RHS yet.
func resolveContainerAssign(s *scope, col catColumn, e exprNode, ag *aggCtx, params *paramTypes) (*rExpr, error) {
	snap := s.catalog.readSnap()
	colRT := resolvedTypeOfCol(col.Type, snap)
	// A bare string literal adapts to the container context (the same string-adapts-to-context rule
	// the cast and INSERT VALUES paths use).
	if e.Kind == exprLiteral && e.Literal != nil && e.Literal.Kind == literalText {
		if col.Type.IsRange() {
			elem, _ := col.Type.RangeElement()
			desc, ok := rangeForElement(elem.Scalar)
			if !ok {
				panic("a range column's element always has a range type")
			}
			node, _, err := coerceStringToRangeExpr(e.Literal.Str, desc)
			return node, err
		}
		// array
		val, err := coerceStringToArray(e.Literal.Str, resolveColType(*col.Type.Array, snap.types))
		if err != nil {
			return nil, err
		}
		return valueToRExpr(val), nil
	}
	if e.Kind == exprLiteral && e.Literal != nil && e.Literal.Kind == literalNull {
		return &rExpr{kind: reConstNull}, nil
	}
	if e.Kind == exprParam {
		kind := "range"
		if col.Type.IsArray() {
			kind = "array"
		}
		return nil, newError(FeatureNotSupported,
			"updating "+kind+" column "+col.Name+" from a parameter is not supported yet")
	}
	// For an array column over a SCALAR element, pass the element type as the hint so a bare
	// `ARRAY[1,2]` constructor adapts its literal elements to the column's element type (the same
	// adaptation `col = ARRAY[…]` uses — without it, bare int literals would type as i64 and miss a
	// narrower i32[]/i16[] column). A range gets no scalar hint (its bare-literal form was handled
	// above; other forms self-describe their element).
	var hint *scalarType
	if col.Type.IsArray() {
		if es, ok := col.Type.Array.AsScalar(); ok {
			hint = &es
		}
	}
	node, ty, err := resolve(s, e, hint, ag, params)
	if err != nil {
		return nil, err
	}
	if ty.kind == rtNull {
		return node, nil // a NULL-typed expression (e.g. a CASE that may be NULL)
	}
	if !containerAssignable(ty, colRT) {
		return nil, typeError("column " + col.Name + " is of type " + col.Type.CanonicalName() +
			" but expression is of type " + rtName(ty))
	}
	return node, nil
}

// containerAssignable reports whether a resolved RHS type is assignable to a range/array column
// type. Ranges require the SAME element scalar (i32range ⇍ i64range — no implicit cross-element
// range conversion, matching the comparison rule, ranges.md §6); arrays require structurally equal
// element types (array.md §5). A NULL RHS is handled by the caller.
func containerAssignable(rhs, col resolvedType) bool {
	if rhs.kind != col.kind {
		return false
	}
	switch col.kind {
	case rtRange:
		re, rok := resolvedRangeElementScalar(rhs.elem)
		ce, cok := resolvedRangeElementScalar(col.elem)
		return rok && cok && re == ce
	case rtArray:
		return resolvedTypeEqual(rhs, col)
	default:
		return false
	}
}

func coerceStringToRangeExpr(text string, desc rangeDesc) (*rExpr, resolvedType, error) {
	val, err := coerceStringToRange(text, desc)
	if err != nil {
		return nil, resolvedType{}, err
	}
	elemRT := resolvedTypeOf(elementScalar(desc))
	return &rExpr{kind: reConstRange, cRange: val}, resolvedType{kind: rtRange, elem: &elemRT}, nil
}

// coerceStringToRange parses a range text literal and coerces its bounds to the element type,
// producing a canonical RangeVal (spec/design/ranges.md §4). Shared by the cast / typed-literal paths.
func coerceStringToRange(text string, desc rangeDesc) (*RangeVal, error) {
	parsed, err := parseRangeText(text)
	if err != nil {
		return nil, err
	}
	if parsed.empty {
		return emptyRangeVal(), nil
	}
	elem := elementScalar(desc)
	coerceBound := func(b *string) (*Value, error) {
		if b == nil {
			return nil, nil
		}
		v, err := coerceStringLiteralToValue(*b, elem)
		if err != nil {
			return nil, err
		}
		return &v, nil
	}
	lower, err := coerceBound(parsed.lower)
	if err != nil {
		return nil, err
	}
	upper, err := coerceBound(parsed.upper)
	if err != nil {
		return nil, err
	}
	return finalizeRange(desc, lower, upper, parsed.lowerInc, parsed.upperInc)
}

func coerceStringLiteral(s string, target scalarType, typmod *decimalTypmod, varcharLen *uint32) (*rExpr, resolvedType, error) {
	switch target {
	case scalarBytea:
		b, err := decodeByteaLiteral(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstBytea, cBytea: b}, resolvedType{kind: rtBytea}, nil
	case scalarUuid:
		b, err := decodeUUIDLiteral(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstUuid, cBytea: b}, resolvedType{kind: rtUuid}, nil
	case scalarTimestamp:
		m, err := parseTimestamp(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstTimestamp, cInt: m}, resolvedType{kind: rtTimestamp}, nil
	case scalarTimestamptz:
		m, err := parseTimestamptz(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstTimestamptz, cInt: m}, resolvedType{kind: rtTimestamptz}, nil
	case scalarInterval:
		iv, err := parseInterval(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstInterval, cIv: iv}, resolvedType{kind: rtInterval}, nil
	case scalarDate:
		d, err := parseDate(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstDate, cInt: int64(d)}, resolvedType{kind: rtDate}, nil
	case scalarJson:
		// `json '…'` / CAST('…' AS json) — validate well-formedness, store the bytes verbatim
		// (spec/design/json.md §4); malformed → 22P02.
		if err := validateJSON(s); err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstJson, cText: s}, resolvedType{kind: rtJson}, nil
	case scalarJsonb:
		// `jsonb '…'` / CAST('…' AS jsonb) — parse + canonicalize (numbers→decimal, keys deduped +
		// sorted — §2); malformed → 22P02.
		node, err := jsonbIn(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstJsonb, cJsonb: &node}, resolvedType{kind: rtJsonb}, nil
	case scalarJsonPath:
		// '…'::jsonpath / jsonpath '…' — compile (P1a structural subset) + store the canonical
		// normalized text. Malformed → 42601; an unsupported (valid-PG) construct → 0A000.
		jp, err := compile(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstJsonPath, cText: jp.Render()}, resolvedType{kind: rtJsonPath}, nil
	case scalarText:
		// text 'x' is identity — the string IS the value. A varchar(n) 'x' typed literal /
		// CAST('x' AS varchar(n)) silently truncates to n code points (the explicit-cast rule,
		// spec/design/types.md §15) — no 22001 at resolve.
		cText := s
		if varcharLen != nil {
			cText = truncateToChars(s, int(*varcharLen))
		}
		return &rExpr{kind: reConstText, cText: cText}, resolvedType{kind: rtText}, nil
	case scalarBool:
		v, err := parseBoolLiteral(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstBool, cBool: v}, resolvedType{kind: rtBool}, nil
	case scalarDecimal:
		d, err := parseDecimalLiteral(s)
		if err != nil {
			return nil, resolvedType{}, err
		}
		if typmod != nil {
			d, err = d.CoerceToTypmod(uint32(typmod.Precision), uint32(typmod.Scale))
		} else {
			d, err = d.CheckCap()
		}
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstDecimal, cDec: d}, resolvedType{kind: rtDecimal}, nil
	case scalarFloat64:
		f, err := parseFloatLiteral(s, scalarFloat64)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstFloat64, cFloat: f}, resolvedType{kind: rtFloat64}, nil
	case scalarFloat32:
		f, err := parseFloatLiteral(s, scalarFloat32)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstFloat32, cFloat: f}, resolvedType{kind: rtFloat32}, nil
	default: // Int16 / Int32 / Int64
		n, err := parseIntLiteral(s, target)
		if err != nil {
			return nil, resolvedType{}, err
		}
		return &rExpr{kind: reConstInt, cInt: n}, resolvedType{kind: rtInt, intTy: target}, nil
	}
}

// parseIntLiteral parses a string literal's content as a signed integer of type ty — the
// text→integer coercion for INTEGER '42' / CAST('42' AS int) (grammar.md §36). jed's OWN
// integer-literal grammar: trimmed ASCII whitespace, optional +/-, then ASCII decimal digits
// (NO hex/octal/binary or underscores — 22P02, a documented PG divergence). Out of range → 22003.
func parseIntLiteral(s string, ty scalarType) (int64, error) {
	t := strings.Trim(s, litWhitespace)
	invalid := func() error {
		return newError(InvalidTextRepresentation,
			"invalid input syntax for type "+ty.CanonicalName()+": \""+s+"\"")
	}
	neg := false
	if rest, ok := strings.CutPrefix(t, "-"); ok {
		neg, t = true, rest
	} else if rest, ok := strings.CutPrefix(t, "+"); ok {
		t = rest
	}
	if t == "" || !allASCIIDigits(t) {
		return 0, invalid()
	}
	// big.Int parses an arbitrary-length digit run; an out-of-range value is 22003, not 22P02.
	mag, ok := new(big.Int).SetString(t, 10)
	if !ok {
		return 0, invalid()
	}
	if neg {
		mag.Neg(mag)
	}
	if !mag.IsInt64() {
		return 0, overflowErr(ty)
	}
	v := mag.Int64()
	if !ty.InRange(v) {
		return 0, overflowErr(ty)
	}
	return v, nil
}

// parseFloatLiteral parses a string literal's content as a float of width ty — the text→float
// coercion for float '1.5e10' / CAST('Infinity' AS f64) (spec/design/float.md §4, the float8in
// spellings). Accepts: trimmed ASCII whitespace, then either a finite numeric (optional sign,
// decimal digits with an optional point and optional e-notation) OR a case-insensitive special
// word — `Infinity`/`+Infinity`/`-Infinity`/`inf`/`+inf`/`-inf`/`NaN`. Malformed → 22P02; a finite
// value outside the width's range → 22003. Returns the value as a f64 (a f32 result is the
// f64 of the binary32 value). NO hex floats / underscores (a documented PG-input narrowing,
// cross-core determinism — like the int/decimal literals).
func parseFloatLiteral(s string, ty scalarType) (float64, error) {
	t := strings.Trim(s, litWhitespace)
	tn := ty.CanonicalName()
	invalid := func() error {
		return newError(InvalidTextRepresentation,
			"invalid input syntax for type "+tn+": \""+s+"\"")
	}
	// Special words (case-insensitive), with an optional leading sign on the infinities.
	lower := toLowerASCII(t)
	sign := 1.0
	body := lower
	if rest, ok := strings.CutPrefix(lower, "-"); ok {
		sign, body = -1.0, rest
	} else if rest, ok := strings.CutPrefix(lower, "+"); ok {
		body = rest
	}
	switch body {
	case "infinity", "inf":
		return math.Inf(int(sign)), nil
	case "nan":
		// NaN carries no sign in jed's total order; a leading sign is accepted and ignored (PG).
		// Use jed's canonical NaN pattern (NOT math.NaN(), whose low payload bit 0x…001 differs from
		// the Rust/TS cores) so a literal NaN is cross-core byte-identical in memory and on disk
		// (spec/design/float.md §3/§10).
		return canonicalNaN64(), nil
	}
	// Finite numeric: validate the grammar by hand (reject Go's hex-float / underscore / "inf"
	// shapes already handled above), then parse with strconv (correctly rounded at the width).
	if !validFloatNumeric(t) {
		return 0, invalid()
	}
	bits := 64
	if ty.IsFloat32() {
		bits = 32
	}
	f, err := strconv.ParseFloat(t, bits)
	if err != nil {
		// strconv only errors here on RANGE for a syntactically-valid number (validFloatNumeric
		// already gated syntax) → a finite value beyond the width's range traps 22003 (§4).
		return 0, overflowErr(ty)
	}
	if math.IsInf(f, 0) {
		// A finite numeric that rounded to ±Inf is out of range (22003), not a literal Infinity.
		return 0, overflowErr(ty)
	}
	if bits == 32 {
		return float64(float32(f)), nil
	}
	return f, nil
}

// validFloatNumeric reports whether t is a well-formed FINITE float numeric: an optional sign,
// then digits with an optional single '.' (a digit on at least one side) and an optional
// e-notation exponent (e/E, optional sign, ≥1 digit). No special words, no hex, no underscores.
func validFloatNumeric(t string) bool {
	if t == "" {
		return false
	}
	if t[0] == '+' || t[0] == '-' {
		t = t[1:]
	}
	mantissa, expPart, hasExp := t, "", false
	if i := strings.IndexAny(t, "eE"); i >= 0 {
		mantissa, expPart, hasExp = t[:i], t[i+1:], true
	}
	intPart, fracPart, hasDot := strings.Cut(mantissa, ".")
	if hasDot && strings.Contains(fracPart, ".") {
		return false
	}
	if !allASCIIDigits(intPart) || !allASCIIDigits(fracPart) {
		return false
	}
	if intPart == "" && fracPart == "" {
		return false // a lone "." or "" mantissa
	}
	if hasExp {
		e := expPart
		if len(e) > 0 && (e[0] == '+' || e[0] == '-') {
			e = e[1:]
		}
		if e == "" || !allASCIIDigits(e) {
			return false
		}
	}
	return true
}

// parseDecimalLiteral parses a string literal's content as a decimal — the text→decimal coercion
// for NUMERIC '1.5' / CAST('1.5' AS numeric) (grammar.md §36). jed's OWN decimal-literal grammar:
// trimmed ASCII whitespace, optional sign, ASCII digits with at most one '.' and a digit on at
// least one side, plus optional scientific e-notation (numeric '1.5e3' → 1500) — built into the
// SAME (digits, scale) the lexer feeds DecimalFromDigitsScale (via the shared decimalFromParts), so
// NUMERIC 'x' is byte-identical to writing x. NO NaN / Infinity and no hex/underscore (22P02).
// Caller applies typmod / cap-check.
func parseDecimalLiteral(s string) (Decimal, error) {
	t := strings.Trim(s, litWhitespace)
	invalid := func() (Decimal, error) {
		return Decimal{}, newError(InvalidTextRepresentation,
			"invalid input syntax for type numeric: \""+s+"\"")
	}
	neg := false
	if rest, ok := strings.CutPrefix(t, "-"); ok {
		neg, t = true, rest
	} else if rest, ok := strings.CutPrefix(t, "+"); ok {
		t = rest
	}
	// Split off an optional exponent. Unlike the lexer (which leaves a bare e for the next token),
	// an isolated string must be a COMPLETE numeric, so an e with no [+-]?digit+ after it is
	// malformed (22P02), matching PG's numeric_in.
	mantissa := t
	hasExp := false
	var exp int64
	if ei := strings.IndexAny(t, "eE"); ei >= 0 {
		mantissa = t[:ei]
		e := t[ei+1:]
		eneg := false
		if rest, ok := strings.CutPrefix(e, "-"); ok {
			eneg, e = true, rest
		} else if rest, ok := strings.CutPrefix(e, "+"); ok {
			e = rest
		}
		if e == "" || !allASCIIDigits(e) {
			return invalid()
		}
		// Clamp the magnitude to expLimit while accumulating (keeps it in i64 and bounds the
		// coefficient the shared builder may materialize).
		for m := 0; m < len(e); m++ {
			if exp < expLimit {
				exp = exp*10 + int64(e[m]-'0')
				if exp > expLimit {
					exp = expLimit
				}
			}
		}
		if eneg {
			exp = -exp
		}
		hasExp = true
	}
	intPart, frac, hasDot := strings.Cut(mantissa, ".")
	// A second '.' lands in frac (Cut splits on the first); reject it.
	if (hasDot && strings.Contains(frac, ".")) ||
		!allASCIIDigits(intPart) || !allASCIIDigits(frac) ||
		(intPart == "" && frac == "") {
		return invalid()
	}
	digits, scale := decimalFromParts(intPart, frac, hasExp, exp)
	return decimalFromDigitsScale(neg, digits, scale), nil
}

// parseBoolLiteral parses a string literal's content as a boolean — the text→boolean coercion for
// BOOLEAN 'true' / CAST('t' AS boolean) (grammar.md §36). Matches PostgreSQL's boolin: trimmed
// ASCII whitespace, case-insensitive; t/tr/tru/true, y/ye/yes, on, 1 → true and f/fa/fal/fals/
// false, n/no, off, 0 → false; anything else 22P02.
func parseBoolLiteral(s string) (bool, error) {
	switch toLowerASCII(strings.Trim(s, litWhitespace)) {
	case "t", "tr", "tru", "true", "y", "ye", "yes", "on", "1":
		return true, nil
	case "f", "fa", "fal", "fals", "false", "n", "no", "off", "0":
		return false, nil
	default:
		return false, newError(InvalidTextRepresentation,
			"invalid input syntax for type boolean: \""+s+"\"")
	}
}

// allASCIIDigits reports whether every byte of s is an ASCII digit (empty → true, so callers
// guard emptiness separately). Used by the int/decimal string coercions — ASCII '0'..'9' only,
// never Unicode digits (a §8 determinism surface).
func allASCIIDigits(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] < '0' || s[i] > '9' {
			return false
		}
	}
	return true
}

// promote is the promotion-tower result type of two arithmetic operands: the
// higher-ranked integer type, or i64 when both are untyped NULLs.
func promote(a, b resolvedType) scalarType {
	ax, aok := intType(a)
	bx, bok := intType(b)
	switch {
	case aok && bok:
		if ax.Rank() >= bx.Rank() {
			return ax
		}
		return bx
	case aok:
		return ax
	case bok:
		return bx
	default:
		return scalarInt64
	}
}

func requireBool(t resolvedType, msg string) error {
	if t.kind == rtInt || t.kind == rtText || t.kind == rtDecimal || t.kind == rtBytea || t.kind == rtUuid ||
		t.kind == rtTimestamp || t.kind == rtTimestamptz || t.kind == rtInterval || t.kind == rtDate ||
		t.kind == rtRange || t.kind == rtJson || t.kind == rtJsonb || t.kind == rtJsonPath {
		return typeError(msg)
	}
	return nil
}

// requireTextOrNull: LIKE requires both operands be text (or a bare NULL literal, which is
// comparable with anything and makes the result NULL at eval). A non-text operand is a 42804
// type error (spec/design/grammar.md §22).
func requireTextOrNull(t resolvedType) error {
	if t.kind == rtText || t.kind == rtNull {
		return nil
	}
	return typeError("LIKE requires text operands")
}
