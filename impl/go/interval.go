package jed

import (
	"fmt"
	"math/big"
	"strings"
)

// The interval type — value model, parsing, and rendering (spec/design/interval.md). A value is
// PostgreSQL's three independent fields: Months (i32), Days (i32), Micros (i64). They are kept
// separate so `+ 1 month` is calendar-aware; comparison/ordering/dedup collapse them via the
// canonical 128-bit span (1 month = 30 days, 1 day = 24 h). Parsing is a port of PostgreSQL's
// interval_in (its unit syntax, the SQL-standard forms, ISO 8601); rendering is PG
// `IntervalStyle = postgres`. A §8 determinism hotspot: the
// fractional-unit cascade, the half-away µs rounding, and the render format must be byte-identical
// across the Rust/Go/TS cores. The cascade and span use math/big (Go has no int128), so the exact
// integer math matches Rust's i128 and TS's BigInt within the digit caps below.

const (
	microsPerDay  = int64(86_400) * microsPerSec // canonical "1 day = 24 h" span weight
	daysPerMonth  = int64(30)                    // canonical "1 month = 30 days"
	monthsPerYear = int64(12)
	maxIntDigits  = 18 // an interval × / number factor's digit bounds (spec/design/interval.md §5)
	maxFracDigits = 9
)

// Interval is a span of time — three independent fields. Comparison/ordering/dedup go through the
// canonical 128-bit span (Span), NOT the field triple, so `'1 mon'` == `'30 days'` == `'720:00:00'`.
type Interval struct {
	Months int32
	Days   int32
	Micros int64
}

// Span is the canonical comparison key: a signed 128-bit microsecond span combining the three
// fields via 1 month = 30 days and 1 day = 24 h (PG interval_cmp_value). Returned as a *big.Int
// (Go has no int128); the value is exact.
func (iv Interval) Span() *big.Int {
	days := int64(iv.Months)*daysPerMonth + int64(iv.Days) // fits i64 (i32*30 + i32)
	b := big.NewInt(days)
	b.Mul(b, big.NewInt(microsPerDay))
	b.Add(b, big.NewInt(iv.Micros))
	return b
}

// SpanCmp compares two intervals by their canonical span: -1, 0, or 1.
func (iv Interval) SpanCmp(o Interval) int { return iv.Span().Cmp(o.Span()) }

// EncodeKey is the order-preserving KEY body for an interval (method interval-span-i128,
// spec/design/encoding.md §2.10): the 16-byte order-preserving encoding of the canonical 128-bit
// Span — int-be-signflip at i128 width (add the bias 2^127, emit a 16-byte big-endian unsigned
// integer), mapping the signed span range monotonically onto [0, 2^128) so negatives sort below
// positives. Fixed-width 16, so self-delimiting with no escape/terminator (like uuid). Because the
// key is the Span, two field-distinct but span-equal intervals ('1 mon' / '30 days') produce
// identical bytes — a UNIQUE interval index treats them as one (the "equal but not identical"
// wrinkle, the decimal 1.5/1.50 precedent). A PK is NOT NULL, so the stored key is this bare
// 16-byte body. (math/big since Go has no int128; the bias keeps the value non-negative so
// FillBytes never panics.)
func (iv Interval) EncodeKey() []byte {
	biased := iv.Span()
	biased.Add(biased, new(big.Int).Lsh(big.NewInt(1), 127)) // + 2^127
	out := make([]byte, 16)
	return biased.FillBytes(out) // big-endian, left-zero-padded into the 16-byte buffer
}

// addI32 adds two int32s with overflow detection.
func addI32(a, b int32) (int32, bool) {
	s := int64(a) + int64(b)
	if s < -2147483648 || s > 2147483647 {
		return 0, false
	}
	return int32(s), true
}

// Add is field-wise interval addition (PG keeps the fields independent, no justification). An
// i32 month/day or i64 micros overflow traps 22008.
func (iv Interval) Add(o Interval) (Interval, error) {
	months, ok1 := addI32(iv.Months, o.Months)
	days, ok2 := addI32(iv.Days, o.Days)
	micros, ok3 := add64(iv.Micros, o.Micros)
	if !ok1 || !ok2 || !ok3 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	return Interval{Months: months, Days: days, Micros: micros}, nil
}

// Sub is field-wise interval subtraction. Overflow traps 22008.
func (iv Interval) Sub(o Interval) (Interval, error) {
	months, ok1 := addI32(iv.Months, -o.Months)
	days, ok2 := addI32(iv.Days, -o.Days)
	micros, ok3 := sub64(iv.Micros, o.Micros)
	// -o.Months / -o.Days can themselves overflow at i32::MIN; addI32 catches the result range,
	// but guard the negation operand too.
	if o.Months == -2147483648 || o.Days == -2147483648 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	if !ok1 || !ok2 || !ok3 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	return Interval{Months: months, Days: days, Micros: micros}, nil
}

// MakeInterval builds an interval from PostgreSQL make_interval's components (functions.md §11).
// years/months fold into the months field (×12), weeks/days into the days field (×7), and
// hours/mins plus the caller's pre-converted secMicros into the micros field — grouped
// (((hours*60)+mins)*60)*1e6 + secMicros like PG. All math here is exact integer (the one float
// step, secs → secMicros, lives in the executor so this stays float-free). Any i32 month/day or
// i64 micros overflow traps 22008.
func makeInterval(years, months, weeks, days, hours, mins, secMicros int64) (Interval, error) {
	monthsTotal, ok1 := mulAdd(years, monthsPerYear, months)
	daysTotal, ok2 := mulAdd(weeks, 7, days)
	hm, ok3 := mulAdd(hours, 60, mins) // total minutes
	sec, ok4 := mul64(hm, 60)          // total seconds
	micros, ok5 := mulAdd(sec, microsPerSec, secMicros)
	if !ok1 || !ok2 || !ok3 || !ok4 || !ok5 ||
		monthsTotal < -2147483648 || monthsTotal > 2147483647 ||
		daysTotal < -2147483648 || daysTotal > 2147483647 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	return Interval{Months: int32(monthsTotal), Days: int32(daysTotal), Micros: micros}, nil
}

// Neg negates all three fields. i32::MIN / i64::MIN would overflow → 22008.
func (iv Interval) Neg() (Interval, error) {
	if iv.Months == -2147483648 || iv.Days == -2147483648 || iv.Micros == -9223372036854775808 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	return Interval{Months: -iv.Months, Days: -iv.Days, Micros: -iv.Micros}, nil
}

// ParseFactorDecimal parses a canonical decimal string `[-]int[.frac]` into an exact fraction
// (num, den) with den = 10^len(frac) (value = num/den). Caps the digit counts (matching the
// Rust i128 cascade's bound) so all cores trap at the same factor size; over-long → 22008.
func parseFactorDecimal(s string) (*big.Int, *big.Int, error) {
	neg := strings.HasPrefix(s, "-")
	body := strings.TrimPrefix(s, "-")
	intPart, fracPart := body, ""
	if i := strings.IndexByte(body, '.'); i >= 0 {
		intPart, fracPart = body[:i], body[i+1:]
	}
	if len(intPart) > maxIntDigits || len(fracPart) > maxFracDigits {
		return nil, nil, intervalFieldOverflow("interval factor has too many digits")
	}
	num, ok := new(big.Int).SetString(intPart+fracPart, 10)
	if !ok {
		return nil, nil, intervalFieldOverflow("interval factor out of range")
	}
	if neg {
		num.Neg(num)
	}
	den := new(big.Int).Exp(big.NewInt(10), big.NewInt(int64(len(fracPart))), nil)
	return num, den, nil
}

// MulByFraction is the exact ×÷ cascade (spec/design/interval.md §5): scale each field by
// fnum/fden (fden > 0), cascading the fractional part months→days→micros, µs rounded half away
// from zero. EXACT (big.Int; mirrors Rust's i128). A field beyond i32/i64 traps 22008.
func mulByFraction(iv Interval, fnum, fden *big.Int) (Interval, error) {
	g := new(big.Int).GCD(nil, nil, new(big.Int).Abs(fnum), new(big.Int).Abs(fden))
	if g.Sign() == 0 {
		g = big.NewInt(1)
	}
	fn := new(big.Int).Quo(fnum, g)
	fd := new(big.Int).Quo(fden, g)
	m := big.NewInt(int64(iv.Months))
	d := big.NewInt(int64(iv.Days))
	u := big.NewInt(iv.Micros)
	of := func() error { return intervalFieldOverflow("interval out of range") }

	mTotal := new(big.Int).Mul(m, fn)
	rMonth := new(big.Int).Quo(mTotal, fd)
	fracMonth := new(big.Int).Sub(mTotal, new(big.Int).Mul(rMonth, fd))
	mrd := new(big.Int).Mul(fracMonth, big.NewInt(daysPerMonth))
	mrdWhole := new(big.Int).Quo(mrd, fd)
	mrdFrac := new(big.Int).Sub(mrd, new(big.Int).Mul(mrdWhole, fd))

	dTotal := new(big.Int).Mul(d, fn)
	rDayPart := new(big.Int).Quo(dTotal, fd)
	dayFrac := new(big.Int).Sub(dTotal, new(big.Int).Mul(rDayPart, fd))
	rDay := new(big.Int).Add(rDayPart, mrdWhole)

	timeNum := new(big.Int).Mul(u, fn)
	fracSum := new(big.Int).Add(dayFrac, mrdFrac)
	timeNum.Add(timeNum, new(big.Int).Mul(fracSum, big.NewInt(microsPerDay)))
	rTime := roundDivBig(timeNum, fd)

	if !rMonth.IsInt64() || !rDay.IsInt64() || !rTime.IsInt64() {
		return Interval{}, of()
	}
	mi, di, ti := rMonth.Int64(), rDay.Int64(), rTime.Int64()
	if mi < -2147483648 || mi > 2147483647 || di < -2147483648 || di > 2147483647 {
		return Interval{}, of()
	}
	return Interval{Months: int32(mi), Days: int32(di), Micros: ti}, nil
}

// TsShift computes ts + iv (or ts - iv with subtract) — the calendar-aware datetime arithmetic
// (spec/design/interval.md §5). Months are added first WITH DAY-OF-MONTH CLAMPING (Jan 31 + 1
// month -> Feb 28/29), then days (24 h each), then micros. Adding to ±infinity stays ±infinity; a
// finite result onto a sentinel or beyond the i64-µs range traps 22008.
func tsShift(ts int64, iv Interval, subtract bool) (int64, error) {
	if ts == negInfinity || ts == posInfinity {
		return ts, nil
	}
	sign := int64(1)
	if subtract {
		sign = -1
	}
	t := ts
	months := sign * int64(iv.Months)
	if months != 0 {
		y, mo, d, h, mi, s, us := civilFromMicros(t)
		total := y*12 + (mo - 1) + months
		ny := floorDiv(total, 12)
		nmo := floorMod(total, 12) + 1
		nd := d
		if maxd := daysInMonth(ny, nmo); nd > maxd {
			nd = maxd
		}
		days := daysFromCivil(ny, nmo, nd)
		secs, ok := mulAdd(days, secsPerDay, h*3600+mi*60+s)
		if !ok {
			return 0, intervalFieldOverflow("timestamp out of range")
		}
		t, ok = mulAdd(secs, microsPerSec, us)
		if !ok {
			return 0, intervalFieldOverflow("timestamp out of range")
		}
	}
	dayUS, ok := mul64(sign*int64(iv.Days), microsPerDay)
	if !ok {
		return 0, intervalFieldOverflow("timestamp out of range")
	}
	t, ok = add64(t, dayUS)
	if !ok {
		return 0, intervalFieldOverflow("timestamp out of range")
	}
	if subtract {
		t, ok = sub64(t, iv.Micros)
	} else {
		t, ok = add64(t, iv.Micros)
	}
	if !ok {
		return 0, intervalFieldOverflow("timestamp out of range")
	}
	if t == negInfinity || t == posInfinity {
		return 0, intervalFieldOverflow("timestamp out of range")
	}
	return t, nil
}

// TsDiff computes a - b of two timestamps (or timestamptz) → an interval, justified into days +
// time with months = 0 (PG timestamp_mi → interval_justify_hours). An ±infinity operand traps
// 22008; a day count beyond i32 traps 22008.
func tsDiff(a, b int64) (Interval, error) {
	if a == negInfinity || a == posInfinity || b == negInfinity || b == posInfinity {
		return Interval{}, intervalFieldOverflow("cannot subtract infinite timestamps")
	}
	micros, ok := sub64(a, b)
	if !ok {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	days := micros / microsPerDay
	rem := micros % microsPerDay
	if days < -2147483648 || days > 2147483647 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	return Interval{Months: 0, Days: int32(days), Micros: rem}, nil
}

// justifyHours is PostgreSQL justify_hours (spec/design/interval.md §5.1): whole 24 h chunks of
// Micros move into Days, then Days and Micros are given the same sign (borrowing one day). Months
// is untouched. A day count beyond i32 traps 22008.
func justifyHours(iv Interval) (Interval, error) {
	wholeDay := iv.Micros / microsPerDay
	micros := iv.Micros - wholeDay*microsPerDay
	d := int64(iv.Days) + wholeDay
	if d < -2147483648 || d > 2147483647 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	days := int32(d)
	if days > 0 && micros < 0 {
		micros += microsPerDay
		days--
	} else if days < 0 && micros > 0 {
		micros -= microsPerDay
		days++
	}
	return Interval{Months: iv.Months, Days: days, Micros: micros}, nil
}

// justifyDays is PostgreSQL justify_days (spec/design/interval.md §5.1): whole 30-day chunks of
// Days move into Months, then Months and Days are given the same sign (borrowing one month).
// Micros is untouched. A month count beyond i32 traps 22008.
func justifyDays(iv Interval) (Interval, error) {
	dpm := int32(daysPerMonth)
	wholeMonth := iv.Days / dpm
	days := iv.Days - wholeMonth*dpm
	months, ok := addI32(iv.Months, wholeMonth)
	if !ok {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	if months > 0 && days < 0 {
		days += dpm
		months--
	} else if months < 0 && days > 0 {
		days -= dpm
		months++
	}
	return Interval{Months: months, Days: days, Micros: iv.Micros}, nil
}

// justifyInterval is PostgreSQL justify_interval (spec/design/interval.md §5.1): both
// justifications, with 0 <= |Micros| < 24 h, 0 <= |Days| < 30, and all three fields made the same
// sign. Ported step for step from PG's interval_justify_interval, including its pre-justification
// of days (which keeps the day sum within i32). A month count beyond i32 traps 22008.
func justifyInterval(iv Interval) (Interval, error) {
	dpm := int32(daysPerMonth)
	months, days, micros := iv.Months, iv.Days, iv.Micros
	var ok bool
	// Pre-justify days when days and micros share a sign (it might prevent overflow).
	if (days > 0 && micros > 0) || (days < 0 && micros < 0) {
		wholeMonth := days / dpm
		days -= wholeMonth * dpm
		if months, ok = addI32(months, wholeMonth); !ok {
			return Interval{}, intervalFieldOverflow("interval out of range")
		}
	}
	wholeDay := micros / microsPerDay
	micros -= wholeDay * microsPerDay
	// Cannot overflow: |days| < 30 after the pre-justify, or days and wholeDay differ in sign.
	days += int32(wholeDay)
	wholeMonth := days / dpm
	days -= wholeMonth * dpm
	if months, ok = addI32(months, wholeMonth); !ok {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	if months > 0 && (days < 0 || (days == 0 && micros < 0)) {
		days += dpm
		months--
	} else if months < 0 && (days > 0 || (days == 0 && micros > 0)) {
		days -= dpm
		months++
	}
	if days > 0 && micros < 0 {
		micros += microsPerDay
		days--
	} else if days < 0 && micros > 0 {
		micros -= microsPerDay
		days++
	}
	return Interval{Months: months, Days: days, Micros: micros}, nil
}

// ageRejectInfinite is age(dt1, dt2)'s infinity rule (spec/design/interval.md §5.2). PostgreSQL
// returns an infinite interval for one infinite operand and raises 22008 "interval out of range"
// for the same infinity twice; jed has no infinite interval, so EVERY infinite operand is that
// 22008. Checked before any zone consultation, like PG's timestamp_age / timestamptz_age.
func ageRejectInfinite(dt1, dt2 int64) error {
	inf := func(t int64) bool { return t == negInfinity || t == posInfinity }
	if inf(dt1) || inf(dt2) {
		return intervalFieldOverflow("interval out of range")
	}
	return nil
}

// age is PostgreSQL age(dt1, dt2) (spec/design/interval.md §5.2), ported step for step from
// timestamp_age / timestamptz_age: the field-wise symbolic difference of the two wall clocks local1
// and local2 (a timestamp's own value; a timestamptz decomposed in the session zone), with the sign
// taken from comparing the values dt1 and dt2 themselves. Each field is subtracted, the result is
// flipped so the later value comes first, negative fields borrow from the next higher one (a day
// borrows the length of the EARLIER value's month), and the sign is restored. Both operands are
// finite (ageRejectInfinite ran first).
func age(dt1, dt2, local1, local2 int64) (Interval, error) {
	y1, mo1, d1, h1, mi1, s1, us1 := civilFromMicros(local1)
	y2, mo2, d2, h2, mi2, s2, us2 := civilFromMicros(local2)
	// Form the symbolic difference.
	usec, sec, minute, hour := us1-us2, s1-s2, mi1-mi2, h1-h2
	mday, mon, year := d1-d2, mo1-mo2, y1-y2
	flip := dt1 < dt2
	if flip {
		usec, sec, minute, hour = -usec, -sec, -minute, -hour
		mday, mon, year = -mday, -mon, -year
	}
	// Propagate any negative fields into the next higher field.
	for usec < 0 {
		usec += microsPerSec
		sec--
	}
	for sec < 0 {
		sec += 60
		minute--
	}
	for minute < 0 {
		minute += 60
		hour--
	}
	for hour < 0 {
		hour += 24
		mday--
	}
	for mday < 0 {
		if flip {
			mday += daysInMonth(y1, mo1)
		} else {
			mday += daysInMonth(y2, mo2)
		}
		mon--
	}
	for mon < 0 {
		mon += monthsPerYear
		year--
	}
	// Recover the sign.
	if flip {
		usec, sec, minute, hour = -usec, -sec, -minute, -hour
		mday, mon, year = -mday, -mon, -year
	}
	// PG itm2interval: the months total must fit i32, the time total i64.
	months, ok := mulAdd(year, monthsPerYear, mon)
	if !ok || months < -2147483648 || months > 2147483647 || mday < -2147483648 || mday > 2147483647 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	micros, ok := mul64(hour, 3600*microsPerSec)
	if ok {
		micros, ok = add64(micros, minute*60*microsPerSec)
	}
	if ok {
		micros, ok = add64(micros, sec*microsPerSec)
	}
	if ok {
		micros, ok = add64(micros, usec)
	}
	if !ok {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	return Interval{Months: int32(months), Days: int32(mday), Micros: micros}, nil
}

// --- parsing -----------------------------------------------------------------

func invalidInterval(detail string) error       { return newError(InvalidDatetimeFormat, detail) }
func intervalFieldOverflow(detail string) error { return newError(DatetimeFieldOverflow, detail) }

// roundDivBig rounds num/den to the nearest integer, half away from zero (the engine's one
// rounding mode). den > 0.
func roundDivBig(num, den *big.Int) *big.Int {
	q := new(big.Int)
	r := new(big.Int)
	q.QuoRem(num, den, r) // q truncated toward zero, r has the sign of num
	twice := new(big.Int).Abs(r)
	twice.Lsh(twice, 1) // 2*|r|
	if twice.Cmp(den) >= 0 {
		if num.Sign() >= 0 {
			q.Add(q, big.NewInt(1))
		} else {
			q.Sub(q, big.NewInt(1))
		}
	}
	return q
}

// The interval text parser is a port of PostgreSQL 18's interval_in (datetime.c): the input is
// broken into fields (ParseDateTime), the fields are read right to left (DecodeInterval), and only
// if that finds the input malformed is it read as ISO 8601 (DecodeISO8601Interval). The one
// numeric difference (spec/design/interval.md §3): PG carries a field's fraction in a double; jed
// carries it exactly, as a signed numerator over 10^25 (fracDen), and rounds the µs half away from
// zero (the engine's one mode).

// dtErr is PostgreSQL's datetime-decode result: dtBadFormat (DTERR_BAD_FORMAT, 22007) or
// dtFieldOverflow (DTERR_FIELD_OVERFLOW, PG's 22015; jed's datetime field overflow 22008).
type dtErr int

const (
	dtOK dtErr = iota
	dtBadFormat
	dtFieldOverflow
)

// fracDigits / fracDen: a field's fractional part is numerator / 10^25, its digits past the 25th
// dropped (far below a microsecond for every unit).
const fracDigits = 25

var fracDen = new(big.Int).Exp(big.NewInt(10), big.NewInt(fracDigits), nil)

const (
	usecsPerSec    = microsPerSec
	usecsPerMinute = 60 * usecsPerSec
	usecsPerHour   = 3600 * usecsPerSec
)

// itmIn is PG's struct pg_itm_in: the fields an interval is assembled from. Years stay apart from
// months until the end (itmin2interval).
type itmIn struct {
	usec int64
	mday int32
	mon  int32
	year int32
}

// PG's DTK_* unit codes — the values DecodeInterval's `type` carries between fields.
const (
	dtkNumber     = 0 // the type after `ago`: no number may precede it
	dtkTZ         = 4
	dtkEarly      = 9
	dtkLate       = 10
	ignoreDTF     = 8 // the initial type: the rightmost unitless number is seconds
	dtkSecond     = 18
	dtkMinute     = 19
	dtkHour       = 20
	dtkDay        = 21
	dtkWeek       = 22
	dtkMonth      = 23
	dtkQuarter    = 24
	dtkYear       = 25
	dtkDecade     = 26
	dtkCentury    = 27
	dtkMillennium = 28
	dtkMillisec   = 29
	dtkMicrosec   = 30
	dtkJulian     = 31
	dtkDOW        = 32
	dtkDOY        = 33
	dtkISOYear    = 36
	dtkISODOW     = 37
)

// The per-field masks of DecodeInterval (PG DTK_M): each field claims its bits, and a field whose
// bits overlap an already-read field's is a duplicate (22007). week and day are DIFFERENT fields,
// as are year/decade/century/millennium and second/millisecond/microsecond.
const (
	fieldMonth       uint32 = 1 << 1
	fieldYear        uint32 = 1 << 2
	fieldDay         uint32 = 1 << 3
	fieldHour        uint32 = 1 << 10
	fieldMinute      uint32 = 1 << 11
	fieldSecond      uint32 = 1 << 12
	fieldMillisecond uint32 = 1 << 13
	fieldMicrosecond uint32 = 1 << 14
	fieldWeek        uint32 = 1 << 24
	fieldDecade      uint32 = 1 << 25
	fieldCentury     uint32 = 1 << 26
	fieldMillennium  uint32 = 1 << 27
	// seconds with a fractional part (PG DTK_ALL_SECS_M)
	fieldAllSecs = fieldSecond | fieldMillisecond | fieldMicrosecond
	// a time HH:MM[:SS[.ffffff]] (PG DTK_TIME_M)
	fieldTime = fieldHour | fieldMinute | fieldAllSecs
	// PG DTK_DATE_M
	fieldDate = fieldYear | fieldMonth | fieldDay
)

// PG's MAXDATEFIELDS and interval_in's 256-byte field buffer: more fields, or more field text, is
// malformed.
const (
	maxDateFields = 25
	fieldBufLen   = 256
)

// isPGSpace is isspace in the C locale.
func isPGSpace(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n' || b == '\v' || b == '\f' || b == '\r'
}

func isAlpha(b byte) bool {
	return (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z')
}

func isDigitByte(b byte) bool { return b >= '0' && b <= '9' }
func isAlnumByte(b byte) bool { return isDigitByte(b) || isAlpha(b) }
func isHexByte(b byte) bool {
	return isDigitByte(b) || (b >= 'a' && b <= 'f') || (b >= 'A' && b <= 'F')
}

// isPunctByte is ispunct in the C locale (ASCII punctuation).
func isPunctByte(b byte) bool {
	return (b >= '!' && b <= '/') || (b >= ':' && b <= '@') || (b >= '[' && b <= '`') || (b >= '{' && b <= '~')
}

func lowerByte(b byte) byte {
	if b >= 'A' && b <= 'Z' {
		return b + ('a' - 'A')
	}
	return b
}

// fieldType is the kind of one field (ParseDateTime's ftype).
type fieldType int

const (
	ftNumber  fieldType = iota // digits, possibly with one `.` fraction (5, 1.5, .5)
	ftString                   // a lowercased word (day, ago)
	ftDate                     // digits with - / . separators, or a word run into digits (1-2, 5., p1d)
	ftTime                     // digits then `:` (1:00:00.5)
	ftTZ                       // a sign then digits, `:`, `.`, `-` (-1, +02:03, -1-2)
	ftSpecial                  // a sign then a word (-infinity)
)

type dtField struct {
	ftype fieldType
	text  []byte
}

// parseDateTime is PostgreSQL's ParseDateTime, as interval_in uses it: split the input into
// fields. Whitespace separates fields; any other punctuation starting a field is a separator too;
// text is lowercased.
func parseDateTime(input string) ([]dtField, dtErr) {
	at := func(k int) byte {
		if k < len(input) {
			return input[k]
		}
		return 0
	}
	var fields []dtField
	// bytes of PG's field buffer used by the finished fields (each field's text plus its NUL)
	bufUsed := 0
	i := 0
	for i < len(input) {
		c := input[i]
		if isPGSpace(c) {
			i++
			continue
		}
		if len(fields) >= maxDateFields {
			return nil, dtBadFormat
		}
		var text []byte
		// PG's APPEND_CHAR: the buffer must keep room for the field's terminating NUL (a field
		// that overruns it is malformed, reported once the field is read)
		full := false
		push := func(ch byte) {
			if bufUsed+len(text)+1 >= fieldBufLen {
				full = true
				return
			}
			text = append(text, ch)
		}
		var ftype fieldType
		switch {
		case isDigitByte(c):
			for isDigitByte(at(i)) {
				push(at(i))
				i++
			}
			if at(i) == ':' {
				// a time field
				ftype = ftTime
				push(':')
				i++
				for isDigitByte(at(i)) || at(i) == ':' || at(i) == '.' {
					push(at(i))
					i++
				}
			} else if at(i) == '-' || at(i) == '/' || at(i) == '.' {
				// a date field (or a number with a fraction)
				delim := at(i)
				push(delim)
				i++
				if isDigitByte(at(i)) {
					ftype = ftDate
					if delim == '.' {
						ftype = ftNumber
					}
					for isDigitByte(at(i)) {
						push(at(i))
						i++
					}
					// a second matching delimiter makes a three-field date
					if at(i) == delim {
						ftype = ftDate
						push(delim)
						i++
						for isDigitByte(at(i)) || at(i) == delim {
							push(at(i))
							i++
						}
					}
				} else {
					ftype = ftDate
					for isAlnumByte(at(i)) || at(i) == delim {
						push(lowerByte(at(i)))
						i++
					}
				}
			} else {
				ftype = ftNumber
			}
		case c == '.':
			// a leading decimal point: fractional seconds
			push('.')
			i++
			for isDigitByte(at(i)) {
				push(at(i))
				i++
			}
			ftype = ftNumber
		case isAlpha(c):
			ftype = ftString
			for isAlpha(at(i)) {
				push(lowerByte(at(i)))
				i++
			}
			// A word may run into a date or zone name — unless it is a known date keyword.
			isDate := false
			if at(i) == '-' || at(i) == '/' || at(i) == '.' {
				isDate = true
			} else if at(i) == '+' || isDigitByte(at(i)) {
				_, known := dateKeyword(string(text))
				isDate = !known
			}
			if isDate {
				ftype = ftDate
				for {
					push(lowerByte(at(i)))
					i++
					n := at(i)
					if !(n == '+' || n == '-' || n == '/' || n == '_' || n == '.' || n == ':' || isAlnumByte(n)) {
						break
					}
				}
			}
		case c == '+' || c == '-':
			push(c)
			i++
			// whitespace after the sign is skipped
			for isPGSpace(at(i)) {
				i++
			}
			if isDigitByte(at(i)) {
				ftype = ftTZ
				push(at(i))
				i++
				for isDigitByte(at(i)) || at(i) == ':' || at(i) == '.' || at(i) == '-' {
					push(at(i))
					i++
				}
			} else if isAlpha(at(i)) {
				ftype = ftSpecial
				for isAlpha(at(i)) {
					push(lowerByte(at(i)))
					i++
				}
			} else {
				return nil, dtBadFormat
			}
		case isPunctByte(c):
			// other punctuation only separates fields
			i++
			continue
		default:
			return nil, dtBadFormat
		}
		if full {
			return nil, dtBadFormat
		}
		bufUsed += len(text) + 1
		fields = append(fields, dtField{ftype: ftype, text: text})
	}
	return fields, dtOK
}

// wordKind is what a word means to DecodeInterval.
type wordKind int

const (
	wordUnknown wordKind = iota
	wordIgnore           // skipped (at, on)
	wordUnits            // a unit: the DTK_* code it gives the number to its left
	wordAgo              // ago
	wordReserv           // a reserved word (infinity, epoch, …): the DTK_* code
	wordOther            // a date keyword with no meaning in an interval (a month name, am, …)
)

// intervalWord is PG's interval unit table (deltatktbl), then its date keyword table (datetktbl)
// — the lookup DecodeUnits then DecodeSpecial do. PG compares at most 10 characters, so any word
// starting with a 10-letter entry (millennium, microsecon, millisecon) is that unit. jed's one
// difference: a bare `m` (PG's minute) is ambiguous with month and is rejected.
func intervalWord(w string) (wordKind, int) {
	if w == "m" {
		return wordUnknown, 0
	}
	if strings.HasPrefix(w, "millennium") {
		return wordUnits, dtkMillennium
	}
	if strings.HasPrefix(w, "microsecon") {
		return wordUnits, dtkMicrosec
	}
	if strings.HasPrefix(w, "millisecon") {
		return wordUnits, dtkMillisec
	}
	switch w {
	case "ago":
		return wordAgo, 0
	case "c", "cent", "centuries", "century":
		return wordUnits, dtkCentury
	case "d", "day", "days":
		return wordUnits, dtkDay
	case "dec", "decade", "decades", "decs":
		return wordUnits, dtkDecade
	case "h", "hour", "hours", "hr", "hrs":
		return wordUnits, dtkHour
	case "mil", "millennia", "millennium", "mils":
		return wordUnits, dtkMillennium
	case "min", "mins", "minute", "minutes":
		return wordUnits, dtkMinute
	case "mon", "mons", "month", "months":
		return wordUnits, dtkMonth
	case "ms", "msec", "msecond", "mseconds", "msecs":
		return wordUnits, dtkMillisec
	case "qtr", "quarter":
		return wordUnits, dtkQuarter
	case "s", "sec", "second", "seconds", "secs":
		return wordUnits, dtkSecond
	case "timezone":
		return wordUnits, dtkTZ
	case "us", "usec", "usecond", "useconds", "usecs":
		return wordUnits, dtkMicrosec
	case "w", "week", "weeks":
		return wordUnits, dtkWeek
	case "y", "year", "years", "yr", "yrs":
		return wordUnits, dtkYear
	}
	k, _ := dateKeyword(w)
	return k.kind, k.value
}

type wordMeaning struct {
	kind  wordKind
	value int
}

// dateKeyword is PG's date keyword table (datetktbl), as an interval reads it.
func dateKeyword(w string) (wordMeaning, bool) {
	switch w {
	case "at", "on":
		return wordMeaning{wordIgnore, 0}, true
	case "infinity", "+infinity":
		return wordMeaning{wordReserv, dtkLate}, true
	case "-infinity":
		return wordMeaning{wordReserv, dtkEarly}, true
	case "allballs", "epoch", "now", "today", "tomorrow", "yesterday":
		return wordMeaning{wordReserv, dtkNumber}, true
	case "d":
		return wordMeaning{wordUnits, dtkDay}, true
	case "dow":
		return wordMeaning{wordUnits, dtkDOW}, true
	case "doy":
		return wordMeaning{wordUnits, dtkDOY}, true
	case "h":
		return wordMeaning{wordUnits, dtkHour}, true
	case "isodow":
		return wordMeaning{wordUnits, dtkISODOW}, true
	case "isoyear":
		return wordMeaning{wordUnits, dtkISOYear}, true
	case "j", "jd", "julian":
		return wordMeaning{wordUnits, dtkJulian}, true
	case "m":
		return wordMeaning{wordUnits, dtkMonth}, true
	case "mm":
		return wordMeaning{wordUnits, dtkMinute}, true
	case "s":
		return wordMeaning{wordUnits, dtkSecond}, true
	case "y":
		return wordMeaning{wordUnits, dtkYear}, true
	case "ad", "am", "apr", "april", "aug", "august", "bc", "dec", "december", "dst", "feb",
		"february", "fri", "friday", "jan", "january", "jul", "july", "jun", "june", "mar",
		"march", "may", "mon", "monday", "nov", "november", "oct", "october", "pm", "sat",
		"saturday", "sep", "sept", "september", "sun", "sunday", "t", "thu", "thur", "thurs",
		"thursday", "tue", "tues", "tuesday", "wed", "wednesday", "weds":
		return wordMeaning{wordOther, 0}, true
	}
	return wordMeaning{wordUnknown, 0}, false
}

// strtoi64 is C strtol (base 10) as PG's strtoi64 uses it on a field: an optional sign then
// digits. It returns the value and the bytes consumed — 0 consumed (value 0) when there are no
// digits. A value beyond int64 is ERANGE, a field overflow.
func strtoi64(s []byte) (int64, int, dtErr) {
	i := 0
	neg := false
	if len(s) > 0 && (s[0] == '-' || s[0] == '+') {
		neg = s[0] == '-'
		i = 1
	}
	start := i
	var v uint64
	overflow := false
	for i < len(s) && isDigitByte(s[i]) {
		if !overflow {
			d := uint64(s[i] - '0')
			if v > (1<<63)/10 || v*10+d > 1<<63 {
				overflow = true
			} else {
				v = v*10 + d
			}
		}
		i++
	}
	if i == start {
		return 0, 0, dtOK
	}
	if overflow || (!neg && v > 1<<63-1) {
		return 0, 0, dtFieldOverflow
	}
	if neg {
		return int64(-v), i, dtOK // v <= 2^63, so -v wraps to the right int64
	}
	return int64(v), i, dtOK
}

// strtoint is PG's strtoint: strtol limited to int32 (beyond it is ERANGE, a field overflow).
func strtoint(s []byte) (int32, int, dtErr) {
	v, n, e := strtoi64(s)
	if e != dtOK {
		return 0, 0, e
	}
	if v < -2147483648 || v > 2147483647 {
		return 0, 0, dtFieldOverflow
	}
	return int32(v), n, dtOK
}

// fracFromDigits is the first fracDigits of a digit run as a fraction numerator over fracDen. A
// fraction whose nonzero digits all lie past the 25th stays nonzero (numerator 1) — too small to
// move any field, but still "a fraction" (it decides whether seconds claim the sub-second fields).
func fracFromDigits(digits []byte) *big.Int {
	buf := make([]byte, fracDigits)
	for k := 0; k < fracDigits; k++ {
		buf[k] = '0'
		if k < len(digits) {
			buf[k] = digits[k]
		}
	}
	num, _ := new(big.Int).SetString(string(buf), 10)
	if num.Sign() == 0 {
		for _, d := range digits {
			if d != '0' {
				return big.NewInt(1)
			}
		}
	}
	return num
}

// parseFraction is PG's ParseFraction: s is `.` then digits (or nothing) and nothing else.
func parseFraction(s []byte) (*big.Int, dtErr) {
	digits := s[1:]
	for _, d := range digits {
		if !isDigitByte(d) {
			return nil, dtBadFormat
		}
	}
	return fracFromDigits(digits), dtOK
}

// parseFractionalSecond is ParseFractionalSecond: a `.digits` fraction of a second to whole µs —
// 0–6 digits exact, 7+ rounded half away from zero on the 7th digit (jed's one mode; PG rounds a
// double half to even).
func parseFractionalSecond(s []byte) (int64, dtErr) {
	digits := s[1:]
	for _, d := range digits {
		if !isDigitByte(d) {
			return 0, dtBadFormat
		}
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
	return us, dtOK
}

// --- the Adjust* helpers of datetime.c: add a scaled value into the fields, failing on overflow ---

// adjustFractMicros is AdjustFractMicroseconds: frac * scale µs, rounded half away from zero.
func adjustFractMicros(frac *big.Int, scale int64, itm *itmIn) dtErr {
	if frac.Sign() == 0 {
		return dtOK
	}
	usec := roundDivBig(new(big.Int).Mul(frac, big.NewInt(scale)), fracDen)
	if !usec.IsInt64() {
		return dtFieldOverflow
	}
	s, ok := add64(itm.usec, usec.Int64())
	if !ok {
		return dtFieldOverflow
	}
	itm.usec = s
	return dtOK
}

// adjustFractDays is AdjustFractDays: frac * scale days — whole days, then the remaining fraction
// of a day as µs.
func adjustFractDays(frac *big.Int, scale int64, itm *itmIn) dtErr {
	if frac.Sign() == 0 {
		return dtOK
	}
	total := new(big.Int).Mul(frac, big.NewInt(scale))
	extraDays, rem := new(big.Int).QuoRem(total, fracDen, new(big.Int)) // truncate toward zero
	d, ok := addI32(itm.mday, int32(extraDays.Int64()))
	if !ok {
		return dtFieldOverflow
	}
	itm.mday = d
	return adjustFractMicros(rem, microsPerDay, itm)
}

// adjustFractYears is AdjustFractYears: frac * scale years as whole months, rounded half to even
// (PG's rint); the rest of a month is dropped, as in PG.
func adjustFractYears(frac *big.Int, scale int64, itm *itmIn) dtErr {
	total := new(big.Int).Mul(frac, big.NewInt(scale*monthsPerYear))
	extra, rem := new(big.Int).QuoRem(total, fracDen, new(big.Int))
	twiceRem := new(big.Int).Abs(rem)
	twiceRem.Lsh(twiceRem, 1)
	if c := twiceRem.Cmp(fracDen); c > 0 || (c == 0 && extra.Bit(0) != 0) {
		extra.Add(extra, big.NewInt(int64(total.Sign())))
	}
	m, ok := addI32(itm.mon, int32(extra.Int64()))
	if !ok {
		return dtFieldOverflow
	}
	itm.mon = m
	return dtOK
}

// adjustMicros is AdjustMicroseconds: (val + frac) * scale µs.
func adjustMicros(val int64, frac *big.Int, scale int64, itm *itmIn) dtErr {
	p, ok := mul64(val, scale)
	if ok {
		p, ok = add64(itm.usec, p)
	}
	if !ok {
		return dtFieldOverflow
	}
	itm.usec = p
	return adjustFractMicros(frac, scale, itm)
}

// toInt32 narrows an int64, ok=false beyond int32.
func toInt32(v int64) (int32, bool) {
	if v < -2147483648 || v > 2147483647 {
		return 0, false
	}
	return int32(v), true
}

// adjustDays is AdjustDays: val * scale days.
func adjustDays(val int64, scale int32, itm *itmIn) dtErr {
	v, ok := toInt32(val)
	if !ok {
		return dtFieldOverflow
	}
	d, ok := toInt32(int64(v) * int64(scale))
	if ok {
		d, ok = addI32(itm.mday, d)
	}
	if !ok {
		return dtFieldOverflow
	}
	itm.mday = d
	return dtOK
}

// adjustMonths is AdjustMonths: val months.
func adjustMonths(val int64, itm *itmIn) dtErr {
	v, ok := toInt32(val)
	if ok {
		v, ok = addI32(itm.mon, v)
	}
	if !ok {
		return dtFieldOverflow
	}
	itm.mon = v
	return dtOK
}

// adjustYears is AdjustYears: val * scale years.
func adjustYears(val int64, scale int32, itm *itmIn) dtErr {
	v, ok := toInt32(val)
	if !ok {
		return dtFieldOverflow
	}
	y, ok := toInt32(int64(v) * int64(scale))
	if ok {
		y, ok = addI32(itm.year, y)
	}
	if !ok {
		return dtFieldOverflow
	}
	itm.year = y
	return dtOK
}

// decodeTimeForInterval is PG's DecodeTimeForInterval (DecodeTimeCommon): a time field H:M,
// H:M:S[.f], or M:S.f (two parts with a fraction are minutes and seconds). The time REPLACES
// itm.usec, as in PG — so a fractional unit to the time's left (read earlier) loses its µs part.
func decodeTimeForInterval(s []byte, itm *itmIn) dtErr {
	hour, n, e := strtoi64(s)
	if e != dtOK {
		return e
	}
	cp := s[n:]
	if len(cp) == 0 || cp[0] != ':' {
		return dtBadFormat
	}
	minute, n, e := strtoint(cp[1:])
	if e != dtOK {
		return e
	}
	cp = cp[1+n:]
	var sec int32
	var fsec int64
	switch {
	case len(cp) == 0:
	case cp[0] == '.':
		// mm:ss.fff
		if fsec, e = parseFractionalSecond(cp); e != dtOK {
			return e
		}
		if hour < -2147483648 || hour > 2147483647 {
			return dtFieldOverflow
		}
		sec = minute
		minute = int32(hour)
		hour = 0
	case cp[0] == ':':
		s2, n, e := strtoint(cp[1:])
		if e != dtOK {
			return e
		}
		sec = s2
		cp = cp[1+n:]
		if len(cp) > 0 {
			if cp[0] != '.' {
				return dtBadFormat
			}
			if fsec, e = parseFractionalSecond(cp); e != dtOK {
				return e
			}
		}
	default:
		return dtBadFormat
	}
	if hour < 0 || minute < 0 || minute > 59 || sec < 0 || sec > 60 || fsec > usecsPerSec {
		return dtFieldOverflow
	}
	usec, ok := mul64(hour, usecsPerHour)
	if ok {
		usec, ok = add64(usec, int64(minute)*usecsPerMinute)
	}
	if ok {
		usec, ok = add64(usec, int64(sec)*usecsPerSec)
	}
	if ok {
		usec, ok = add64(usec, fsec)
	}
	if !ok {
		return dtFieldOverflow
	}
	itm.usec = usec
	return dtOK
}

// decodeInterval is PG's DecodeInterval (IntervalStyle postgres, no field qualifier): read the
// fields right to left, so a unit word is met before the number it applies to. A number with no
// unit takes the last unit read: seconds at the right end, then whatever unit was read last —
// except that after an hour or a time field the next unitless number is days ('1 1:00',
// '5 1 hour'). N-M is years-months. infinite reports `infinity` / `-infinity`.
func decodeInterval(fields []dtField, itm *itmIn) (infinite bool, err dtErr) {
	isBefore := false
	parsingUnitVal := false
	var fmask uint32
	ty := ignoreDTF
	nf := len(fields)
	for i := nf - 1; i >= 0; i-- {
		f := fields[i]
		var tmask uint32
		ftype := f.ftype
		if ftype == ftTime {
			if e := decodeTimeForInterval(f.text, itm); e != dtOK {
				return false, e
			}
			tmask = fieldTime
			ty = dtkDay
			parsingUnitVal = false
		} else if ftype == ftTZ {
			// A signed time (-1:00, +02:03) — else a signed number, read below.
			if strings.IndexByte(string(f.text[1:]), ':') >= 0 && decodeTimeForInterval(f.text[1:], itm) == dtOK {
				if f.text[0] == '-' {
					u, ok := neg64(itm.usec)
					if !ok {
						return false, dtFieldOverflow
					}
					itm.usec = u
				}
				tmask = fieldTime
				ty = dtkDay
				parsingUnitVal = false
			} else {
				ftype = ftNumber
			}
		}
		switch ftype {
		case ftTime, ftTZ:
		case ftNumber, ftDate:
			if ty == ignoreDTF {
				ty = dtkSecond
			}
			s := f.text
			val, n, e := strtoi64(s)
			if e != dtOK {
				return false, e
			}
			cp := s[n:]
			negative := s[0] == '-'
			fval := new(big.Int)
			switch {
			case len(cp) == 0:
			case cp[0] == '-':
				// SQL-standard years-months
				v2, n2, e := strtoint(cp[1:])
				if e != dtOK {
					return false, e
				}
				if v2 < 0 || int64(v2) >= monthsPerYear {
					return false, dtFieldOverflow
				}
				if len(cp) != 1+n2 {
					return false, dtBadFormat
				}
				ty = dtkMonth
				if negative {
					v2 = -v2
				}
				m, ok := mul64(val, monthsPerYear)
				if ok {
					m, ok = add64(m, int64(v2))
				}
				if !ok {
					return false, dtFieldOverflow
				}
				val = m
			case cp[0] == '.':
				fv, e := parseFraction(cp)
				if e != dtOK {
					return false, e
				}
				fval = fv
				if negative {
					fval.Neg(fval)
				}
			default:
				return false, dtBadFormat
			}
			var e2 dtErr
			switch ty {
			case dtkMicrosec:
				e2 = adjustMicros(val, fval, 1, itm)
				tmask = fieldMicrosecond
			case dtkMillisec:
				e2 = adjustMicros(val, fval, 1000, itm)
				tmask = fieldMillisecond
			case dtkSecond:
				e2 = adjustMicros(val, fval, usecsPerSec, itm)
				// fractional seconds claim the millisecond and microsecond fields too
				tmask = fieldSecond
				if fval.Sign() != 0 {
					tmask = fieldAllSecs
				}
			case dtkMinute:
				e2 = adjustMicros(val, fval, usecsPerMinute, itm)
				tmask = fieldMinute
			case dtkHour:
				e2 = adjustMicros(val, fval, usecsPerHour, itm)
				tmask = fieldHour
				ty = dtkDay // a unitless number left of an hour is days
			case dtkDay:
				if e2 = adjustDays(val, 1, itm); e2 == dtOK {
					e2 = adjustFractMicros(fval, microsPerDay, itm)
				}
				tmask = fieldDay
			case dtkWeek:
				if e2 = adjustDays(val, 7, itm); e2 == dtOK {
					e2 = adjustFractDays(fval, 7, itm)
				}
				tmask = fieldWeek
			case dtkMonth:
				if e2 = adjustMonths(val, itm); e2 == dtOK {
					e2 = adjustFractDays(fval, daysPerMonth, itm)
				}
				tmask = fieldMonth
			case dtkYear, dtkDecade, dtkCentury, dtkMillennium:
				scale, mask := int32(1), fieldYear
				switch ty {
				case dtkDecade:
					scale, mask = 10, fieldDecade
				case dtkCentury:
					scale, mask = 100, fieldCentury
				case dtkMillennium:
					scale, mask = 1000, fieldMillennium
				}
				if e2 = adjustYears(val, scale, itm); e2 == dtOK {
					e2 = adjustFractYears(fval, int64(scale), itm)
				}
				tmask = mask
			default:
				return false, dtBadFormat
			}
			if e2 != dtOK {
				return false, e2
			}
			parsingUnitVal = false
		case ftString, ftSpecial:
			// two unit words in a row
			if parsingUnitVal {
				return false, dtBadFormat
			}
			kind, value := intervalWord(string(f.text))
			switch kind {
			case wordIgnore:
				// skipped — but, as in PG, it resets the unit a number to its left takes
				ty = ignoreDTF
				continue
			case wordUnits:
				ty = value
				parsingUnitVal = true
			case wordAgo:
				// `ago` only as the last field
				if i != nf-1 {
					return false, dtBadFormat
				}
				isBefore = true
				ty = dtkNumber
			case wordReserv:
				tmask = fieldDate | fieldTime
				// only infinity / -infinity, and nothing after it
				if (value != dtkLate && value != dtkEarly) || i != nf-1 {
					return false, dtBadFormat
				}
				infinite = true
			default:
				return false, dtBadFormat
			}
		}
		if tmask&fmask != 0 {
			return false, dtBadFormat
		}
		fmask |= tmask
	}
	// at least one field, and no unit left without its number
	if fmask == 0 || parsingUnitVal {
		return false, dtBadFormat
	}
	if isBefore {
		u, ok := neg64(itm.usec)
		if !ok || itm.mday == -2147483648 || itm.mon == -2147483648 || itm.year == -2147483648 {
			return false, dtFieldOverflow
		}
		itm.usec, itm.mday, itm.mon, itm.year = u, -itm.mday, -itm.mon, -itm.year
	}
	return infinite, dtOK
}

// strtod's range limits, to 40 significant digits: a decimal at or above the overflow threshold
// (2^1024 − 2^970, ~1.8e308) rounds to infinity, and a nonzero one below the underflow threshold
// (2^-1022 − 2^-1076, ~2.2e-308) is too small for a normal double — both ERANGE, which PG reports
// as malformed input.
const (
	strtodOverflowDigits  = "1797693134862315807937289714053034150799"
	strtodUnderflowDigits = "2225073858507201259573821257020768020077"
)

// cmpLeadDigits compares a significand's leading 40 digits (zero-padded) with a 40-digit threshold.
func cmpLeadDigits(d []byte, t string) int {
	for k := 0; k < len(t); k++ {
		dk := byte('0')
		if k < len(d) {
			dk = d[k]
		}
		if dk != t[k] {
			if dk < t[k] {
				return -1
			}
			return 1
		}
	}
	return 0
}

// readExponentPart reads the signed-exponent tail [eE|pP][+-]digits of a strtod number at s[*i],
// only consuming it when digits follow; the value saturates far beyond any meaningful exponent.
func readExponentPart(s string, i *int, marker byte) int64 {
	at := func(k int) byte {
		if k < len(s) {
			return s[k]
		}
		return 0
	}
	if lowerByte(at(*i)) != marker {
		return 0
	}
	j := *i + 1
	neg := false
	if at(j) == '+' || at(j) == '-' {
		neg = at(j) == '-'
		j++
	}
	if !isDigitByte(at(j)) {
		return 0
	}
	var e int64
	for isDigitByte(at(j)) {
		e = e*10 + int64(at(j)-'0')
		if e > 1_000_000_000_000 {
			e = 1_000_000_000_000
		}
		j++
	}
	*i = j
	if neg {
		return -e
	}
	return e
}

// scanDecimal reads a decimal strtod number digits[.digits][e[+-]digits] at s[i:] (no sign) as
// (integer part, fraction numerator over fracDen, end), or the ParseISO8601Number error: a value
// strtod would report ERANGE for is malformed, one beyond ±1e15 a field overflow.
func scanDecimal(s string, i int) (int64, *big.Int, int, dtErr) {
	var digits []byte
	var fracCount int64
	sawDigit := false
	for i < len(s) && isDigitByte(s[i]) {
		digits = append(digits, s[i])
		sawDigit = true
		i++
	}
	if i < len(s) && s[i] == '.' && (sawDigit || (i+1 < len(s) && isDigitByte(s[i+1]))) {
		i++
		for i < len(s) && isDigitByte(s[i]) {
			digits = append(digits, s[i])
			fracCount++
			sawDigit = true
			i++
		}
	}
	if !sawDigit {
		return 0, nil, 0, dtBadFormat
	}
	// value = significand * 10^exp, the significand without leading or trailing zeros
	exp := readExponentPart(s, &i, 'e') - fracCount
	first := 0
	for first < len(digits) && digits[first] == '0' {
		first++
	}
	if first == len(digits) {
		return 0, new(big.Int), i, dtOK
	}
	sig := digits[first:]
	for sig[len(sig)-1] == '0' {
		sig = sig[:len(sig)-1]
		exp++
	}
	// the power of ten of the leading digit
	lead := int64(len(sig)) - 1 + exp
	if lead > 308 || (lead == 308 && cmpLeadDigits(sig, strtodOverflowDigits) > 0) {
		return 0, nil, 0, dtBadFormat
	}
	if lead < -308 || (lead == -308 && cmpLeadDigits(sig, strtodUnderflowDigits) <= 0) {
		return 0, nil, 0, dtBadFormat
	}
	if lead > 15 || (lead == 15 && string(sig) != "1") {
		return 0, nil, 0, dtFieldOverflow
	}
	if exp >= 0 {
		var v int64
		for _, d := range sig {
			v = v*10 + int64(d-'0')
		}
		for k := int64(0); k < exp; k++ {
			v *= 10
		}
		return v, new(big.Int), i, dtOK
	}
	// k digits are after the decimal point
	k := int(-exp)
	var intDigits, fracDigitRun []byte
	if len(sig) > k {
		intDigits = sig[:len(sig)-k]
		fracDigitRun = sig[len(sig)-k:]
	} else {
		zeros := k - len(sig)
		if zeros > fracDigits {
			zeros = fracDigits
		}
		fracDigitRun = append([]byte(strings.Repeat("0", zeros)), sig...)
	}
	var v int64
	for _, d := range intDigits {
		v = v*10 + int64(d-'0')
	}
	return v, fracFromDigits(fracDigitRun), i, dtOK
}

// scanHex reads a hexadecimal strtod number 0x hex[.hex][p[+-]digits] whose digits start at
// s[i:] — as scanDecimal, computed exactly from its bits (the fraction to 64 bits).
func scanHex(s string, i int) (int64, *big.Int, int, dtErr) {
	var bits []byte
	var fracHex int64
	pushHex := func(h byte) {
		var v byte
		switch {
		case isDigitByte(h):
			v = h - '0'
		default:
			v = lowerByte(h) - 'a' + 10
		}
		for b := 3; b >= 0; b-- {
			bits = append(bits, (v>>b)&1)
		}
	}
	for i < len(s) && isHexByte(s[i]) {
		pushHex(s[i])
		i++
	}
	if i < len(s) && s[i] == '.' {
		i++
		for i < len(s) && isHexByte(s[i]) {
			pushHex(s[i])
			fracHex++
			i++
		}
	}
	// value = bits * 2^e2
	e2 := readExponentPart(s, &i, 'p') - 4*fracHex
	first := 0
	for first < len(bits) && bits[first] == 0 {
		first++
	}
	if first == len(bits) {
		return 0, new(big.Int), i, dtOK
	}
	bits = bits[first:]
	n := int64(len(bits))
	// the power of two of the leading bit, and of the lowest set bit
	lead := n - 1 + e2
	last := int64(len(bits) - 1)
	for bits[last] == 0 {
		last--
	}
	low := n - 1 - last + e2
	// the leading 54 bits all ones: rounds up to the next power of two in 53-bit precision
	roundsUp := len(bits) >= 54
	for k := 0; roundsUp && k < 54; k++ {
		roundsUp = bits[k] == 1
	}
	if lead > 1023 || (lead == 1023 && roundsUp) {
		return 0, nil, 0, dtBadFormat
	}
	// ERANGE on underflow: below the normal range and not exact in the subnormal range
	tiny := lead < -1022 && !(lead == -1023 && roundsUp)
	if tiny && low < -1074 {
		return 0, nil, 0, dtBadFormat
	}
	if lead >= 50 {
		return 0, nil, 0, dtFieldOverflow
	}
	bitAt := func(pow int64) uint64 {
		// the bit worth 2^pow
		p := lead - pow
		if p >= 0 && p < n {
			return uint64(bits[p])
		}
		return 0
	}
	var ipart int64
	for pow := lead; pow >= 0; pow-- {
		ipart = ipart*2 + int64(bitAt(pow))
	}
	var f64bits uint64
	for q := int64(1); q <= 64; q++ {
		f64bits = f64bits<<1 | bitAt(-q)
	}
	if ipart > 1_000_000_000_000_000 || (ipart == 1_000_000_000_000_000 && f64bits > 0) {
		return 0, nil, 0, dtFieldOverflow
	}
	// f64bits / 2^64 = f64bits * 5^25 / 2^39 / 10^25
	frac := new(big.Int).SetUint64(f64bits)
	frac.Mul(frac, big.NewInt(298_023_223_876_953_125))
	frac.Rsh(frac, 39)
	return ipart, frac, i, dtOK
}

// parseISO8601Number is PG's ParseISO8601Number: a strtod number at s[i:] (it must start with a
// digit, `-`, or `.`) as (integer part, fraction, end), both parts carrying the sign.
func parseISO8601Number(s string, i int) (int64, *big.Int, int, dtErr) {
	at := func(k int) byte {
		if k < len(s) {
			return s[k]
		}
		return 0
	}
	c := at(i)
	if !(isDigitByte(c) || c == '-' || c == '.') {
		return 0, nil, 0, dtBadFormat
	}
	neg := c == '-'
	j := i
	if neg {
		j++
	}
	// strtod's infinity / NaN spellings: a value out of range
	word := func(w string) bool {
		for k := 0; k < len(w); k++ {
			if lowerByte(at(j+k)) != w[k] {
				return false
			}
		}
		return true
	}
	if word("inf") || word("nan") {
		return 0, nil, 0, dtFieldOverflow
	}
	isHex := at(j) == '0' && lowerByte(at(j+1)) == 'x' &&
		(isHexByte(at(j+2)) || (at(j+2) == '.' && isHexByte(at(j+3))))
	var ipart int64
	var frac *big.Int
	var end int
	var e dtErr
	if isHex {
		ipart, frac, end, e = scanHex(s, j+2)
	} else {
		ipart, frac, end, e = scanDecimal(s, j)
	}
	if e != dtOK {
		return 0, nil, 0, e
	}
	if neg {
		ipart = -ipart
		frac.Neg(frac)
	}
	return ipart, frac, end, dtOK
}

// iso8601IntegerWidth is PG's ISO8601IntegerWidth: the integer digits of a number field, past a
// leading `-`.
func iso8601IntegerWidth(s string) int {
	if len(s) > 0 && s[0] == '-' {
		s = s[1:]
	}
	n := 0
	for n < len(s) && isDigitByte(s[n]) {
		n++
	}
	return n
}

// decodeISO8601Interval is PG's DecodeISO8601Interval: the ISO 8601 "format with designators"
// (P1Y2M3DT4H5M6S, P2W, any field fractional or negative) and the "alternative format"
// (P0001-02-03T04:05:06, basic P00010203T040506). The whole input, untrimmed, must be the value.
func decodeISO8601Interval(s string, itm *itmIn) dtErr {
	*itm = itmIn{}
	if len(s) < 2 || s[0] != 'P' {
		return dtBadFormat
	}
	at := func(k int) byte {
		if k < len(s) {
			return s[k]
		}
		return 0
	}
	// first runs Adjust steps in order, stopping at the first failure
	first := func(steps ...func() dtErr) dtErr {
		for _, st := range steps {
			if e := st(); e != dtOK {
				return e
			}
		}
		return dtOK
	}
	i := 1
	datepart := true
	havefield := false
	for i < len(s) {
		if s[i] == 'T' {
			// the start of the time part
			datepart = false
			havefield = false
			i++
			continue
		}
		fieldstart := i
		val, fval, end, e := parseISO8601Number(s, i)
		if e != dtOK {
			return e
		}
		i = end
		unit := at(i)
		i++
		if datepart {
			switch unit {
			case 'Y':
				e = first(func() dtErr { return adjustYears(val, 1, itm) },
					func() dtErr { return adjustFractYears(fval, 1, itm) })
			case 'M':
				e = first(func() dtErr { return adjustMonths(val, itm) },
					func() dtErr { return adjustFractDays(fval, daysPerMonth, itm) })
			case 'W':
				e = first(func() dtErr { return adjustDays(val, 7, itm) },
					func() dtErr { return adjustFractDays(fval, 7, itm) })
			case 'D':
				e = first(func() dtErr { return adjustDays(val, 1, itm) },
					func() dtErr { return adjustFractMicros(fval, microsPerDay, itm) })
			case 'T', 0, '-':
				if unit != '-' && iso8601IntegerWidth(s[fieldstart:]) == 8 && !havefield {
					// alternative format, basic: YYYYMMDD
					if e = first(func() dtErr { return adjustYears(val/10000, 1, itm) },
						func() dtErr { return adjustMonths((val/100)%100, itm) },
						func() dtErr { return adjustDays(val%100, 1, itm) },
						func() dtErr { return adjustFractMicros(fval, microsPerDay, itm) }); e != dtOK {
						return e
					}
					if unit == 0 {
						return dtOK
					}
					datepart = false
					havefield = false
					continue
				}
				// alternative format, extended: YYYY-MM-DD
				if havefield {
					return dtBadFormat
				}
				if e = first(func() dtErr { return adjustYears(val, 1, itm) },
					func() dtErr { return adjustFractYears(fval, 1, itm) }); e != dtOK {
					return e
				}
				if unit == 0 {
					return dtOK
				}
				if unit == 'T' {
					datepart = false
					havefield = false
					continue
				}
				val, fval, end, e = parseISO8601Number(s, i)
				if e != dtOK {
					return e
				}
				i = end
				if e = first(func() dtErr { return adjustMonths(val, itm) },
					func() dtErr { return adjustFractDays(fval, daysPerMonth, itm) }); e != dtOK {
					return e
				}
				if i >= len(s) {
					return dtOK
				}
				if s[i] == 'T' {
					datepart = false
					havefield = false
					continue
				}
				if s[i] != '-' {
					return dtBadFormat
				}
				i++
				val, fval, end, e = parseISO8601Number(s, i)
				if e != dtOK {
					return e
				}
				i = end
				if e = first(func() dtErr { return adjustDays(val, 1, itm) },
					func() dtErr { return adjustFractMicros(fval, microsPerDay, itm) }); e != dtOK {
					return e
				}
				if i >= len(s) {
					return dtOK
				}
				if s[i] == 'T' {
					datepart = false
					havefield = false
					continue
				}
				return dtBadFormat
			default:
				return dtBadFormat
			}
		} else {
			switch unit {
			case 'H':
				e = adjustMicros(val, fval, usecsPerHour, itm)
			case 'M':
				e = adjustMicros(val, fval, usecsPerMinute, itm)
			case 'S':
				e = adjustMicros(val, fval, usecsPerSec, itm)
			case 0, ':':
				if unit == 0 && iso8601IntegerWidth(s[fieldstart:]) == 6 && !havefield {
					// alternative format, basic: HHMMSS
					zero := new(big.Int)
					return first(func() dtErr { return adjustMicros(val/10000, zero, usecsPerHour, itm) },
						func() dtErr { return adjustMicros((val/100)%100, zero, usecsPerMinute, itm) },
						func() dtErr { return adjustMicros(val%100, zero, usecsPerSec, itm) },
						func() dtErr { return adjustFractMicros(fval, 1, itm) })
				}
				// alternative format, extended: HH:MM:SS
				if havefield {
					return dtBadFormat
				}
				if e = adjustMicros(val, fval, usecsPerHour, itm); e != dtOK {
					return e
				}
				if unit == 0 {
					return dtOK
				}
				val, fval, end, e = parseISO8601Number(s, i)
				if e != dtOK {
					return e
				}
				i = end
				if e = adjustMicros(val, fval, usecsPerMinute, itm); e != dtOK {
					return e
				}
				if i >= len(s) {
					return dtOK
				}
				if s[i] != ':' {
					return dtBadFormat
				}
				i++
				val, fval, end, e = parseISO8601Number(s, i)
				if e != dtOK {
					return e
				}
				i = end
				if e = adjustMicros(val, fval, usecsPerSec, itm); e != dtOK {
					return e
				}
				if i >= len(s) {
					return dtOK
				}
				return dtBadFormat
			default:
				return dtBadFormat
			}
		}
		if e != dtOK {
			return e
		}
		havefield = true
	}
	return dtOK
}

// parseInterval parses interval text into the three-field value — PostgreSQL's interval_in
// (spec/design/interval.md §3): the PostgreSQL / SQL-standard field syntax, else ISO 8601.
// Malformed input is 22007; a field beyond its range is 22008 (PG's 22015).
func parseInterval(input string) (Interval, error) {
	var itm itmIn
	infinite := false
	fields, e := parseDateTime(input)
	if e == dtOK {
		infinite, e = decodeInterval(fields, &itm)
	}
	if e == dtBadFormat {
		infinite = false
		e = decodeISO8601Interval(input, &itm)
	}
	switch {
	case e == dtBadFormat || (e == dtOK && infinite):
		// (jed has no infinite interval, so `infinity` is not an interval value)
		return Interval{}, invalidInterval(fmt.Sprintf("invalid input syntax for type interval: \"%s\"", input))
	case e == dtFieldOverflow:
		return Interval{}, intervalFieldOverflow(fmt.Sprintf("interval field value out of range: \"%s\"", input))
	}
	months := int64(itm.year)*monthsPerYear + int64(itm.mon)
	m, ok := toInt32(months)
	if !ok {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	return Interval{Months: m, Days: itm.mday, Micros: itm.usec}, nil
}

// neg64 negates an int64, ok=false for the one value with no negation.
func neg64(a int64) (int64, bool) {
	if a == -9223372036854775808 {
		return 0, false
	}
	return -a, true
}

// --- rendering ---------------------------------------------------------------

// RenderInterval renders an interval to PG's canonical `IntervalStyle = postgres` text
// (spec/design/interval.md §4). Pure integer→string formatting (no locale).
func renderInterval(iv Interval) string {
	months := int64(iv.Months)
	days := int64(iv.Days)
	micros := iv.Micros
	if months == 0 && days == 0 && micros == 0 {
		return "00:00:00"
	}
	year := months / monthsPerYear
	mon := months % monthsPerYear

	var b strings.Builder
	isZero := true
	isBefore := false
	addIntPart(&b, year, "year", &isZero, &isBefore)
	addIntPart(&b, mon, "mon", &isZero, &isBefore)
	addIntPart(&b, days, "day", &isZero, &isBefore)

	if micros != 0 || isZero {
		neg := micros < 0
		a := uint64(micros)
		if neg {
			a = uint64(-micros) // as uint64, so math.MinInt64 negates to 2^63
		}
		h := a / (3600 * microsPerSec) // unbounded hour (micros not justified into days)
		mi := (a / (60 * microsPerSec)) % 60
		s := (a / microsPerSec) % 60
		us := a % microsPerSec
		if !isZero {
			b.WriteByte(' ')
		}
		if neg {
			b.WriteByte('-')
		} else if isBefore {
			b.WriteByte('+')
		}
		fmt.Fprintf(&b, "%02d:%02d:%02d", h, mi, s)
		if us != 0 {
			frac := strings.TrimRight(fmt.Sprintf("%06d", us), "0")
			b.WriteByte('.')
			b.WriteString(frac)
		}
	}
	return b.String()
}

// addIntPart appends one integer field (year/mon/day) in PG postgres-style: nothing when zero;
// otherwise a leading space (unless first), a `+` only when a previous field was negative and
// this one is positive, the value, the unit, and a plural `s` when the value is not exactly 1.
func addIntPart(b *strings.Builder, value int64, unit string, isZero, isBefore *bool) {
	if value == 0 {
		return
	}
	if !*isZero {
		b.WriteByte(' ')
	}
	if *isBefore && value > 0 {
		b.WriteByte('+')
	}
	fmt.Fprintf(b, "%d %s", value, unit)
	if value != 1 {
		b.WriteByte('s')
	}
	*isBefore = value < 0
	*isZero = false
}
