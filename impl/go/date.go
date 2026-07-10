package jed

// The date calendar type — parsing and rendering (spec/design/date.md). A date is an i32
// count of days since the Unix epoch (1970-01-01), proleptic Gregorian. It is the day-granular
// sibling of timestamp and REUSES timestamp's calendar core verbatim (daysFromCivil/civilFromDays,
// same epoch — spec/design/timestamp.md §2), so the two types cannot drift.
//
// Unlike timestamp, a date keeps ONLY the date portion: a time/offset in the input is parsed and
// validated, then DISCARDED — and 24:00:00 does NOT roll into the day (PG behavior). No instant is
// ever computed, so a date spans a wider range than the i64-µs timestamp (finite
// math.MinInt32+1 .. math.MaxInt32-1).

import (
	"fmt"
	"strings"
)

// DateNegInfinity is the -infinity sentinel — the smallest i32, sorts before every finite date.
const dateNegInfinity int32 = -2147483648

// DatePosInfinity is the +infinity sentinel — the largest i32, sorts after every finite date.
const datePosInfinity int32 = 2147483647

// Finite day counts occupy [MinInt32+1, MaxInt32-1]; the extremes are reserved for ±infinity.
const (
	dateMinFinite int64 = -2147483647
	dateMaxFinite int64 = 2147483646
)

// dateClockSpecial classifies a date-input string as one of PostgreSQL's special values beyond
// ±infinity (spec/design/date.md §6): 'epoch' (the constant 1970-01-01 — epoch=true), and the
// CLOCK-RELATIVE words 'today' / 'now' (offset 0), 'tomorrow' (+1), 'yesterday' (−1) — the
// statement-clock day in the session zone, shifted by offset days. Case-insensitive, whitespace
// trimmed (like parseDate's own specials). ok=false for every other string; parseDate itself
// stays pure and continues to reject these words (its callers classify first where the specials
// are admitted — literal adaptation and the explicit casts, never the assignment coercions).
func dateClockSpecial(input string) (offsetDays int32, epoch, ok bool) {
	switch strings.ToLower(trimASCIIWS(input)) {
	case "epoch":
		return 0, true, true
	case "now", "today":
		return 0, false, true
	case "tomorrow":
		return 1, false, true
	case "yesterday":
		return -1, false, true
	}
	return 0, false, false
}

// makeDate builds a date from its (year, month, day) fields — PostgreSQL's make_date, the
// makeTimestamp sibling (spec/design/functions.md §11). A negative year is BC; year zero, a bad
// month/day-for-month, or a day count beyond the finite i32 window traps 22008 (PG "date field
// value out of range"). The same daysFromCivil calendar core as parseDate, so the two cannot drift.
func makeDate(year, month, day int64) (int32, error) {
	if year == 0 {
		return 0, datetimeFieldOverflow("date field value out of range")
	}
	bc := year < 0
	mag := year
	if bc {
		mag = -year
	}
	if mag > 9_999_999 {
		// Only an i64-overflow guard for daysFromCivil (like parseDate's year cap); the real
		// bound is the finite-i32 day-range check below.
		return 0, datetimeFieldOverflow("date field value out of range")
	}
	if month < 1 || month > 12 {
		return 0, datetimeFieldOverflow("date field value out of range")
	}
	astro := mag
	if bc {
		astro = 1 - mag
	}
	if day < 1 || day > daysInMonth(astro, month) {
		return 0, datetimeFieldOverflow("date field value out of range")
	}
	days := daysFromCivil(astro, month, day)
	if days < dateMinFinite || days > dateMaxFinite {
		return 0, datetimeFieldOverflow("date field value out of range")
	}
	return int32(days), nil
}

// dateClockIsRelative reports whether input names a CLOCK-RELATIVE special — 'today' / 'now' /
// 'tomorrow' / 'yesterday', but not 'epoch' (a foldable constant).
func dateClockIsRelative(input string) bool {
	_, epoch, ok := dateClockSpecial(input)
	return ok && !epoch
}

// dateClockIsSpecial reports whether input names ANY date special beyond ±infinity —
// clock-relative or the constant 'epoch'.
func dateClockIsSpecial(input string) bool {
	_, _, ok := dateClockSpecial(input)
	return ok
}

// ParseDate parses a date literal to its i32 day count since 1970-01-01. The grammar is the
// full timestamp literal grammar (spec/design/timestamp.md §3), but only the date portion is
// kept: a trailing time and/or offset is validated then discarded, and 24:00:00 does not advance
// the day. Malformed syntax traps 22007; an out-of-range field or a day count beyond the finite
// i32 range traps 22008.
func parseDate(input string) (int32, error) {
	s := trimASCIIWS(input)
	low := strings.ToLower(s)

	switch low {
	case "infinity", "+infinity":
		return datePosInfinity, nil
	case "-infinity":
		return dateNegInfinity, nil
	case "epoch":
		// PG's constant special: 1970-01-01 (day 0). The CLOCK-relative specials (today/now/…)
		// are deliberately NOT parseDate's — it stays a pure function; they resolve a level
		// above (dateClockSpecial → the STABLE node / literal adaptation, date.md §6).
		return 0, nil
	}

	bc := false
	body := s
	if strings.HasSuffix(low, " bc") {
		bc = true
		body = trimASCIIWS(s[:len(s)-3])
	} else if strings.HasSuffix(low, " ad") {
		body = trimASCIIWS(s[:len(s)-3])
	}

	i := 0
	year, err := readUint(body, &i)
	if err != nil {
		return 0, err
	}
	if err := expectByte(body, &i, '-'); err != nil {
		return 0, err
	}
	month, err := readUint(body, &i)
	if err != nil {
		return 0, err
	}
	if err := expectByte(body, &i, '-'); err != nil {
		return 0, err
	}
	day, err := readUint(body, &i)
	if err != nil {
		return 0, err
	}

	bad := invalidDatetime("invalid input syntax for type date")

	// optional time — validated for syntax/range, then DISCARDED (the day is taken from the date
	// fields directly; 24:00:00 does not advance it).
	var hour, minute, second, micro int64
	if i < len(body) && (body[i] == ' ' || body[i] == 'T' || body[i] == 't') {
		i++
		if hour, err = readUint(body, &i); err != nil {
			return 0, err
		}
		if err = expectByte(body, &i, ':'); err != nil {
			return 0, err
		}
		if minute, err = readUint(body, &i); err != nil {
			return 0, err
		}
		if i < len(body) && body[i] == ':' {
			i++
			if second, err = readUint(body, &i); err != nil {
				return 0, err
			}
			if i < len(body) && body[i] == '.' {
				i++
				if micro, err = readFrac(body, &i); err != nil {
					return 0, err
				}
			}
		}
	}

	// optional offset — validated, then DISCARDED (never applied, so it cannot shift the day).
	if i < len(body) {
		switch body[i] {
		case 'Z', 'z':
			i++
		case '+', '-':
			i++
			oh, err := readUint(body, &i)
			if err != nil {
				return 0, err
			}
			var om, os int64
			if i < len(body) && body[i] == ':' {
				i++
				if om, err = readUint(body, &i); err != nil {
					return 0, err
				}
				if i < len(body) && body[i] == ':' {
					i++
					if os, err = readUint(body, &i); err != nil {
						return 0, err
					}
				}
			}
			if oh > 15 || om > 59 || os > 59 {
				return 0, datetimeFieldOverflow("time zone offset out of range")
			}
		default:
			return 0, bad
		}
	}
	if i != len(body) {
		return 0, bad
	}

	// Field validation (range errors are 22008). The year magnitude cap (a date spans ≈ ±5.88M
	// years, far wider than timestamp's ±294k) is only an i64-overflow guard for daysFromCivil;
	// the real bound is the i32 day-range check below.
	if year < 1 || year > 9_999_999 {
		return 0, datetimeFieldOverflow("year out of range")
	}
	if month < 1 || month > 12 {
		return 0, datetimeFieldOverflow("month out of range")
	}
	astro := year
	if bc {
		astro = 1 - year
	}
	if day < 1 || day > daysInMonth(astro, month) {
		return 0, datetimeFieldOverflow("day out of range for month")
	}
	// hour 0..23, plus exactly 24:00:00 (a valid end-of-day; unlike timestamp it does NOT advance
	// the date — the day comes from the date fields directly).
	allow24 := hour == 24 && minute == 0 && second == 0 && micro == 0
	if hour > 23 && !allow24 {
		return 0, datetimeFieldOverflow("hour out of range")
	}
	if minute > 59 {
		return 0, datetimeFieldOverflow("minute out of range")
	}
	if second > 59 {
		return 0, datetimeFieldOverflow("second out of range") // no leap seconds (:60)
	}

	days := daysFromCivil(astro, month, day)
	if days < dateMinFinite || days > dateMaxFinite {
		return 0, datetimeFieldOverflow("date out of range")
	}
	return int32(days), nil
}

// RenderDate renders a date value (i32 days since 1970-01-01) to its canonical YYYY-MM-DD text
// (a BC suffix for an astronomical year <= 0; ±infinity render as the bare words).
func renderDate(days int32) string {
	if days == dateNegInfinity {
		return "-infinity"
	}
	if days == datePosInfinity {
		return "infinity"
	}
	y, mo, d := civilFromDays(int64(days))
	displayed := y
	era := ""
	if y <= 0 {
		displayed = 1 - y
		era = " BC"
	}
	return fmt.Sprintf("%04d-%02d-%02d%s", displayed, mo, d, era)
}
