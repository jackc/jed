package jed

import (
	"fmt"
	"math/big"
	"strings"
)

// The interval type — value model, parsing, and rendering (spec/design/interval.md). A value is
// PostgreSQL's three independent fields: Months (i32), Days (i32), Micros (i64). They are kept
// separate so `+ 1 month` is calendar-aware; comparison/ordering/dedup collapse them via the
// canonical 128-bit span (1 month = 30 days, 1 day = 24 h). Parsing accepts the "unit + time"
// subset; rendering is PG `IntervalStyle = postgres`. A §8 determinism hotspot: the
// fractional-unit cascade, the half-away µs rounding, and the render format must be byte-identical
// across the Rust/Go/TS cores. The cascade and span use math/big (Go has no int128), so the exact
// integer math matches Rust's i128 and TS's BigInt within the digit caps below.

const (
	microsPerDay  = int64(86_400) * microsPerSec // canonical "1 day = 24 h" span weight
	daysPerMonth  = int64(30)                    // canonical "1 month = 30 days"
	monthsPerYear = int64(12)
	maxIntDigits  = 18 // per spec/design/interval.md — bounds the exact cascade
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
	days := int64(iv.Months)*daysPerMonth + int64(iv.Days) // fits int64 (i32*30 + i32)
	b := big.NewInt(days)
	b.Mul(b, big.NewInt(microsPerDay))
	b.Add(b, big.NewInt(iv.Micros))
	return b
}

// SpanCmp compares two intervals by their canonical span: -1, 0, or 1.
func (iv Interval) SpanCmp(o Interval) int { return iv.Span().Cmp(o.Span()) }

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
func MakeInterval(years, months, weeks, days, hours, mins, secMicros int64) (Interval, error) {
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
func ParseFactorDecimal(s string) (*big.Int, *big.Int, error) {
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
func MulByFraction(iv Interval, fnum, fden *big.Int) (Interval, error) {
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
// finite result onto a sentinel or beyond the int64-µs range traps 22008.
func TsShift(ts int64, iv Interval, subtract bool) (int64, error) {
	if ts == NegInfinity || ts == PosInfinity {
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
	if t == NegInfinity || t == PosInfinity {
		return 0, intervalFieldOverflow("timestamp out of range")
	}
	return t, nil
}

// TsDiff computes a - b of two timestamps (or timestamptz) → an interval, justified into days +
// time with months = 0 (PG timestamp_mi → interval_justify_hours). An ±infinity operand traps
// 22008; a day count beyond i32 traps 22008.
func TsDiff(a, b int64) (Interval, error) {
	if a == NegInfinity || a == PosInfinity || b == NegInfinity || b == PosInfinity {
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

// --- parsing -----------------------------------------------------------------

func invalidInterval(detail string) error       { return NewError(InvalidDatetimeFormat, detail) }
func intervalFieldOverflow(detail string) error { return NewError(DatetimeFieldOverflow, detail) }

// intervalAcc accumulates the three fields exactly. The fractional part of each unit token
// cascades down (months→days→micros) using exact big.Int math, the µs result rounded half away
// from zero. Field overflow traps 22008.
type intervalAcc struct {
	months int64
	days   int64
	micros int64
}

func (a *intervalAcc) addMonths(m int64) error {
	s, ok := add64(a.months, m)
	if !ok {
		return intervalFieldOverflow("interval out of range")
	}
	a.months = s
	return nil
}

func (a *intervalAcc) addDays(d int64) error {
	s, ok := add64(a.days, d)
	if !ok {
		return intervalFieldOverflow("interval out of range")
	}
	a.days = s
	return nil
}

func (a *intervalAcc) addMicros(u int64) error {
	s, ok := add64(a.micros, u)
	if !ok {
		return intervalFieldOverflow("interval out of range")
	}
	a.micros = s
	return nil
}

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

// toI64 converts a big.Int to int64, trapping 22008 on overflow.
func toI64(b *big.Int) (int64, error) {
	if !b.IsInt64() {
		return 0, intervalFieldOverflow("interval out of range")
	}
	return b.Int64(), nil
}

// applyUnit adds value = sign * intPart.fracNum/fracDen of a unit to the accumulator, where the
// unit is measured in monthsPer, daysPer, or microsPer of one base field (exactly one nonzero).
// The integer part lands in that field; the fractional part cascades to the next-lower fields
// using 1 month = 30 days and 1 day = 24 h (spec/design/interval.md §3). All exact big.Int math.
func (a *intervalAcc) applyUnit(neg bool, intPart, fracNum, fracDen *big.Int, monthsPer, daysPer, microsPer int64) error {
	// n = sign * (intPart*fracDen + fracNum); d = fracDen.
	n := new(big.Int).Mul(intPart, fracDen)
	n.Add(n, fracNum)
	if neg {
		n.Neg(n)
	}
	d := fracDen

	months := big.NewInt(0)
	days := big.NewInt(0)
	microsNum := big.NewInt(0) // exact micros numerator over d

	if monthsPer != 0 {
		total := new(big.Int).Mul(n, big.NewInt(monthsPer)) // months * d
		months = new(big.Int).Quo(total, d)
		rem := new(big.Int).Sub(total, new(big.Int).Mul(months, d))
		dayTotal := rem.Mul(rem, big.NewInt(daysPerMonth)) // days * d
		wholeDays := new(big.Int).Quo(dayTotal, d)
		days.Add(days, wholeDays)
		remDays := new(big.Int).Sub(dayTotal, new(big.Int).Mul(wholeDays, d))
		microsNum.Add(microsNum, remDays.Mul(remDays, big.NewInt(microsPerDay)))
	}
	if daysPer != 0 {
		total := new(big.Int).Mul(n, big.NewInt(daysPer)) // days * d
		wholeDays := new(big.Int).Quo(total, d)
		days.Add(days, wholeDays)
		remDays := new(big.Int).Sub(total, new(big.Int).Mul(wholeDays, d))
		microsNum.Add(microsNum, remDays.Mul(remDays, big.NewInt(microsPerDay)))
	}
	if microsPer != 0 {
		microsNum.Add(microsNum, new(big.Int).Mul(n, big.NewInt(microsPer)))
	}

	if months.Sign() != 0 {
		m, err := toI64(months)
		if err != nil {
			return err
		}
		if err := a.addMonths(m); err != nil {
			return err
		}
	}
	if days.Sign() != 0 {
		dd, err := toI64(days)
		if err != nil {
			return err
		}
		if err := a.addDays(dd); err != nil {
			return err
		}
	}
	if microsNum.Sign() != 0 {
		u, err := toI64(roundDivBig(microsNum, d))
		if err != nil {
			return err
		}
		if err := a.addMicros(u); err != nil {
			return err
		}
	}
	return nil
}

// unitWeights returns the cascade weights (monthsPer, daysPer, microsPer) for a unit word
// (case-insensitive), or ok=false for an unrecognized unit. Exactly one weight is nonzero.
func unitWeights(unit string) (int64, int64, int64, bool) {
	switch strings.ToLower(unit) {
	case "millennium", "millennia", "mil", "mils":
		return 12000, 0, 0, true
	case "century", "centuries", "cent", "c":
		return 1200, 0, 0, true
	case "decade", "decades", "dec", "decs":
		return 120, 0, 0, true
	case "year", "years", "yr", "yrs", "y":
		return monthsPerYear, 0, 0, true
	case "month", "months", "mon", "mons":
		return 1, 0, 0, true
	case "week", "weeks", "w":
		return 0, 7, 0, true
	case "day", "days", "d":
		return 0, 1, 0, true
	case "hour", "hours", "hr", "hrs", "h":
		return 0, 0, 3600 * microsPerSec, true
	case "minute", "minutes", "min", "mins":
		return 0, 0, 60 * microsPerSec, true
	case "second", "seconds", "sec", "secs", "s":
		return 0, 0, microsPerSec, true
	case "millisecond", "milliseconds", "msec", "msecs", "ms":
		return 0, 0, 1000, true
	case "microsecond", "microseconds", "usec", "usecs", "us":
		return 0, 0, 1, true
	}
	return 0, 0, 0, false
}

type intervalCursor struct {
	b string
	i int
}

func (c *intervalCursor) skipWS() {
	for c.i < len(c.b) && isWS(c.b[c.i]) {
		c.i++
	}
}

func (c *intervalCursor) done() bool { return c.i >= len(c.b) }

func (c *intervalCursor) peek() (byte, bool) {
	if c.i < len(c.b) {
		return c.b[c.i], true
	}
	return 0, false
}

// readIntervalDigits reads a run of ASCII digits as a big.Int. ok=false for an empty run (caller
// raises 22007); more than maxIntDigits digits → err 22008.
func (c *intervalCursor) readIntervalDigits() (*big.Int, bool, error) {
	start := c.i
	for c.i < len(c.b) && c.b[c.i] >= '0' && c.b[c.i] <= '9' {
		c.i++
	}
	if c.i == start {
		return nil, false, nil
	}
	if c.i-start > maxIntDigits {
		return nil, false, intervalFieldOverflow("interval field has too many digits")
	}
	v, _ := new(big.Int).SetString(c.b[start:c.i], 10)
	return v, true, nil
}

// peekIntervalWord peeks an ASCII-letter word at the cursor (not consuming).
func (c *intervalCursor) peekWord() (string, bool) {
	start := c.i
	j := start
	for j < len(c.b) && isAlpha(c.b[j]) {
		j++
	}
	if j == start {
		return "", false
	}
	return c.b[start:j], true
}

func isAlpha(b byte) bool {
	return (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z')
}

// parseIntervalTime parses MM[:SS[.ffffff]] after an integer hour and a ':' — adding their micros
// to acc with the given sign. hour is the already-read integer hour (unbounded; PG allows
// 100:00:00). Sub-µs digits round half away from zero (like timestamp).
func (a *intervalAcc) parseTime(c *intervalCursor, neg bool, hour *big.Int) error {
	c.i++ // consume ':'
	minute, ok, err := c.readIntervalDigits()
	if err != nil {
		return err
	}
	if !ok {
		return invalidInterval("expected minutes")
	}
	second := big.NewInt(0)
	fracUS := int64(0)
	if b, has := c.peek(); has && b == ':' {
		c.i++
		second, ok, err = c.readIntervalDigits()
		if err != nil {
			return err
		}
		if !ok {
			return invalidInterval("expected seconds")
		}
		if b2, has2 := c.peek(); has2 && b2 == '.' {
			c.i++
			fracUS, err = c.readIntervalFracUS()
			if err != nil {
				return err
			}
		}
	}
	// total µs = hour*3600e6 + minute*60e6 + second*1e6 + fracUS (exact big.Int)
	total := new(big.Int).Mul(hour, big.NewInt(3600*microsPerSec))
	total.Add(total, new(big.Int).Mul(minute, big.NewInt(60*microsPerSec)))
	total.Add(total, new(big.Int).Mul(second, big.NewInt(microsPerSec)))
	total.Add(total, big.NewInt(fracUS))
	if neg {
		total.Neg(total)
	}
	u, err := toI64(total)
	if err != nil {
		return err
	}
	return a.addMicros(u)
}

// readIntervalFracUS reads fractional-seconds digits after '.' into microseconds (0..1_000_000),
// 0–6 digits exact, 7+ rounded half away from zero — identical to the timestamp rule.
func (c *intervalCursor) readIntervalFracUS() (int64, error) {
	start := c.i
	for c.i < len(c.b) && c.b[c.i] >= '0' && c.b[c.i] <= '9' {
		c.i++
	}
	digits := c.b[start:c.i]
	if len(digits) == 0 {
		return 0, invalidInterval("expected fractional digits after '.'")
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

// readIntervalUnitFrac reads a unit value's fractional digits after '.' as (numerator,
// denominator) with the denominator a power of ten. More than maxFracDigits digits → err 22008.
func (c *intervalCursor) readIntervalUnitFrac() (*big.Int, *big.Int, error) {
	start := c.i
	for c.i < len(c.b) && c.b[c.i] >= '0' && c.b[c.i] <= '9' {
		c.i++
	}
	digits := c.b[start:c.i]
	if len(digits) == 0 {
		return nil, nil, invalidInterval("expected fractional digits after '.'")
	}
	if len(digits) > maxFracDigits {
		return nil, nil, intervalFieldOverflow("interval value has too many fractional digits")
	}
	num, _ := new(big.Int).SetString(digits, 10)
	den := new(big.Int).Exp(big.NewInt(10), big.NewInt(int64(len(digits))), nil)
	return num, den, nil
}

// ParseInterval parses an interval literal (the "unit + time" subset) into the three-field value.
// Errors: malformed syntax → 22007; a field beyond the representable range → 22008.
func ParseInterval(input string) (Interval, error) {
	trimmed := trimASCIIWS(input)
	c := &intervalCursor{b: trimmed}
	a := &intervalAcc{}

	c.skipWS()
	// An optional leading `@` (PG's verbose lead-in) is accepted and ignored.
	if b, ok := c.peek(); ok && b == '@' {
		c.i++
		c.skipWS()
	}
	if c.done() {
		return Interval{}, invalidInterval("empty interval")
	}

	ago := false
	sawField := false
	one := big.NewInt(1)
	for !c.done() {
		if word, ok := c.peekWord(); ok && strings.EqualFold(word, "ago") {
			c.i += len(word)
			ago = true
			c.skipWS()
			break
		}

		neg := false
		if b, ok := c.peek(); ok {
			if b == '-' {
				neg = true
				c.i++
			} else if b == '+' {
				c.i++
			}
		}
		intPart, ok, err := c.readIntervalDigits()
		if err != nil {
			return Interval{}, err
		}
		if !ok {
			return Interval{}, invalidInterval("expected a number")
		}

		if b, has := c.peek(); has && b == ':' {
			if err := a.parseTime(c, neg, intPart); err != nil {
				return Interval{}, err
			}
			sawField = true
		} else {
			fracNum, fracDen := big.NewInt(0), one
			if b2, has2 := c.peek(); has2 && b2 == '.' {
				c.i++
				fracNum, fracDen, err = c.readIntervalUnitFrac()
				if err != nil {
					return Interval{}, err
				}
			}
			c.skipWS()
			// A bare number with no unit defaults to SECONDS (PG); a trailing `ago` is left for
			// the loop top; a recognized unit applies its weights; else 22007.
			var mper, dper, uper int64
			if word, has := c.peekWord(); has {
				if strings.EqualFold(word, "ago") {
					mper, dper, uper = 0, 0, microsPerSec
				} else {
					c.i += len(word)
					var known bool
					mper, dper, uper, known = unitWeights(word)
					if !known {
						return Interval{}, invalidInterval(fmt.Sprintf("unknown interval unit %q", word))
					}
				}
			} else {
				mper, dper, uper = 0, 0, microsPerSec
			}
			if err := a.applyUnit(neg, intPart, fracNum, fracDen, mper, dper, uper); err != nil {
				return Interval{}, err
			}
			sawField = true
		}
		c.skipWS()
	}

	if !c.done() {
		return Interval{}, invalidInterval("trailing characters in interval")
	}
	if !sawField {
		return Interval{}, invalidInterval("empty interval")
	}

	if ago {
		var ok bool
		if a.months, ok = neg64(a.months); !ok {
			return Interval{}, intervalFieldOverflow("interval out of range")
		}
		if a.days, ok = neg64(a.days); !ok {
			return Interval{}, intervalFieldOverflow("interval out of range")
		}
		if a.micros, ok = neg64(a.micros); !ok {
			return Interval{}, intervalFieldOverflow("interval out of range")
		}
	}

	if a.months < -2147483648 || a.months > 2147483647 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	if a.days < -2147483648 || a.days > 2147483647 {
		return Interval{}, intervalFieldOverflow("interval out of range")
	}
	return Interval{Months: int32(a.months), Days: int32(a.days), Micros: a.micros}, nil
}

func neg64(a int64) (int64, bool) {
	if a == -9223372036854775808 {
		return 0, false
	}
	return -a, true
}

// --- rendering ---------------------------------------------------------------

// RenderInterval renders an interval to PG's canonical `IntervalStyle = postgres` text
// (spec/design/interval.md §4). Pure integer→string formatting (no locale).
func RenderInterval(iv Interval) string {
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
		a := micros
		if neg {
			a = -a // micros is never math.MinInt64 in a valid interval
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
