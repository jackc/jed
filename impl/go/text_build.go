package jed

import (
	"math"
	"strings"
	"unicode/utf8"
)

// The text-building VARIADIC "any" kernels (spec/design/string-functions.md §6; mirrors
// impl/rust/src/executor/text_build.rs): concat, concat_ws, and format, over their already-evaluated
// argument values.

// formatHint is the hint PostgreSQL attaches to a malformed format() specifier (varlena.c).
const formatHint = `For a single "%" use "%%".`

// evalTextVariadic evaluates a concat / concat_ws / format node (the caller charged its
// operator_eval). sargs are the fixed parameter (none for concat; the separator / format string)
// followed by the spread arguments, or by the one array of the VARIADIC-array form. Non-strict: every
// argument is evaluated, then a NULL separator / format string yields NULL. A NULL VARIADIC array
// yields NULL for concat / concat_ws and is no arguments for format, as in PostgreSQL.
func evalTextVariadic(e *rExpr, row storedRow, env *evalEnv, m *costMeter) (Value, error) {
	vals := make([]Value, len(e.sargs))
	for i, a := range e.sargs {
		v, err := a.eval(row, env, m)
		if err != nil {
			return Value{}, err
		}
		vals[i] = v
	}
	fixed := 1
	if e.vfunc == vfConcat {
		fixed = 0
	}
	head := ""
	if fixed == 1 {
		if vals[0].IsNull() {
			return NullValue(), nil
		}
		head = vals[0].str()
	}
	var rest []Value
	if e.variadicArray {
		switch {
		case vals[fixed].IsNull() && e.vfunc == vfFormat:
			rest = nil
		case vals[fixed].IsNull():
			return NullValue(), nil
		default:
			rest = vals[fixed].arrayVal().Elements
		}
	} else {
		rest = vals[fixed:]
	}
	zone := env.exec.session.timeZone
	switch e.vfunc {
	case vfConcat:
		s, err := concatValues(rest, "", false, zone, m)
		if err != nil {
			return Value{}, err
		}
		// A growth kernel (cost.md §8.1), free over constant operands.
		return growthResult(m, e.sargs, TextValue(s))
	case vfConcatWs:
		s, err := concatValues(rest, head, true, zone, m)
		if err != nil {
			return Value{}, err
		}
		return TextValue(s), nil
	default:
		s, err := formatText(head, rest, zone, m)
		if err != nil {
			return Value{}, err
		}
		return TextValue(s), nil
	}
}

// outputText is one argument's OUTPUT text (string-functions.md §6): the text PostgreSQL's type
// output function produces. That is the value's rendering, except a boolean, which is t/f (boolout)
// rather than the true/false of the ::text cast. A finite timestamptz at any depth renders in the
// session zone and charges one timezone unit, charged and guarded before rendering. The caller
// handles NULL.
func outputText(v Value, zone ZoneRef, m *costMeter) (string, error) {
	switch v.Kind {
	case ValText:
		return v.str(), nil
	case ValBool:
		if v.boolVal() {
			return "t", nil
		}
		return "f", nil
	}
	if zoned := int64(v.zoneRenderCount()); zoned > 0 {
		m.Charge(costs.Timezone * zoned)
		if err := m.Guard(); err != nil {
			return "", err
		}
	}
	return v.renderIn(zone)
}

// concatValues is concat(VARIADIC "any") / concat_ws(sep, VARIADIC "any") (string-functions.md §6):
// the non-NULL values' output text joined by sep ("" for concat). For concat_ws (amplify) the kernel
// is an amplifier (cost.md §8.1): it sizes the result exactly, caps it at maxResultChars (54000),
// charges scalar_byte per output byte, guards, and reserves the bytes before building. concat is a
// growth kernel and its caller charges the built result.
func concatValues(vals []Value, sep string, amplify bool, zone ZoneRef, m *costMeter) (string, error) {
	parts := make([]string, 0, len(vals))
	for _, v := range vals {
		if v.IsNull() {
			continue
		}
		s, err := outputText(v, zone, m)
		if err != nil {
			return "", err
		}
		parts = append(parts, s)
	}
	if amplify {
		var size int64
		for _, p := range parts {
			size += int64(len(p))
		}
		if len(parts) > 1 {
			size += int64(len(parts)-1) * int64(len(sep))
		}
		if size > maxResultChars {
			return "", newError(ProgramLimitExceeded, "requested length too large")
		}
		m.Charge(costs.ScalarByte * size)
		if err := m.Guard(); err != nil {
			return "", err
		}
		if err := m.ReserveScalar(size); err != nil {
			return "", err
		}
	}
	return strings.Join(parts, sep), nil
}

// formatPiece is one piece of a format() result: a literal span [lo, hi) of the format string
// (isLit), or a converted argument with pad spaces (on the right when left).
type formatPiece struct {
	isLit  bool
	lo, hi int
	text   string
	pad    int64
	left   bool
}

func formatUnterminated() error {
	return newError(InvalidParameterValue, "unterminated format() type specifier").hinted(formatHint)
}

func formatNumberOutOfRange() error {
	return newError(NumericValueOutOfRange, "number is out of range")
}

func formatTooFew() error {
	return newError(InvalidParameterValue, "too few arguments for format()")
}

func formatArgZero() error {
	return newError(InvalidParameterValue, "format specifies argument 0, but arguments are numbered from 1")
}

// formatAdvance advances past byte i of the format string; reaching its end is the
// unterminated-specifier error (PostgreSQL's ADVANCE_PARSE_POINTER).
func formatAdvance(b string, i int) (int, error) {
	if i+1 >= len(b) {
		return 0, formatUnterminated()
	}
	return i + 1, nil
}

// formatDigits parses a run of ASCII digits at i as a nonnegative int32 (PostgreSQL's
// text_format_parse_digits): found reports whether at least one digit was read. An int32 overflow is
// 22003; digits that run to the end of the string are unterminated.
func formatDigits(b string, i int) (val int32, found bool, next int, err error) {
	for b[i] >= '0' && b[i] <= '9' {
		d := int64(b[i] - '0')
		n := int64(val)*10 + d
		if n > math.MaxInt32 {
			return 0, false, 0, formatNumberOutOfRange()
		}
		val = int32(n)
		if i, err = formatAdvance(b, i); err != nil {
			return 0, false, 0, err
		}
		found = true
	}
	return val, found, i, nil
}

// formatSpec is a parsed %… specifier, positioned at its conversion character (PostgreSQL's
// text_format_parse_format). argpos and widthpos are 1-based, -1 when absent; widthpos 0 means a *
// width taken from the next argument.
type formatSpec struct {
	argpos, widthpos int32
	minus            bool
	width            int32
	at               int
}

func formatParseSpec(b string, start int) (formatSpec, error) {
	spec := formatSpec{argpos: -1, widthpos: -1}
	n, found, i, err := formatDigits(b, start)
	if err != nil {
		return spec, err
	}
	if found {
		if b[i] != '$' {
			// Just a width and the conversion character.
			spec.width = n
			spec.at = i
			return spec, nil
		}
		if n == 0 {
			return spec, formatArgZero()
		}
		spec.argpos = n
		if i, err = formatAdvance(b, i); err != nil {
			return spec, err
		}
	}
	for b[i] == '-' {
		spec.minus = true
		if i, err = formatAdvance(b, i); err != nil {
			return spec, err
		}
	}
	if b[i] == '*' {
		if i, err = formatAdvance(b, i); err != nil {
			return spec, err
		}
		if n, found, i, err = formatDigits(b, i); err != nil {
			return spec, err
		}
		if found {
			if b[i] != '$' {
				return spec, newError(InvalidParameterValue, `width argument position must be ended by "$"`)
			}
			if n == 0 {
				return spec, formatArgZero()
			}
			spec.widthpos = n
			if i, err = formatAdvance(b, i); err != nil {
				return spec, err
			}
		} else {
			spec.widthpos = 0
		}
	} else {
		if n, found, i, err = formatDigits(b, i); err != nil {
			return spec, err
		}
		if found {
			spec.width = n
		}
	}
	spec.at = i
	return spec, nil
}

// formatWidthValue reads a * width argument as an int32 (PostgreSQL reads an int2/int4 directly and
// converts any other type through its output text). NULL is width 0; an integer outside int32 is
// 22003; any other value goes through the text → i32 cast, so a non-integer is that cast's 22P02 /
// 22003.
func formatWidthValue(v Value, zone ZoneRef, m *costMeter) (int32, error) {
	switch v.Kind {
	case ValNull:
		return 0, nil
	case ValInt:
		if v.Int < math.MinInt32 || v.Int > math.MaxInt32 {
			return 0, overflowErr(scalarInt32)
		}
		return int32(v.Int), nil
	}
	text, err := outputText(v, zone, m)
	if err != nil {
		return 0, err
	}
	n, err := parseIntLiteral(text, scalarInt32)
	if err != nil {
		return 0, err
	}
	return int32(n), nil
}

// formatText is format(formatstr, VARIADIC "any") (string-functions.md §6), PostgreSQL's text_format
// over args (the values after the format string). An amplifier (cost.md §8.1): the format string's
// UTF-8 bytes are charged as the input scan and guarded; one pass parses it and renders each
// converted argument (the specifier errors surface in scan order); the exact output size is then
// capped at maxResultChars (54000), charged, guarded, and reserved before the result is built.
func formatText(fmtStr string, args []Value, zone ZoneRef, m *costMeter) (string, error) {
	m.Charge(costs.ScalarByte * int64(len(fmtStr)))
	if err := m.Guard(); err != nil {
		return "", err
	}
	b := fmtStr
	nargs := len(args) + 1 // the format string is argument 0
	arg := 1               // the next argument to print
	var pieces []formatPiece
	lit := 0
	i := 0
	for i < len(b) {
		if b[i] != '%' {
			i++
			continue
		}
		if lit < i {
			pieces = append(pieces, formatPiece{isLit: true, lo: lit, hi: i})
		}
		var err error
		if i, err = formatAdvance(b, i); err != nil {
			return "", err
		}
		if b[i] == '%' {
			pieces = append(pieces, formatPiece{isLit: true, lo: i, hi: i + 1})
			i++
			lit = i
			continue
		}
		spec, err := formatParseSpec(b, i)
		if err != nil {
			return "", err
		}
		i = spec.at
		conv := b[i]
		if conv != 's' && conv != 'I' && conv != 'L' {
			r, _ := utf8.DecodeRuneInString(b[i:])
			return "", newError(InvalidParameterValue, `unrecognized format() type specifier "`+string(r)+`"`).hinted(formatHint)
		}
		width := spec.width
		if spec.widthpos >= 0 {
			if spec.widthpos > 0 {
				arg = int(spec.widthpos)
			}
			if arg >= nargs {
				return "", formatTooFew()
			}
			if width, err = formatWidthValue(args[arg-1], zone, m); err != nil {
				return "", err
			}
			arg++
		}
		if spec.argpos > 0 {
			arg = int(spec.argpos)
		}
		if arg >= nargs {
			return "", formatTooFew()
		}
		v := args[arg-1]
		arg++
		var text string
		if v.IsNull() {
			switch conv {
			case 's':
				text = ""
			case 'L':
				text = "NULL"
			default:
				return "", newError(NullValueNotAllowed, "null values cannot be formatted as an SQL identifier")
			}
		} else {
			out, err := outputText(v, zone, m)
			if err != nil {
				return "", err
			}
			switch conv {
			case 'I':
				text = quoteIdentText(out)
			case 'L':
				text = quoteLiteralText(out)
			default:
				text = out
			}
		}
		left := spec.minus
		if width < 0 {
			// A negative width is an implicit - flag; -INT_MIN is unrepresentable.
			if width == math.MinInt32 {
				return "", formatNumberOutOfRange()
			}
			left = true
			width = -width
		}
		var pad int64
		if chars := int64(utf8.RuneCountInString(text)); int64(width) > chars {
			pad = int64(width) - chars
		}
		pieces = append(pieces, formatPiece{text: text, pad: pad, left: left})
		i++
		lit = i
	}
	if lit < len(b) {
		pieces = append(pieces, formatPiece{isLit: true, lo: lit, hi: len(b)})
	}
	var size int64
	for _, p := range pieces {
		if p.isLit {
			size += int64(p.hi - p.lo)
		} else {
			size += int64(len(p.text)) + p.pad
		}
	}
	if size > maxResultChars {
		return "", newError(ProgramLimitExceeded, "requested length too large")
	}
	m.Charge(costs.ScalarByte * size)
	if err := m.Guard(); err != nil {
		return "", err
	}
	if err := m.ReserveScalar(size); err != nil {
		return "", err
	}
	var out strings.Builder
	out.Grow(int(size))
	for _, p := range pieces {
		switch {
		case p.isLit:
			out.WriteString(b[p.lo:p.hi])
		case p.left:
			out.WriteString(p.text)
			out.WriteString(strings.Repeat(" ", int(p.pad)))
		default:
			out.WriteString(strings.Repeat(" ", int(p.pad)))
			out.WriteString(p.text)
		}
	}
	return out.String(), nil
}
