package jed

// date_trunc and EXTRACT — the datetime field/truncation kernels (spec/design/timezones.md
// §9.1/§9.2). Pure functions over an instant's microseconds (or a wall-clock decomposition / an
// interval), shared by the executor's date_trunc / EXTRACT evaluation. Zone handling lives in the
// executor (it converts a timestamptz instant to a local wall-clock micros in the session/explicit
// zone, then calls these wall-clock kernels); this module is zone-free and so is a §8 cross-core
// determinism contract on its own — the same (unit/field, value) yields the byte-identical result on
// every core. Calendar math reuses timestamp's Hinnant core. The year-group fields and `year` use
// PostgreSQL's BC-aware year numbering (no year 0); jed stores astronomical years (0 = 1 BC), so
// toPgYear / fromPgYear convert at the boundary. The exact integer formulas are oracle-pinned.

import (
	"strconv"
)

const (
	microsPerSecDT  int64 = 1_000_000
	secsPerDayDT    int64 = 86_400
	microsPerDayDT  int64 = secsPerDayDT * microsPerSecDT
	microsPerMinDT  int64 = 60 * microsPerSecDT
	microsPerHourDT int64 = 3_600 * microsPerSecDT
	// 365.25 days/year * 86400 s/day = 31_557_600 s (integral — PG's interval-epoch year).
	secsPerIntervalYear  int64 = 31_557_600
	secsPerIntervalMonth int64 = 30 * secsPerDayDT
	// unixEpochJD is the Julian Day number of 1970-01-01 (PG's UNIX_EPOCH_JDATE).
	unixEpochJD int64 = 2_440_588
)

// toPgYear converts an astronomical year (0 = 1 BC) to PostgreSQL's year numbering (no year 0;
// 1 BC = -1). EXTRACT and the year-group fields report this. astro <= 0 is BC.
func toPgYear(astro int64) int64 {
	if astro <= 0 {
		return astro - 1
	}
	return astro
}

// fromPgYear is the inverse of toPgYear (a negative PG year is BC, shifted up by one to skip the
// missing year 0).
func fromPgYear(pg int64) int64 {
	if pg < 0 {
		return pg + 1
	}
	return pg
}

func absI64(v int64) int64 {
	if v < 0 {
		return -v
	}
	return v
}

// decimalScaled builds a numeric from an unscaled int64 and a scale (value = unscaled * 10^-scale).
func decimalScaled(unscaled int64, scale uint32) Decimal {
	return decimalFromDigitsScale(unscaled < 0, strconv.FormatInt(absI64(unscaled), 10), scale)
}

// dowSun0 is the day of week, 0 = Sunday .. 6 = Saturday (PG `dow`). 1970-01-01 was a Thursday (=4).
func dowSun0(daysSinceEpoch int64) int64 { return floorMod(daysSinceEpoch+4, 7) }

// isodowMon1 is the ISO day of week, 1 = Monday .. 7 = Sunday (PG `isodow`).
func isodowMon1(daysSinceEpoch int64) int64 { return floorMod(daysSinceEpoch+3, 7) + 1 }

func isoP(y int64) int64 {
	return floorMod(y+floorDiv(y, 4)-floorDiv(y, 100)+floorDiv(y, 400), 7)
}

func weeksInIsoYear(y int64) int64 {
	if isoP(y) == 4 || isoP(y-1) == 3 {
		return 53
	}
	return 52
}

// isoWeekYear returns (isoWeek, isoYearAstronomical) for a civil date (astronomical year).
func isoWeekYear(y, mo, d int64) (int64, int64) {
	days := daysFromCivil(y, mo, d)
	ordinal := days - daysFromCivil(y, 1, 1) + 1
	weekday := isodowMon1(days)
	week := floorDiv(ordinal-weekday+10, 7)
	if week < 1 {
		return weeksInIsoYear(y - 1), y - 1
	}
	if week > weeksInIsoYear(y) {
		return 1, y + 1
	}
	return week, y
}

func extractDecade(pgYear int64) int64 {
	if pgYear >= 0 {
		return pgYear / 10
	}
	return -((8 - pgYear) / 10)
}

func extractCentury(pgYear int64) int64 {
	if pgYear >= 0 {
		return (pgYear + 99) / 100
	}
	return -((99 - pgYear) / 100)
}

func extractMillennium(pgYear int64) int64 {
	if pgYear >= 0 {
		return (pgYear + 999) / 1000
	}
	return -((999 - pgYear) / 1000)
}

// intervalQuarter is EXTRACT(quarter FROM interval) from the signed months field — PG's
// interval_part_common, which works from the months count rather than the truncated month-of-year so
// that a negative interval's quarter is the negation of the sign-reversed interval's: (months%12)/3+1
// for months >= 0, else -((-months%12)/3+1) (so -12 mons is -1, not 1; oracle-pinned).
func intervalQuarter(months int64) int64 {
	if months >= 0 {
		return (months%12)/3 + 1
	}
	return -(((-months)%12)/3 + 1)
}

// fieldName is what a unit name means to EXTRACT / date_part (spec/design/timezones.md §9.6):
// a canonical unit, a reserved word other than epoch, or unknown.
type fieldName struct {
	kind int    // fieldUnit, fieldReserved, fieldUnknown
	unit string // the canonical unit, for fieldUnit
}

const (
	fieldUnit = iota
	fieldReserved
	fieldUnknown
)

// lowerUnit is the unit name as PostgreSQL's messages print it: ASCII-lowercased
// (downcase_truncate_identifier; a non-ASCII letter keeps its case).
func lowerUnit(name string) string { return asciiLower(name) }

// searchTokens is PostgreSQL's datebsearch: the entry whose token equals the first dtTokenMaxLen
// bytes of the lowercased name (so "microseconds" finds "microsecon"), or nil.
func searchTokens(table []dtToken, name string) *dtToken {
	key := name
	if len(key) > dtTokenMaxLen {
		key = key[:dtTokenMaxLen]
	}
	lo, hi := 0, len(table)
	for lo < hi {
		mid := (lo + hi) / 2
		switch {
		case table[mid].token == key:
			return &table[mid]
		case table[mid].token < key:
			lo = mid + 1
		default:
			hi = mid
		}
	}
	return nil
}

// decodeUnits is PostgreSQL's DecodeUnits over a lowercased name: the canonical unit a deltatktbl
// units entry names ("hrs" -> "hour"). date_trunc reads unit names this way.
func decodeUnits(name string) (string, bool) {
	if t := searchTokens(dtDeltaTokens, name); t != nil && t.kind == "units" {
		return t.value, true
	}
	return "", false
}

// decodeField is EXTRACT / date_part's field lookup over a lowercased name: PostgreSQL's
// DecodeUnits, then DecodeSpecial (datetktbl) when the name is not in deltatktbl at all.
func decodeField(name string) fieldName {
	if t := searchTokens(dtDeltaTokens, name); t != nil {
		if t.kind == "units" {
			return fieldName{kind: fieldUnit, unit: t.value}
		}
		return fieldName{kind: fieldUnknown}
	}
	t := searchTokens(dtDateTokens, name)
	switch {
	case t == nil:
		return fieldName{kind: fieldUnknown}
	case t.kind == "units":
		return fieldName{kind: fieldUnit, unit: t.value}
	case t.kind == "reserv" && t.value == "epoch":
		return fieldName{kind: fieldUnit, unit: "epoch"}
	case t.kind == "reserv":
		return fieldName{kind: fieldReserved}
	default:
		return fieldName{kind: fieldUnknown}
	}
}

// canonicalField is the canonical field an EXTRACT / date_part field name resolves to
// ("jd" -> "julian"), or "" when it names no field.
func canonicalField(field string) string {
	f := decodeField(lowerUnit(field))
	if f.kind != fieldUnit {
		return ""
	}
	return f.unit
}

func unitNotRecognized(name, typeName string) error {
	return newError(InvalidParameterValue, "unit \""+name+"\" not recognized for type "+typeName)
}

func unitNotSupported(name, typeName string) *EngineError {
	return newError(FeatureNotSupported, "unit \""+name+"\" not supported for type "+typeName)
}

// timestampTypeName is a timestamp family's type name, as PostgreSQL's unit messages print it.
func timestampTypeName(tz bool) string {
	if tz {
		return "timestamp with time zone"
	}
	return "timestamp without time zone"
}

// dateTruncMicros truncates a wall-clock instant `micros` down to the start of `unit` (tz names the
// source type in messages). The unit is resolved first, as PostgreSQL does: an unrecognized unit is
// 22023 and a timezone* unit 0A000, even over ±infinity, which otherwise passes through unchanged.
func dateTruncMicros(unit string, micros int64, tz bool) (int64, error) {
	name := lowerUnit(unit)
	u, ok := decodeUnits(name)
	if !ok {
		return 0, unitNotRecognized(name, timestampTypeName(tz))
	}
	if micros == posInfinity || micros == negInfinity {
		// Validate the unit against a finite probe, then pass the infinity through.
		if _, err := dateTruncMicros(unit, 0, tz); err != nil {
			return 0, err
		}
		return micros, nil
	}
	y, mo, d, h, mi, s, us := civilFromMicros(micros)
	rebuild := func(y, mo, d, h, mi, s, us int64) int64 {
		return daysFromCivil(y, mo, d)*microsPerDayDT +
			h*microsPerHourDT + mi*microsPerMinDT + s*microsPerSecDT + us
	}
	switch u {
	case "microseconds":
		return micros, nil
	case "milliseconds":
		return rebuild(y, mo, d, h, mi, s, (us/1000)*1000), nil
	case "second":
		return rebuild(y, mo, d, h, mi, s, 0), nil
	case "minute":
		return rebuild(y, mo, d, h, mi, 0, 0), nil
	case "hour":
		return rebuild(y, mo, d, h, 0, 0, 0), nil
	case "day":
		return rebuild(y, mo, d, 0, 0, 0, 0), nil
	case "week":
		days := daysFromCivil(y, mo, d)
		monday0 := floorMod(days+3, 7)
		return (days - monday0) * microsPerDayDT, nil
	case "month":
		return rebuild(y, mo, 1, 0, 0, 0, 0), nil
	case "quarter":
		return rebuild(y, ((mo-1)/3)*3+1, 1, 0, 0, 0, 0), nil
	case "year":
		return rebuild(y, 1, 1, 0, 0, 0, 0), nil
	case "decade":
		d10 := extractDecade(toPgYear(y))
		startTm := d10 * 10
		if d10 < 1 {
			startTm = d10*10 - 1
		}
		return rebuild(fromPgYear(startTm), 1, 1, 0, 0, 0, 0), nil
	case "century":
		c := extractCentury(toPgYear(y))
		startTm := (c-1)*100 + 1
		if c < 1 {
			startTm = c * 100
		}
		return rebuild(fromPgYear(startTm), 1, 1, 0, 0, 0, 0), nil
	case "millennium":
		m := extractMillennium(toPgYear(y))
		startTm := (m-1)*1000 + 1
		if m < 1 {
			startTm = m * 1000
		}
		return rebuild(fromPgYear(startTm), 1, 1, 0, 0, 0, 0), nil
	default:
		return 0, unitNotSupported(name, timestampTypeName(tz))
	}
}

// dateTruncInterval truncates an interval's fields down to `unit`. `week` and the timezone* units
// are 0A000; an unrecognized unit is 22023.
func dateTruncInterval(unit string, iv Interval) (Interval, error) {
	name := lowerUnit(unit)
	u, ok := decodeUnits(name)
	if !ok {
		return Interval{}, unitNotRecognized(name, "interval")
	}
	months := int64(iv.Months)
	micros := iv.Micros
	keepMd := func(m int64) Interval { return Interval{Months: iv.Months, Days: iv.Days, Micros: m} }
	switch u {
	case "microseconds":
		return iv, nil
	case "milliseconds":
		return keepMd((micros / 1000) * 1000), nil
	case "second":
		return keepMd((micros / microsPerSecDT) * microsPerSecDT), nil
	case "minute":
		return keepMd((micros / microsPerMinDT) * microsPerMinDT), nil
	case "hour":
		return keepMd((micros / microsPerHourDT) * microsPerHourDT), nil
	case "day":
		return keepMd(0), nil
	case "week":
		return Interval{}, unitNotSupported(name, "interval").withDetail("Months usually have fractional weeks.")
	case "month":
		return Interval{Months: iv.Months}, nil
	case "quarter":
		return Interval{Months: int32((months / 3) * 3)}, nil
	case "year":
		return Interval{Months: int32((months / 12) * 12)}, nil
	case "decade":
		return Interval{Months: int32((months / 120) * 120)}, nil
	case "century":
		return Interval{Months: int32((months / 1200) * 1200)}, nil
	case "millennium":
		return Interval{Months: int32((months / 12000) * 12000)}, nil
	default:
		return Interval{}, unitNotSupported(name, "interval")
	}
}

// julianParts splits wall-clock micros into the Julian Day number of its date (PG's date2j) and
// the microseconds into that day.
func julianParts(micros int64) (jd, todUs int64) {
	days := floorDiv(micros, microsPerDayDT)
	return days + unixEpochJD, micros - days*microsPerDayDT
}

// extractJulian is EXTRACT(julian FROM timestamp) over finite wall-clock micros: the Julian Day
// number plus the fraction of the day, exactly as PG's numeric path computes it — jd +
// (time-of-day micros / 86400000000), the division at numeric's select_div_scale.
func extractJulian(micros int64) (Decimal, error) {
	jd, todUs := julianParts(micros)
	frac, err := decimalFromInt64(todUs).Div(decimalFromInt64(microsPerDayDT))
	if err != nil {
		return Decimal{}, err
	}
	return decimalFromInt64(jd).Add(frac)
}

// datePartJulian is date_part('julian', timestamp) over wall-clock micros — PG's float8 formula
// verbatim (jd + (seconds + fsec/1e6) / 86400.0, not the numeric value rounded), so the result is
// PG's to the bit. An infinite source traps like every other field (22003).
func datePartJulian(micros int64) (float64, error) {
	if micros == posInfinity || micros == negInfinity {
		return 0, newError(NumericValueOutOfRange, "cannot extract field from an infinite timestamp")
	}
	jd, todUs := julianParts(micros)
	secs := todUs / microsPerSecDT
	fsec := todUs % microsPerSecDT
	return float64(jd) + (float64(secs)+float64(fsec)/1000000.0)/float64(secsPerDayDT), nil
}

// extractSrc is the source value of an EXTRACT(field FROM source). For a timestamptz the caller
// supplies the wall-clock Local micros (already converted into the session zone), the raw Instant
// (for `epoch`), and the zone OffsetSecs (for the timezone* fields); for a timestamp only the wall
// micros (in Local).
type extractSrc struct {
	kind       int // 0=timestamp, 1=timestamptz, 2=date, 3=interval
	local      int64
	instant    int64
	offsetSecs int64
	days       int32
	iv         Interval
}

const (
	srcTs   = 0
	srcTstz = 1
	srcDate = 2
	srcIv   = 3
)

// extractField returns EXTRACT(field FROM source) as numeric. The field name is resolved first,
// through PostgreSQL's unit tables (so "hours", "msec", "jd" are hour, milliseconds, julian —
// timezones.md §9.6); then an unsupported field for the type is 0A000 and an unrecognized field
// 22023, both ahead of the infinite-source trap.
func extractField(field string, src extractSrc) (Decimal, error) {
	name := lowerUnit(field)
	var typeName string
	switch src.kind {
	case srcTs:
		typeName = timestampTypeName(false)
	case srcTstz:
		typeName = timestampTypeName(true)
	case srcDate:
		typeName = "date"
	default:
		typeName = "interval"
	}
	f := decodeField(name)
	switch {
	case f.kind == fieldReserved && src.kind != srcIv:
		// A reserved word other than epoch is unsupported for the date/time types but unrecognized
		// for an interval — PostgreSQL's interval_part_common checks only epoch.
		return Decimal{}, unitNotSupported(name, typeName)
	case f.kind != fieldUnit:
		return Decimal{}, unitNotRecognized(name, typeName)
	}
	switch src.kind {
	case srcTs:
		return extractDatetime(f.unit, name, src.local, false, 0, 0)
	case srcTstz:
		return extractDatetime(f.unit, name, src.local, true, src.instant, src.offsetSecs)
	case srcDate:
		return extractDate(f.unit, name, int64(src.days))
	default:
		return extractInterval(f.unit, name, src.iv)
	}
}

// extractDatetime extracts the canonical `unit` (written as `name`, for messages) from the
// wall-clock micros of a timestamp / timestamptz.
func extractDatetime(unit, name string, micros int64, isTz bool, instant, offsetSecs int64) (Decimal, error) {
	if micros == posInfinity || micros == negInfinity {
		// Every unit is accepted here (PG's NonFiniteTimestampTzPart, even timezone of a
		// timestamp); jed's decimal is finite-only (decimal.md §2), so 22003 where PG returns
		// ±Infinity or NULL — a documented divergence (timezones.md §9.2).
		return Decimal{}, newError(NumericValueOutOfRange, "cannot extract field from an infinite timestamp")
	}
	y, mo, d, h, mi, s, us := civilFromMicros(micros)
	secUs := s*microsPerSecDT + us
	days := daysFromCivil(y, mo, d)
	switch unit {
	case "microseconds":
		return decimalScaled(secUs, 0), nil
	case "milliseconds":
		return decimalScaled(secUs, 3), nil
	case "second":
		return decimalScaled(secUs, 6), nil
	case "minute":
		return decimalFromInt64(mi), nil
	case "hour":
		return decimalFromInt64(h), nil
	case "day":
		return decimalFromInt64(d), nil
	case "month":
		return decimalFromInt64(mo), nil
	case "quarter":
		return decimalFromInt64((mo-1)/3 + 1), nil
	case "year":
		return decimalFromInt64(toPgYear(y)), nil
	case "decade":
		return decimalFromInt64(extractDecade(toPgYear(y))), nil
	case "century":
		return decimalFromInt64(extractCentury(toPgYear(y))), nil
	case "millennium":
		return decimalFromInt64(extractMillennium(toPgYear(y))), nil
	case "week":
		w, _ := isoWeekYear(y, mo, d)
		return decimalFromInt64(w), nil
	case "dow":
		return decimalFromInt64(dowSun0(days)), nil
	case "isodow":
		return decimalFromInt64(isodowMon1(days)), nil
	case "doy":
		return decimalFromInt64(days - daysFromCivil(y, 1, 1) + 1), nil
	case "isoyear":
		_, iy := isoWeekYear(y, mo, d)
		return decimalFromInt64(toPgYear(iy)), nil
	case "epoch":
		inst := micros
		if isTz {
			inst = instant
		}
		return decimalScaled(inst, 6), nil
	case "timezone", "timezone_hour", "timezone_minute":
		if !isTz {
			return Decimal{}, unitNotSupported(name, timestampTypeName(false))
		}
		switch unit {
		case "timezone":
			return decimalFromInt64(offsetSecs), nil
		case "timezone_hour":
			return decimalFromInt64(offsetSecs / 3600), nil
		default:
			return decimalFromInt64((offsetSecs % 3600) / 60), nil
		}
	case "julian":
		return extractJulian(micros)
	default:
		return Decimal{}, unitNotSupported(name, timestampTypeName(isTz))
	}
}

// extractDate extracts the canonical `unit` (written as `name`, for messages) from a date.
func extractDate(unit, name string, days int64) (Decimal, error) {
	if days == int64(datePosInfinity) || days == int64(dateNegInfinity) {
		// As for an infinite timestamp: an unsupported field is still 0A000 (checked on a finite
		// probe), else 22003 where PG returns ±Infinity or NULL (timezones.md §9.2).
		if _, err := extractDate(unit, name, 0); err != nil {
			return Decimal{}, err
		}
		return Decimal{}, newError(NumericValueOutOfRange, "cannot extract field from an infinite date")
	}
	y, mo, d := civilFromDays(days)
	switch unit {
	case "day":
		return decimalFromInt64(d), nil
	case "month":
		return decimalFromInt64(mo), nil
	case "quarter":
		return decimalFromInt64((mo-1)/3 + 1), nil
	case "year":
		return decimalFromInt64(toPgYear(y)), nil
	case "decade":
		return decimalFromInt64(extractDecade(toPgYear(y))), nil
	case "century":
		return decimalFromInt64(extractCentury(toPgYear(y))), nil
	case "millennium":
		return decimalFromInt64(extractMillennium(toPgYear(y))), nil
	case "week":
		w, _ := isoWeekYear(y, mo, d)
		return decimalFromInt64(w), nil
	case "dow":
		return decimalFromInt64(dowSun0(days)), nil
	case "isodow":
		return decimalFromInt64(isodowMon1(days)), nil
	case "doy":
		return decimalFromInt64(days - daysFromCivil(y, 1, 1) + 1), nil
	case "isoyear":
		_, iy := isoWeekYear(y, mo, d)
		return decimalFromInt64(toPgYear(iy)), nil
	case "epoch":
		return decimalFromInt64(days * secsPerDayDT), nil
	case "julian":
		return decimalFromInt64(days + unixEpochJD), nil
	default:
		return Decimal{}, unitNotSupported(name, "date")
	}
}

// extractInterval extracts the canonical `unit` (written as `name`, for messages) from an interval.
func extractInterval(unit, name string, iv Interval) (Decimal, error) {
	months := int64(iv.Months)
	days := int64(iv.Days)
	micros := iv.Micros
	years := months / 12
	mo := months % 12
	timeSecUs := micros % microsPerMinDT
	switch unit {
	case "microseconds":
		return decimalScaled(timeSecUs, 0), nil
	case "milliseconds":
		return decimalScaled(timeSecUs, 3), nil
	case "second":
		return decimalScaled(timeSecUs, 6), nil
	case "minute":
		return decimalFromInt64((micros / microsPerMinDT) % 60), nil
	case "hour":
		return decimalFromInt64(micros / microsPerHourDT), nil
	case "day":
		return decimalFromInt64(days), nil
	case "week":
		return decimalFromInt64(days / 7), nil
	case "month":
		return decimalFromInt64(mo), nil
	case "quarter":
		return decimalFromInt64(intervalQuarter(months)), nil
	case "year":
		return decimalFromInt64(years), nil
	case "decade":
		return decimalFromInt64(years / 10), nil
	case "century":
		return decimalFromInt64(years / 100), nil
	case "millennium":
		return decimalFromInt64(years / 1000), nil
	case "epoch":
		intSecs := years*secsPerIntervalYear + mo*secsPerIntervalMonth + days*secsPerDayDT
		return decimalFromInt64(intSecs).Add(decimalScaled(micros, 6))
	default:
		return Decimal{}, unitNotSupported(name, "interval")
	}
}
