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
	"strings"
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

func intervalQuarter(mo int64) int64 {
	if mo == 0 {
		return 1
	}
	sign := int64(1)
	if mo < 0 {
		sign = -1
	}
	return sign * (absI64(mo)/3 + 1)
}

// DateTruncMicros truncates a wall-clock instant `micros` down to the start of `unit`. ±infinity
// passes through; an unrecognized unit is 22023.
func dateTruncMicros(unit string, micros int64) (int64, error) {
	if micros == posInfinity || micros == negInfinity {
		return micros, nil
	}
	u := strings.ToLower(unit)
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
		return 0, newError(InvalidParameterValue, "unit \""+unit+"\" not recognized")
	}
}

// dateTruncInterval truncates an interval's fields down to `unit`. `week` is 0A000; an unrecognized
// unit is 22023.
func dateTruncInterval(unit string, iv Interval) (Interval, error) {
	u := strings.ToLower(unit)
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
		return Interval{}, newError(FeatureNotSupported, "unit \"week\" not supported for type interval")
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
		return Interval{}, newError(InvalidParameterValue, "unit \""+unit+"\" not recognized")
	}
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

// extractField returns EXTRACT(field FROM source) as numeric. An unsupported field for the type is
// 0A000; an unrecognized field is 22023; julian is a deferred field (0A000).
func extractField(field string, src extractSrc) (Decimal, error) {
	f := strings.ToLower(field)
	switch src.kind {
	case srcTs:
		return extractDatetime(f, src.local, false, 0, 0)
	case srcTstz:
		return extractDatetime(f, src.local, true, src.instant, src.offsetSecs)
	case srcDate:
		return extractDate(f, int64(src.days))
	default:
		return extractInterval(f, src.iv)
	}
}

func extractDatetime(field string, micros int64, isTz bool, instant, offsetSecs int64) (Decimal, error) {
	if micros == posInfinity || micros == negInfinity {
		// jed's decimal is finite-only (decimal.md §2); PG returns ±Infinity — a documented
		// divergence (timezones.md §9.2).
		return Decimal{}, newError(NumericValueOutOfRange, "cannot extract field from an infinite timestamp")
	}
	y, mo, d, h, mi, s, us := civilFromMicros(micros)
	secUs := s*microsPerSecDT + us
	days := daysFromCivil(y, mo, d)
	switch field {
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
			return Decimal{}, newError(FeatureNotSupported,
				"unit \""+field+"\" not supported for type timestamp without time zone")
		}
		switch field {
		case "timezone":
			return decimalFromInt64(offsetSecs), nil
		case "timezone_hour":
			return decimalFromInt64(offsetSecs / 3600), nil
		default:
			return decimalFromInt64((offsetSecs % 3600) / 60), nil
		}
	case "julian":
		return Decimal{}, newError(FeatureNotSupported, "unit \"julian\" not supported yet (jed deferred)")
	default:
		return Decimal{}, newError(InvalidParameterValue, "unit \""+field+"\" not recognized")
	}
}

func extractDate(field string, days int64) (Decimal, error) {
	y, mo, d := civilFromDays(days)
	switch field {
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
	case "microseconds", "milliseconds", "second", "minute", "hour",
		"timezone", "timezone_hour", "timezone_minute", "julian":
		return Decimal{}, newError(FeatureNotSupported, "unit \""+field+"\" not supported for type date")
	default:
		return Decimal{}, newError(InvalidParameterValue, "unit \""+field+"\" not recognized")
	}
}

func extractInterval(field string, iv Interval) (Decimal, error) {
	months := int64(iv.Months)
	days := int64(iv.Days)
	micros := iv.Micros
	years := months / 12
	mo := months % 12
	timeSecUs := micros % microsPerMinDT
	switch field {
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
		return decimalFromInt64(intervalQuarter(mo)), nil
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
	case "dow", "isodow", "doy", "isoyear", "julian",
		"timezone", "timezone_hour", "timezone_minute":
		return Decimal{}, newError(FeatureNotSupported, "unit \""+field+"\" not supported for type interval")
	default:
		return Decimal{}, newError(InvalidParameterValue, "unit \""+field+"\" not recognized")
	}
}
