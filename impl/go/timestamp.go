package jed

import (
	"fmt"
	"math"
	"strings"
)

// Timestamp / timestamptz calendar math, parsing, and rendering (spec/design/timestamp.md).
// Both types are an i64 count of microseconds since the Unix epoch (1970-01-01 00:00:00
// UTC), proleptic Gregorian, no leap seconds. This is a §8 determinism hotspot: the
// civil↔instant conversion (Hinnant), parse grammar, and render format must be byte-identical
// across the Rust/Go/TS cores. The civil↔days path uses Go's TRUNCATING `/` paired with the
// Hinnant -399/-146096 adjustment; the instant↔civil decomposition uses FLOOR div/mod helpers.

// NegInfinity is the -infinity sentinel — the smallest i64, sorts before every finite instant.
const negInfinity int64 = -9223372036854775808

// PosInfinity is the +infinity sentinel — the largest i64, sorts after every finite instant.
const posInfinity int64 = 9223372036854775807

const (
	microsPerSec = 1_000_000
	secsPerDay   = 86_400
)

// floorDiv is integer division rounding toward negative infinity (Go's `/` truncates).
func floorDiv(a, b int64) int64 {
	q := a / b
	if (a%b != 0) && ((a < 0) != (b < 0)) {
		q--
	}
	return q
}

// floorMod is the modulo matching floorDiv (always in [0, b) for b > 0).
func floorMod(a, b int64) int64 {
	m := a % b
	if m != 0 && ((m < 0) != (b < 0)) {
		m += b
	}
	return m
}

func isLeap(y int64) bool {
	return y%4 == 0 && (y%100 != 0 || y%400 == 0)
}

func daysInMonth(y int64, month int64) int64 {
	switch month {
	case 1, 3, 5, 7, 8, 10, 12:
		return 31
	case 4, 6, 9, 11:
		return 30
	case 2:
		if isLeap(y) {
			return 29
		}
		return 28
	default:
		return 0
	}
}

// daysFromCivil returns days since 1970-01-01 for the civil date (y, m, d) (Hinnant). `y` is
// the astronomical year; `/` is truncating, paired with the y-399 adjustment (= floor for y<0).
func daysFromCivil(y, m, d int64) int64 {
	if m <= 2 {
		y--
	}
	var eraNum int64
	if y >= 0 {
		eraNum = y
	} else {
		eraNum = y - 399
	}
	era := eraNum / 400
	yoe := y - era*400
	var mAdj int64 = 9
	if m > 2 {
		mAdj = -3
	}
	doy := (153*(m+mAdj)+2)/5 + (d - 1)
	doe := yoe*365 + yoe/4 - yoe/100 + doy
	return era*146097 + doe - 719468
}

// civilFromDays returns the civil date (year, month, day) from days since 1970-01-01.
func civilFromDays(z int64) (int64, int64, int64) {
	z += 719468
	var eraNum int64
	if z >= 0 {
		eraNum = z
	} else {
		eraNum = z - 146096
	}
	era := eraNum / 146097
	doe := z - era*146097
	yoe := (doe - doe/1460 + doe/36524 - doe/146096) / 365
	y := yoe + era*400
	doy := doe - (365*yoe + yoe/4 - yoe/100)
	mp := (5*doy + 2) / 153
	d := doy - (153*mp+2)/5 + 1
	var m int64
	if mp < 10 {
		m = mp + 3
	} else {
		m = mp - 9
	}
	if m <= 2 {
		y++
	}
	return y, m, d
}

// civilFromMicros decomposes an instant into civil fields using FLOOR division (so pre-1970 /
// BC instants decompose correctly; us is always 0..999_999).
func civilFromMicros(t int64) (y, mo, d, h, mi, s, us int64) {
	us = floorMod(t, microsPerSec)
	secs := floorDiv(t, microsPerSec)
	sod := floorMod(secs, secsPerDay)
	days := floorDiv(secs, secsPerDay)
	y, mo, d = civilFromDays(days)
	h = sod / 3600
	mi = (sod % 3600) / 60
	s = sod % 60
	return
}

// --- parsing -----------------------------------------------------------------

func invalidDatetime(detail string) error {
	return newError(InvalidDatetimeFormat, detail)
}

func datetimeFieldOverflow(detail string) error {
	return newError(DatetimeFieldOverflow, detail)
}

// errZoneShiftOutOfRange is the 22008 raised when a zone shift (instantToLocalMicros /
// localToInstantMicros) leaves the finite timestamp range (timezones.md §6).
func errZoneShiftOutOfRange() error { return datetimeFieldOverflow("timestamp out of range") }

func isWS(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n' || b == '\f' || b == '\r'
}

func trimASCIIWS(s string) string {
	start, end := 0, len(s)
	for start < end && isWS(s[start]) {
		start++
	}
	for end > start && isWS(s[end-1]) {
		end--
	}
	return s[start:end]
}

// readUint reads one run of ASCII digits at *i as an i64 (checked). Empty run → 22007; a
// value that overflows i64 → 22008.
func readUint(b string, i *int) (int64, error) {
	start := *i
	var v int64
	for *i < len(b) && b[*i] >= '0' && b[*i] <= '9' {
		d := int64(b[*i] - '0')
		nv := v*10 + d
		if nv < v { // i64 overflow
			return 0, datetimeFieldOverflow("numeric field too large")
		}
		v = nv
		*i++
	}
	if *i == start {
		return 0, invalidDatetime("expected a number")
	}
	return v, nil
}

func expectByte(b string, i *int, c byte) error {
	if *i < len(b) && b[*i] == c {
		*i++
		return nil
	}
	return invalidDatetime(fmt.Sprintf("expected %q", c))
}

// readFrac parses fractional-seconds digits into microseconds (0..1_000_000; 1_000_000 means
// the rounding carried). 0–6 digits exact; 7+ round to µs half away from zero (7th digit >= 5).
func readFrac(b string, i *int) (int64, error) {
	start := *i
	for *i < len(b) && b[*i] >= '0' && b[*i] <= '9' {
		*i++
	}
	digits := b[start:*i]
	if len(digits) == 0 {
		return 0, invalidDatetime("expected fractional digits after '.'")
	}
	var us int64
	for k := 0; k < 6; k++ {
		us *= 10
		if k < len(digits) {
			us += int64(digits[k] - '0')
		}
	}
	if len(digits) > 6 && digits[6] >= '5' {
		us++
	}
	return us, nil
}

// parseDatetime parses a zoneless timestamp to µs since the epoch: an offset is parsed/validated
// but ignored (PG behavior). typeName is used only for error messages.
func parseDatetime(input string, typeName string) (int64, error) {
	w, err := parseDatetimeWall(input, typeName)
	if err != nil {
		return 0, err
	}
	if w.infinite {
		return w.micros, nil
	}
	return finiteInstant(w.micros)
}

// datetimeWall is a parsed datetime string: a ±infinity sentinel (infinite), or a finite wall
// clock (µs since the epoch, not yet checked against the sentinels) with its explicit trailing
// offset in seconds when hasOffset.
type datetimeWall struct {
	micros     int64
	offsetSecs int64
	hasOffset  bool
	infinite   bool
}

// finiteInstant rejects a finite instant landing on a ±infinity sentinel (22008).
func finiteInstant(micros int64) (int64, error) {
	if micros == negInfinity || micros == posInfinity {
		return 0, datetimeFieldOverflow("value out of range")
	}
	return micros, nil
}

// applyUTCOffset normalizes a finite wall clock carrying offsetSecs to its UTC instant. Any
// overflow, or a result landing on a ±infinity sentinel, is a 22008 range error.
func applyUTCOffset(wall, offsetSecs int64) (int64, error) {
	off, ok := mul64(offsetSecs, microsPerSec)
	if !ok {
		return 0, datetimeFieldOverflow("value out of range")
	}
	micros, ok := sub64(wall, off)
	if !ok {
		return 0, datetimeFieldOverflow("value out of range")
	}
	return finiteInstant(micros)
}

// parseTimestamptzInput parses a timestamptz input to its reading: an explicit offset (or
// ±infinity) yields the instant (local = false), while an offset-less wall clock is returned with
// local = true for the caller to interpret in the session zone. The runtime text → timestamptz cast
// (types.md §5) uses it directly so it can charge the zone consult only when one happens. A local
// wall clock is not yet checked against the sentinels.
func parseTimestamptzInput(s string) (micros int64, local bool, err error) {
	w, err := parseDatetimeWall(s, "timestamptz")
	if err != nil {
		return 0, false, err
	}
	switch {
	case w.infinite:
		return w.micros, false, nil
	case w.hasOffset:
		mc, err := applyUTCOffset(w.micros, w.offsetSecs)
		return mc, false, err
	default:
		return w.micros, true, nil
	}
}

// parseDatetimeWall parses a datetime string to its datetimeWall reading. Shared by every
// timestamp/timestamptz input path; typeName is used only for error messages.
func parseDatetimeWall(input string, typeName string) (datetimeWall, error) {
	s := trimASCIIWS(input)
	low := strings.ToLower(s)

	switch low {
	case "infinity", "+infinity":
		return datetimeWall{micros: posInfinity, infinite: true}, nil
	case "-infinity":
		return datetimeWall{micros: negInfinity, infinite: true}, nil
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
		return datetimeWall{}, err
	}
	if err := expectByte(body, &i, '-'); err != nil {
		return datetimeWall{}, err
	}
	month, err := readUint(body, &i)
	if err != nil {
		return datetimeWall{}, err
	}
	if err := expectByte(body, &i, '-'); err != nil {
		return datetimeWall{}, err
	}
	day, err := readUint(body, &i)
	if err != nil {
		return datetimeWall{}, err
	}

	bad := invalidDatetime("invalid input syntax for type " + typeName)

	var hour, minute, second, micro int64
	if i < len(body) && (body[i] == ' ' || body[i] == 'T' || body[i] == 't') {
		i++
		if hour, err = readUint(body, &i); err != nil {
			return datetimeWall{}, err
		}
		if err = expectByte(body, &i, ':'); err != nil {
			return datetimeWall{}, err
		}
		if minute, err = readUint(body, &i); err != nil {
			return datetimeWall{}, err
		}
		if i < len(body) && body[i] == ':' {
			i++
			if second, err = readUint(body, &i); err != nil {
				return datetimeWall{}, err
			}
			if i < len(body) && body[i] == '.' {
				i++
				if micro, err = readFrac(body, &i); err != nil {
					return datetimeWall{}, err
				}
			}
		}
	}

	var offsetSecs int64
	hasOffset := false
	if i < len(body) {
		switch body[i] {
		case 'Z', 'z':
			i++
			hasOffset = true
		case '+', '-':
			sign := int64(1)
			if body[i] == '-' {
				sign = -1
			}
			i++
			oh, err := readUint(body, &i)
			if err != nil {
				return datetimeWall{}, err
			}
			var om, os int64
			if i < len(body) && body[i] == ':' {
				i++
				if om, err = readUint(body, &i); err != nil {
					return datetimeWall{}, err
				}
				if i < len(body) && body[i] == ':' {
					i++
					if os, err = readUint(body, &i); err != nil {
						return datetimeWall{}, err
					}
				}
			}
			if oh > 15 || om > 59 || os > 59 {
				return datetimeWall{}, datetimeFieldOverflow("time zone offset out of range")
			}
			offsetSecs = sign * (oh*3600 + om*60 + os)
			hasOffset = true
		default:
			return datetimeWall{}, bad
		}
	}
	if i != len(body) {
		return datetimeWall{}, bad
	}

	if year < 1 || year > 999_999 {
		return datetimeWall{}, datetimeFieldOverflow("year out of range")
	}
	if month < 1 || month > 12 {
		return datetimeWall{}, datetimeFieldOverflow("month out of range")
	}
	astro := year
	if bc {
		astro = 1 - year
	}
	if day < 1 || day > daysInMonth(astro, month) {
		return datetimeWall{}, datetimeFieldOverflow("day out of range for month")
	}
	extraDay := hour == 24 && minute == 0 && second == 0 && micro == 0
	if hour > 23 && !extraDay {
		return datetimeWall{}, datetimeFieldOverflow("hour out of range")
	}
	if minute > 59 {
		return datetimeWall{}, datetimeFieldOverflow("minute out of range")
	}
	if second > 59 {
		return datetimeWall{}, datetimeFieldOverflow("second out of range")
	}
	hourPart := hour
	if extraDay {
		hourPart = 0
	}

	days := daysFromCivil(astro, month, day)
	if extraDay {
		days++
	}
	micros, ok := mulAdd(days, secsPerDay, hourPart*3600+minute*60+second)
	if !ok {
		return datetimeWall{}, datetimeFieldOverflow("value out of range")
	}
	micros, ok = mulAdd(micros, microsPerSec, micro)
	if !ok {
		return datetimeWall{}, datetimeFieldOverflow("value out of range")
	}
	return datetimeWall{micros: micros, offsetSecs: offsetSecs, hasOffset: hasOffset}, nil
}

// mulAdd computes a*b + c with i64 overflow detection.
func mulAdd(a, b, c int64) (int64, bool) {
	p, ok := mul64(a, b)
	if !ok {
		return 0, false
	}
	return add64(p, c)
}

func mul64(a, b int64) (int64, bool) {
	if a == 0 || b == 0 {
		return 0, true
	}
	p := a * b
	if p/b != a {
		return 0, false
	}
	return p, true
}

func add64(a, b int64) (int64, bool) {
	s := a + b
	if (b > 0 && s < a) || (b < 0 && s > a) {
		return 0, false
	}
	return s, true
}

func sub64(a, b int64) (int64, bool) {
	s := a - b
	if (b < 0 && s < a) || (b > 0 && s > a) {
		return 0, false
	}
	return s, true
}

// ParseTimestamp parses a timestamp (zoneless) literal: an offset in the text is accepted and
// ignored (PG behavior).
func parseTimestamp(s string) (int64, error) { return parseDatetime(s, "timestamp") }

// ParseTimestamptz parses a timestamptz input string, as PostgreSQL's timestamptz_in does
// (timezones.md §9.3): a trailing offset normalizes the wall clock to UTC, ±infinity is its
// sentinel, and an offset-less wall clock is read in zone — the session zone at every caller (DST
// gap/overlap as the timestamp → timestamptz cast; a shift past the range is 22008). It meters
// nothing: a caller that evaluates per row charges the zone consult.
func parseTimestamptz(s string, zone ZoneRef) (int64, error) {
	iz := inputZone{zone: zone}
	return iz.parseTimestamptz(s)
}

// inputZone is the zone a text input reads an offset-less timestamptz in — always the session zone
// (timezones.md §9.3) — plus a record of whether any parse actually consulted it. Threaded through
// every text-input path (literal folding, assignment coercion, bound parameters, and the
// array/range/record text parsers). The resolver reads consulted after folding a literal
// (paramTypes.noteInputZone): a folded instant that depended on the session zone makes the plan
// uncacheable (a plan is shared across sessions) and nonimmutable (a persisted expression is
// re-resolved in whatever zone the next statement runs under — 42P17). Explicit offsets and
// ±infinity never consult it.
type inputZone struct {
	zone      ZoneRef
	consulted bool
}

// parseTimestamptz is the package parseTimestamptz in this zone, recording a zone consult for an
// offset-less wall clock.
func (iz *inputZone) parseTimestamptz(s string) (int64, error) {
	mc, local, err := parseTimestamptzInput(s)
	if err != nil {
		return 0, err
	}
	if !local {
		return mc, nil
	}
	iz.consulted = true
	inst, ok := localToInstantMicros(iz.zone, mc)
	if !ok {
		return 0, errZoneShiftOutOfRange()
	}
	return inst, nil
}

// MakeTimestamp builds a zoneless timestamp (µs since the 1970 epoch) from calendar fields — the
// workhorse for make_timestamp / make_timestamptz (functions.md §11; PG make_timestamp_internal). A
// negative year denotes BC (PG). Field validation mirrors the timestamp parser and traps 22008: the
// year magnitude in 1..999999 (no year zero), month 1..12, day valid for the month, and the
// assembled time of day not past 24:00:00 — PG allows hour = 24 or sec = 60 so long as the whole
// time of day stays within a day, enforced here by one total-of-day check. sec (double precision)
// folds to micros by one correctly-rounded multiply + half-away round (the engine's one mode —
// float.md §6); it differs from PG's rint only at an exact half-microsecond tie, which realistic
// input never hits.
func makeTimestamp(year, month, day, hour, minute int64, sec float64) (int64, error) {
	// Date fields (22008). A negative year is BC; year 0 has no AD/BC representation.
	if year == 0 {
		return 0, datetimeFieldOverflow("date field value out of range")
	}
	bc := year < 0
	mag := year // |year|; the displayed magnitude
	if bc {
		mag = -year
	}
	if mag > 999_999 {
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
	// Time fields (22008), matching PG float_time_overflows: hour 0..24, minute 0..59, sec rounded
	// to micros in [0, 60_000_000] (so sec = 60 is allowed), and the whole time of day ≤ 24:00:00.
	if hour < 0 || hour > 24 || minute < 0 || minute > 59 {
		return 0, datetimeFieldOverflow("time field value out of range")
	}
	secMicros := math.Round(sec * float64(microsPerSec)) // round-half-away (math.Round)
	if math.IsNaN(secMicros) || math.IsInf(secMicros, 0) || secMicros < 0 || secMicros > 60_000_000 {
		return 0, datetimeFieldOverflow("time field value out of range")
	}
	todMicros := ((hour*60+minute)*60)*microsPerSec + int64(secMicros) // ≤ 9e10, no i64 overflow
	if todMicros > secsPerDay*microsPerSec {
		return 0, datetimeFieldOverflow("time field value out of range")
	}
	// Compose the instant in checked i64 arithmetic; any overflow is a range error.
	micros, ok := mulAdd(daysFromCivil(astro, month, day), secsPerDay*microsPerSec, todMicros)
	if !ok {
		return 0, datetimeFieldOverflow("timestamp out of range")
	}
	if micros == negInfinity || micros == posInfinity {
		return 0, datetimeFieldOverflow("timestamp out of range") // reserved for ±infinity
	}
	return micros, nil
}

// --- rendering ---------------------------------------------------------------

// renderDatetime renders micros (a wall-clock reading) as ISO text. offset is the UT offset in
// seconds to suffix for a timestamptz (nil for a timestamp): PostgreSQL's EncodeTimezone spelling —
// +HH, +HH:MM when the minutes are nonzero, +HH:MM:SS when the seconds are (an LMT offset), placed
// before any " BC" era.
func renderDatetime(micros int64, offset *int32) string {
	return renderDatetimeStyled(micros, offset, false)
}

// renderDatetimeStyled is the shared body of renderDatetime (xsd false, PostgreSQL's ISO date style)
// and the JSON renders (xsd true, the XSD style JsonEncodeDateTime forces — json-sql-functions.md
// §2.2): XSD separates the date and time with T instead of a space, and always spells an offset's
// minutes (+00:00, not +00).
func renderDatetimeStyled(micros int64, offset *int32, xsd bool) string {
	if micros == negInfinity {
		return "-infinity"
	}
	if micros == posInfinity {
		return "infinity"
	}
	y, mo, d, h, mi, s, us := civilFromMicros(micros)
	displayed := y
	era := ""
	if y <= 0 {
		displayed = 1 - y
		era = " BC"
	}
	var b strings.Builder
	sep := byte(' ')
	if xsd {
		sep = 'T'
	}
	fmt.Fprintf(&b, "%04d-%02d-%02d%c%02d:%02d:%02d", displayed, mo, d, sep, h, mi, s)
	if us != 0 {
		frac := fmt.Sprintf("%06d", us)
		frac = strings.TrimRight(frac, "0")
		b.WriteByte('.')
		b.WriteString(frac)
	}
	if offset != nil {
		sign := byte('+')
		a := int64(*offset)
		if a < 0 {
			sign = '-'
			a = -a
		}
		h, mi, sec := a/3600, a/60%60, a%60
		b.WriteByte(sign)
		fmt.Fprintf(&b, "%02d", h)
		if sec != 0 {
			fmt.Fprintf(&b, ":%02d:%02d", mi, sec)
		} else if mi != 0 || xsd {
			fmt.Fprintf(&b, ":%02d", mi)
		}
	}
	b.WriteString(era)
	return b.String()
}

// RenderTimestamp renders a timestamp value to its canonical text.
func renderTimestamp(micros int64) string { return renderDatetime(micros, nil) }

// RenderTimestamptz renders a timestamptz value to its canonical text (always UTC, fixed +00).
func renderTimestamptz(micros int64) string {
	var utc int32
	return renderDatetime(micros, &utc)
}

// renderTimestamptzIn renders a timestamptz instant as text in zone — the timestamptz → text cast,
// which follows the session zone (timezones.md §9.5): the local wall clock plus the offset in effect
// at the instant. ±infinity render bare; a finite instant whose local reading leaves the timestamp
// range traps 22008 (timezones.md §6).
func renderTimestamptzIn(micros int64, zone ZoneRef) (string, error) {
	if micros == negInfinity || micros == posInfinity {
		return renderDatetime(micros, nil), nil
	}
	off := offsetAtRef(zone, floorDiv(micros, 1_000_000))
	local, ok := instantToLocalMicros(zone, micros)
	if !ok {
		return "", errZoneShiftOutOfRange()
	}
	return renderDatetime(local, &off.Utoff), nil
}

// renderTimestampXSD renders a timestamp value as its JSON string body (JsonEncodeDateTime, the XSD
// style — json-sql-functions.md §2.2): 2024-01-15T10:30:00.5, ±infinity bare, a " BC" suffix.
func renderTimestampXSD(micros int64) string { return renderDatetimeStyled(micros, nil, true) }

// renderTimestamptzXSDIn renders a timestamptz instant as its JSON string body in zone (the XSD
// style): the local wall clock with a T separator and an offset that always shows its minutes
// (+05:30, +00:00). ±infinity render bare; a local reading out of range traps 22008, as the text
// render does.
func renderTimestamptzXSDIn(micros int64, zone ZoneRef) (string, error) {
	if micros == negInfinity || micros == posInfinity {
		return renderDatetime(micros, nil), nil
	}
	off := offsetAtRef(zone, floorDiv(micros, 1_000_000))
	local, ok := instantToLocalMicros(zone, micros)
	if !ok {
		return "", errZoneShiftOutOfRange()
	}
	return renderDatetimeStyled(local, &off.Utoff, true), nil
}
