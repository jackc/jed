package jed

// The data-type formatting functions — to_char, to_timestamp, to_date, to_number
// (spec/design/formatting.md). A transcription of PostgreSQL 18's formatting.c: the template parser
// (parse_format), the datetime renderer (DCH_to_char) and reader (DCH_from_char + do_to_timestamp),
// and the number processor (NUM_processor). The template vocabulary is shared spec data
// (formatting_gen.go from spec/types/formatting.toml); everything here is the hand-written logic
// that consumes it. jed has no locale, so every locale-dependent piece behaves as PostgreSQL does
// under the C locale: TM renders the English names, and D / G / L / S / MI / PL are ".", ",", " ",
// "-", "+". The processors work on UTF-8 BYTES, exactly as formatting.c does, so the three cores
// agree byte for byte; C int arithmetic is reproduced with int32 (PostgreSQL builds with -fwrapv,
// so its int overflow wraps) wherever an operand can leave the int range.

import (
	"math"
	"strconv"
	"strings"
)

// --- template nodes --------------------------------------------------------------------------

// The format-node kinds (formatting.c NODE_TYPE_*).
const (
	fnEnd = iota
	fnAction
	fnChar
	fnSeparator
	fnSpace
)

// fmtNode is one parsed template element: a pattern (key) with its suffix bits, or one literal
// character (chars, a whole UTF-8 sequence).
type fmtNode struct {
	typ    int
	chars  string
	suffix int
	key    *fmtKeyword
}

// isSpaceC is C's isspace in the C locale.
func isSpaceC(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n' || b == '\v' || b == '\f' || b == '\r'
}

func isDigitC(b byte) bool { return b >= '0' && b <= '9' }

func isAlphaC(b byte) bool { return (b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z') }

// isSeparatorChar is formatting.c's is_separator_char: printable ASCII that is not a letter or digit.
func isSeparatorChar(b byte) bool {
	return b > 0x20 && b < 0x7f && !isAlphaC(b) && !isDigitC(b)
}

// pgMblen is PostgreSQL's pg_utf_mblen of the character at s[i], clipped to the string.
func pgMblen(s string, i int) int {
	c := s[i]
	n := 1
	switch {
	case c&0x80 == 0:
		n = 1
	case c&0xe0 == 0xc0:
		n = 2
	case c&0xf0 == 0xe0:
		n = 3
	case c&0xf8 == 0xf0:
		n = 4
	}
	if i+n > len(s) {
		n = len(s) - i
	}
	return n
}

// fmtIndexSearch is formatting.c's index_seq_search: the first keyword, in table order from the
// first one sharing s's first byte, whose name is a prefix of s.
func fmtIndexSearch(s string, kws []fmtKeyword, index *[128]int16) *fmtKeyword {
	c := s[0]
	if c <= ' ' || c >= '~' {
		return nil
	}
	k := int(index[c])
	if k < 0 {
		return nil
	}
	for {
		if strings.HasPrefix(s, kws[k].name) {
			return &kws[k]
		}
		k++
		if k == len(kws) || kws[k].name[0] != c {
			return nil
		}
	}
}

// fmtSuffixSearch is formatting.c's suff_search over DCH_suff for one suffix type.
func fmtSuffixSearch(s string, prefix bool) *fmtSuffix {
	for i := range dchSuffixes {
		sf := &dchSuffixes[i]
		if sf.prefix == prefix && strings.HasPrefix(s, sf.name) {
			return sf
		}
	}
	return nil
}

// parseFormat is formatting.c's parse_format: the template as a node list ending in an fnEnd node.
// A number template (dch = false) also fills num, which can reject the template (42601).
func parseFormat(str string, dch bool, num *numDesc) ([]fmtNode, error) {
	kws, index := numKeywords, &numIndex
	if dch {
		kws, index = dchKeywords, &dchIndex
	}
	nodes := make([]fmtNode, 0, len(str)+1)
	i := 0
	for i < len(str) {
		suffix := 0
		if dch {
			if sf := fmtSuffixSearch(str[i:], true); sf != nil {
				suffix |= sf.flag
				i += len(sf.name)
			}
		}
		if i >= len(str) {
			break
		}
		if k := fmtIndexSearch(str[i:], kws, index); k != nil {
			n := fmtNode{typ: fnAction, suffix: suffix, key: k}
			i += len(k.name)
			if !dch {
				if err := num.prepare(k.id); err != nil {
					return nil, err
				}
			}
			if dch && i < len(str) {
				if sf := fmtSuffixSearch(str[i:], false); sf != nil {
					n.suffix |= sf.flag
					i += len(sf.name)
				}
			}
			nodes = append(nodes, n)
			continue
		}
		if str[i] == '"' {
			// A double-quoted literal; a backslash quotes the next character.
			i++
			for i < len(str) {
				if str[i] == '"' {
					i++
					break
				}
				if str[i] == '\\' && i+1 < len(str) {
					i++
				}
				cl := pgMblen(str, i)
				nodes = append(nodes, fmtNode{typ: fnChar, chars: str[i : i+cl]})
				i += cl
			}
			continue
		}
		// Outside quotes a backslash is special only before a double quote.
		if str[i] == '\\' && i+1 < len(str) && str[i+1] == '"' {
			i++
		}
		cl := pgMblen(str, i)
		typ := fnChar
		if dch && isSeparatorChar(str[i]) {
			typ = fnSeparator
		} else if isSpaceC(str[i]) {
			typ = fnSpace
		}
		nodes = append(nodes, fmtNode{typ: typ, chars: str[i : i+cl]})
		i += cl
	}
	return append(nodes, fmtNode{typ: fnEnd}), nil
}

// --- C formatting helpers --------------------------------------------------------------------

// cPadInt is C's "%0*d": v zero-padded to width w (the sign counts toward the width; 0 = none).
func cPadInt(v int64, w int) string {
	s := strconv.FormatInt(v, 10)
	if len(s) >= w {
		return s
	}
	if v < 0 {
		return "-" + strings.Repeat("0", w-len(s)) + s[1:]
	}
	return strings.Repeat("0", w-len(s)) + s
}

// cPadRight is C's "%-*s": s space-padded on the right to w bytes.
func cPadRight(s string, w int) string {
	if len(s) >= w {
		return s
	}
	return s + strings.Repeat(" ", w-len(s))
}

// cPadLeft is C's "%*s": s space-padded on the left to w bytes.
func cPadLeft(s string, w int) string {
	if len(s) >= w {
		return s
	}
	return strings.Repeat(" ", w-len(s)) + s
}

// fmtOrdinal is formatting.c's get_th: the ordinal suffix of a rendered number (22P02 when its
// last byte is not a digit).
func fmtOrdinal(num string, upper bool) (string, error) {
	last := num[len(num)-1]
	if !isDigitC(last) {
		return "", newError(InvalidTextRepresentation, strconv.Quote(num)+" is not a number")
	}
	if len(num) > 1 && num[len(num)-2] == '1' {
		last = 0
	}
	idx := 3
	switch last {
	case '1':
		idx = 0
	case '2':
		idx = 1
	case '3':
		idx = 2
	}
	if upper {
		return fmtWordOrdinalUpper[idx], nil
	}
	return fmtWordOrdinalLower[idx], nil
}

// asciiUpperStr / asciiLowerStr are PostgreSQL's asc_toupper / asc_tolower.
func asciiUpperStr(s string) string {
	b := []byte(s)
	for i, c := range b {
		if c >= 'a' && c <= 'z' {
			b[i] = c - 32
		}
	}
	return string(b)
}

func asciiLowerStr(s string) string {
	b := []byte(s)
	for i, c := range b {
		if c >= 'A' && c <= 'Z' {
			b[i] = c + 32
		}
	}
	return string(b)
}

// --- PostgreSQL's calendar helpers (datetime.c / timestamp.c), C int arithmetic --------------

// pgDate2j is datetime.c's date2j: the Julian day of a (proleptic Gregorian) date, in wrapping
// int32 arithmetic.
func pgDate2j(year, month, day int32) int32 {
	if month > 2 {
		month++
		year += 4800
	} else {
		month += 13
		year += 4799
	}
	century := year / 100
	julian := year*365 - 32167
	julian += year/4 - century + century/4
	julian += 7834*month/256 + day
	return julian
}

// pgJ2date is datetime.c's j2date (unsigned int arithmetic).
func pgJ2date(jd int32) (year, month, day int32) {
	julian := uint32(jd)
	julian += 32044
	quad := julian / 146097
	extra := (julian-quad*146097)*4 + 3
	julian += 60 + quad*3 + extra/146097
	quad = julian / 1461
	julian -= quad * 1461
	y := int32(julian * 4 / 1461)
	if y != 0 {
		julian = (julian+305)%365 + 123
	} else {
		julian = (julian+306)%366 + 123
	}
	y += int32(quad * 4)
	year = y - 4800
	quad = julian * 2141 / 65536
	day = int32(julian - 7834*quad/256)
	month = int32((quad+10)%12 + 1)
	return year, month, day
}

// pgJ2day is datetime.c's j2day: 0..6 = Sunday..Saturday.
func pgJ2day(date int32) int32 {
	date++
	date %= 7
	if date < 0 {
		date += 7
	}
	return date
}

// pgIsoweek2j is timestamp.c's isoweek2j.
func pgIsoweek2j(year, week int32) int32 {
	day4 := pgDate2j(year, 1, 4)
	day0 := pgJ2day(day4 - 1)
	return (week-1)*7 + (day4 - day0)
}

// pgDate2isoweek is timestamp.c's date2isoweek.
func pgDate2isoweek(year, mon, mday int32) int32 {
	dayn := pgDate2j(year, mon, mday)
	day4 := pgDate2j(year, 1, 4)
	day0 := pgJ2day(day4 - 1)
	if dayn < day4-day0 {
		day4 = pgDate2j(year-1, 1, 4)
		day0 = pgJ2day(day4 - 1)
	}
	result := (dayn-(day4-day0))/7 + 1
	if result >= 52 {
		day4 = pgDate2j(year+1, 1, 4)
		day0 = pgJ2day(day4 - 1)
		if dayn >= day4-day0 {
			result = (dayn-(day4-day0))/7 + 1
		}
	}
	return result
}

// pgDate2isoyear is timestamp.c's date2isoyear.
func pgDate2isoyear(year, mon, mday int32) int32 {
	dayn := pgDate2j(year, mon, mday)
	day4 := pgDate2j(year, 1, 4)
	day0 := pgJ2day(day4 - 1)
	if dayn < day4-day0 {
		day4 = pgDate2j(year-1, 1, 4)
		day0 = pgJ2day(day4 - 1)
		year--
	}
	result := (dayn-(day4-day0))/7 + 1
	if result >= 52 {
		day4 = pgDate2j(year+1, 1, 4)
		day0 = pgJ2day(day4 - 1)
		if dayn >= day4-day0 {
			year++
		}
	}
	return year
}

// pgDate2isoyearday is timestamp.c's date2isoyearday.
func pgDate2isoyearday(year, mon, mday int32) int32 {
	return pgDate2j(year, mon, mday) - pgIsoweek2j(pgDate2isoyear(year, mon, mday), 1) + 1
}

// pgIsLeap is PostgreSQL's isleap.
func pgIsLeap(y int32) bool { return y%4 == 0 && (y%100 != 0 || y%400 == 0) }

var pgDayTab = [2][12]int32{
	{31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31},
	{31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31},
}

// --- to_char for datetimes and intervals (DCH_to_char) ---------------------------------------

// fmtTm is formatting.c's fmt_tm: a broken-down datetime or interval. hour is 64-bit (an
// interval's hours are unbounded); the rest are C ints.
type fmtTm struct {
	sec, min                    int32
	hour                        int64
	mday, mon, year, wday, yday int32
	gmtoff                      int64
}

func invalidForInterval() error {
	return newError(InvalidDatetimeFormat, "invalid format specification for an interval value")
}

// adjustYear is formatting.c's ADJUST_YEAR: a BC year shown positive (an interval's as is).
func adjustYear(year int32, isInterval bool) int32 {
	if isInterval {
		return year
	}
	if year <= 0 {
		return -(year - 1)
	}
	return year
}

// dchToChar is formatting.c's DCH_to_char.
func dchToChar(nodes []fmtNode, isInterval bool, tm *fmtTm, fsec int64, tzn *string) (string, error) {
	var b strings.Builder
	fm := func(n *fmtNode) bool { return n.suffix&dchSuffixFM != 0 }
	tmS := func(n *fmtNode) bool { return n.suffix&dchSuffixTM != 0 }
	// width is a "%0*d" width: 0 in fill mode, else w (w+1 for a negative value).
	width := func(n *fmtNode, w int, neg bool) int {
		if fm(n) {
			return 0
		}
		if neg {
			return w + 1
		}
		return w
	}
	// num writes a number with its optional ordinal suffix (TH / th).
	num := func(n *fmtNode, s string) error {
		if n.suffix&(dchSuffixTH|dchSuffixth) != 0 {
			th, err := fmtOrdinal(s, n.suffix&dchSuffixTH != 0)
			if err != nil {
				return err
			}
			s += th
		}
		b.WriteString(s)
		return nil
	}
	// word writes a padded (or, with TM / FM, bare) month or day name in the requested case.
	word := func(n *fmtNode, name string, caseMode int, pad bool) {
		switch caseMode {
		case 1:
			name = asciiUpperStr(name)
		case 2:
			name = asciiLowerStr(name)
		}
		if pad && !tmS(n) && !fm(n) {
			name = cPadRight(name, 9)
		}
		b.WriteString(name)
	}
	pm := tm.hour%24 >= 12
	for i := range nodes {
		n := &nodes[i]
		if n.typ == fnEnd {
			break
		}
		if n.typ != fnAction {
			b.WriteString(n.chars)
			continue
		}
		var err error
		switch n.key.id {
		case dchA_M, dchP_M:
			b.WriteString(map[bool]string{true: "P.M.", false: "A.M."}[pm])
		case dchAM, dchPM:
			b.WriteString(map[bool]string{true: "PM", false: "AM"}[pm])
		case dcha_m, dchp_m:
			b.WriteString(map[bool]string{true: "p.m.", false: "a.m."}[pm])
		case dcham, dchpm:
			b.WriteString(map[bool]string{true: "pm", false: "am"}[pm])
		case dchHH, dchHH12:
			v := tm.hour % 12
			if v == 0 {
				v = 12
			}
			err = num(n, cPadInt(v, width(n, 2, tm.hour < 0)))
		case dchHH24:
			err = num(n, cPadInt(tm.hour, width(n, 2, tm.hour < 0)))
		case dchMI:
			err = num(n, cPadInt(int64(tm.min), width(n, 2, tm.min < 0)))
		case dchSS:
			err = num(n, cPadInt(int64(tm.sec), width(n, 2, tm.sec < 0)))
		case dchFF1:
			err = num(n, cPadInt(fsec/100000, 1))
		case dchFF2:
			err = num(n, cPadInt(fsec/10000, 2))
		case dchFF3, dchMS:
			err = num(n, cPadInt(fsec/1000, 3))
		case dchFF4:
			err = num(n, cPadInt(fsec/100, 4))
		case dchFF5:
			err = num(n, cPadInt(fsec/10, 5))
		case dchFF6, dchUS:
			err = num(n, cPadInt(fsec, 6))
		case dchSSSS:
			err = num(n, strconv.FormatInt(tm.hour*3600+int64(tm.min)*60+int64(tm.sec), 10))
		case dchtz, dchTZ:
			if isInterval {
				return "", invalidForInterval()
			}
			if tzn != nil {
				p := *tzn
				if n.key.id == dchtz {
					p = asciiLowerStr(p)
				}
				if len(p) > len(n.key.name)*12 {
					return "", newError(DatetimeFieldOverflow, "time zone format value too long")
				}
				b.WriteString(p)
			}
		case dchTZH:
			if isInterval {
				return "", invalidForInterval()
			}
			b.WriteString(offsetSign(tm.gmtoff) + cPadInt(absI64(tm.gmtoff)/3600, 2))
		case dchTZM:
			if isInterval {
				return "", invalidForInterval()
			}
			b.WriteString(cPadInt((absI64(tm.gmtoff)%3600)/60, 2))
		case dchOF:
			if isInterval {
				return "", invalidForInterval()
			}
			w := 2
			if fm(n) {
				w = 0
			}
			b.WriteString(offsetSign(tm.gmtoff) + cPadInt(absI64(tm.gmtoff)/3600, w))
			if absI64(tm.gmtoff)%3600 != 0 {
				b.WriteString(":" + cPadInt((absI64(tm.gmtoff)%3600)/60, 2))
			}
		case dchA_D, dchB_C, dchAD, dchBC, dcha_d, dchb_c, dchad, dchbc:
			if isInterval {
				return "", invalidForInterval()
			}
			bc := tm.year <= 0
			var s string
			switch n.key.id {
			case dchA_D, dchB_C:
				s = map[bool]string{true: "B.C.", false: "A.D."}[bc]
			case dchAD, dchBC:
				s = map[bool]string{true: "BC", false: "AD"}[bc]
			case dcha_d, dchb_c:
				s = map[bool]string{true: "b.c.", false: "a.d."}[bc]
			default:
				s = map[bool]string{true: "bc", false: "ad"}[bc]
			}
			b.WriteString(s)
		case dchMONTH, dchMonth, dchmonth:
			if isInterval {
				return "", invalidForInterval()
			}
			if tm.mon == 0 {
				break
			}
			word(n, fmtWordMonthsFull[tm.mon-1], caseOf(n.key.id, dchMONTH, dchmonth), true)
		case dchMON, dchMon, dchmon:
			if isInterval {
				return "", invalidForInterval()
			}
			if tm.mon == 0 {
				break
			}
			word(n, fmtWordMonthsAbbrev[tm.mon-1], caseOf(n.key.id, dchMON, dchmon), false)
		case dchMM:
			err = num(n, cPadInt(int64(tm.mon), width(n, 2, tm.mon < 0)))
		case dchDAY, dchDay, dchday:
			if isInterval {
				return "", invalidForInterval()
			}
			word(n, fmtWordDaysFull[tm.wday], caseOf(n.key.id, dchDAY, dchday), true)
		case dchDY, dchDy, dchdy:
			if isInterval {
				return "", invalidForInterval()
			}
			word(n, fmtWordDaysAbbrev[tm.wday], caseOf(n.key.id, dchDY, dchdy), false)
		case dchDDD, dchIDDD:
			v := tm.yday
			if n.key.id == dchIDDD {
				v = pgDate2isoyearday(tm.year, tm.mon, tm.mday)
			}
			err = num(n, cPadInt(int64(v), width(n, 3, false)))
		case dchDD:
			err = num(n, cPadInt(int64(tm.mday), width(n, 2, false)))
		case dchD:
			if isInterval {
				return "", invalidForInterval()
			}
			err = num(n, strconv.Itoa(int(tm.wday)+1))
		case dchID:
			if isInterval {
				return "", invalidForInterval()
			}
			v := tm.wday
			if v == 0 {
				v = 7
			}
			err = num(n, strconv.Itoa(int(v)))
		case dchWW:
			err = num(n, cPadInt(int64((tm.yday-1)/7+1), width(n, 2, false)))
		case dchIW:
			err = num(n, cPadInt(int64(pgDate2isoweek(tm.year, tm.mon, tm.mday)), width(n, 2, false)))
		case dchQ:
			if tm.mon == 0 {
				break
			}
			err = num(n, strconv.Itoa(int((tm.mon-1)/3+1)))
		case dchCC:
			var c int32
			switch {
			case isInterval:
				c = tm.year / 100
			case tm.year > 0:
				c = (tm.year-1)/100 + 1
			default:
				c = tm.year/100 - 1
			}
			if c <= 99 && c >= -99 {
				err = num(n, cPadInt(int64(c), width(n, 2, c < 0)))
			} else {
				err = num(n, strconv.Itoa(int(c)))
			}
		case dchY_YYY:
			y := adjustYear(tm.year, isInterval)
			k := y / 1000
			err = num(n, strconv.Itoa(int(k))+","+cPadInt(int64(y-k*1000), 3))
		case dchYYYY, dchIYYY, dchYYY, dchIYY, dchYY, dchIY, dchY, dchI:
			adj := adjustYear(tm.year, isInterval)
			y := adj
			switch n.key.id {
			case dchIYYY, dchIYY, dchIY, dchI:
				y = adjustYear(pgDate2isoyear(tm.year, tm.mon, tm.mday), isInterval)
			}
			var w int
			switch n.key.id {
			case dchYYYY, dchIYYY:
				w = width(n, 4, adj < 0)
			case dchYYY, dchIYY:
				y %= 1000
				w = width(n, 3, adj < 0)
			case dchYY, dchIY:
				y %= 100
				w = width(n, 2, adj < 0)
			default:
				y %= 10
				w = 1
			}
			err = num(n, cPadInt(int64(y), w))
		case dchRM, dchrm:
			if tm.mon == 0 && tm.year == 0 {
				break
			}
			months := fmtWordRmMonthsUpper
			if n.key.id == dchrm {
				months = fmtWordRmMonthsLower
			}
			var mon int32
			switch {
			case tm.mon == 0:
				if tm.year >= 0 {
					mon = 0
				} else {
					mon = 11
				}
			case tm.mon < 0:
				mon = -1 * (tm.mon + 1)
			default:
				mon = 12 - tm.mon
			}
			s := months[mon]
			if !fm(n) {
				s = cPadRight(s, 4)
			}
			b.WriteString(s)
		case dchW:
			err = num(n, strconv.Itoa(int((tm.mday-1)/7+1)))
		case dchJ:
			err = num(n, strconv.Itoa(int(pgDate2j(tm.year, tm.mon, tm.mday))))
		}
		if err != nil {
			return "", err
		}
	}
	return b.String(), nil
}

// caseOf maps a name pattern's id to its case: 1 upper, 2 lower, 0 the name as written (initcap).
func caseOf(id, upper, lower int) int {
	switch id {
	case upper:
		return 1
	case lower:
		return 2
	}
	return 0
}

func offsetSign(gmtoff int64) string {
	if gmtoff >= 0 {
		return "+"
	}
	return "-"
}

// tmFromWall broken-down a wall-clock microsecond reading (PostgreSQL's timestamp2tm plus the wday
// and yday to_char computes). fsec is the non-negative microsecond of the second.
func tmFromWall(micros int64) (fmtTm, int64) {
	y, mo, d, h, mi, s, us := civilFromMicros(micros)
	tm := fmtTm{sec: int32(s), min: int32(mi), hour: h, mday: int32(d), mon: int32(mo), year: int32(y)}
	thisdate := pgDate2j(tm.year, tm.mon, tm.mday)
	// PostgreSQL's (thisdate + 1) % 7; a floor modulo beyond its range, where a Julian day is negative.
	tm.wday = int32(floorMod(int64(thisdate)+1, 7))
	tm.yday = thisdate - pgDate2j(tm.year, 1, 1) + 1
	return tm, us
}

// datetimeToChar is formatting.c's datetime_to_char_body: an empty template is NULL.
func datetimeToChar(tm *fmtTm, fsec int64, tzn *string, tmpl string, isInterval bool) (Value, error) {
	if tmpl == "" {
		return NullValue(), nil
	}
	nodes, err := parseFormat(tmpl, true, nil)
	if err != nil {
		return Value{}, err
	}
	s, err := dchToChar(nodes, isInterval, tm, fsec, tzn)
	if err != nil {
		return Value{}, err
	}
	return TextValue(s), nil
}

// toCharTimestamp is to_char(timestamp, text): ±infinity is NULL.
func toCharTimestamp(micros int64, tmpl string) (Value, error) {
	if tmpl == "" || micros == posInfinity || micros == negInfinity {
		return NullValue(), nil
	}
	tm, fsec := tmFromWall(micros)
	return datetimeToChar(&tm, fsec, nil, tmpl, false)
}

// toCharTimestamptz is to_char(timestamptz, text): the instant decomposed in zone, with its offset
// (OF, TZH, TZM) and abbreviation (TZ). A fixed-offset zone other than UTC has no abbreviation, as
// PostgreSQL's ±HH:MM zone spelling has none. ±infinity is NULL.
func toCharTimestamptz(instant int64, zone ZoneRef, tmpl string, m *costMeter) (Value, error) {
	if tmpl == "" || instant == posInfinity || instant == negInfinity {
		return NullValue(), nil
	}
	m.Charge(costs.Timezone)
	if err := m.Guard(); err != nil {
		return Value{}, err
	}
	local, ok := instantToLocalMicros(zone, instant)
	if !ok {
		return Value{}, errZoneShiftOutOfRange()
	}
	off := offsetAtRef(zone, floorDiv(instant, 1_000_000))
	tm, fsec := tmFromWall(local)
	tm.gmtoff = int64(off.Utoff)
	tzn := off.Abbrev
	if zone.Fixed {
		tzn = ""
		if zone.Off == 0 {
			tzn = "UTC"
		}
	}
	return datetimeToChar(&tm, fsec, &tzn, tmpl, false)
}

// toCharInterval is to_char(interval, text) — PostgreSQL's interval2itm decomposition, with yday
// the approximate span in days.
func toCharInterval(iv Interval, tmpl string) (Value, error) {
	if tmpl == "" {
		return NullValue(), nil
	}
	t := iv.Micros
	hour := t / 3_600_000_000
	t -= hour * 3_600_000_000
	min := t / 60_000_000
	t -= min * 60_000_000
	sec := t / 1_000_000
	usec := t - sec*1_000_000
	tm := fmtTm{
		sec: int32(sec), min: int32(min), hour: hour, mday: iv.Days,
		mon: iv.Months % 12, year: iv.Months / 12,
	}
	tm.yday = (tm.year*12+tm.mon)*30 + tm.mday
	return datetimeToChar(&tm, usec, nil, tmpl, true)
}

// --- to_timestamp / to_date (DCH_from_char + do_to_timestamp) --------------------------------

// tmFromChar is formatting.c's TmFromChar: the fields a template read (0 = absent).
type tmFromChar struct {
	mode                                                      int
	hh, pm, mi, ss, ssss, d, dd, ddd, mm, ms, year, bc, ww, w int32
	cc, j, us, yysz, clock, tzsign, tzh, tzm, ff              int32
}

func (t *tmFromChar) setMode(mode int) error {
	if mode != fmtModeNone {
		if t.mode == fmtModeNone {
			t.mode = mode
		} else if t.mode != mode {
			return newError(InvalidDatetimeFormat, "invalid combination of date conventions")
		}
	}
	return nil
}

// fromCharSetInt is formatting.c's from_char_set_int: a field may be set twice only to one value.
func fromCharSetInt(dest *int32, value int32, n *fmtNode) error {
	if *dest != 0 && *dest != value {
		return newError(InvalidDatetimeFormat, "conflicting values for \""+n.key.name+"\" field in formatting string")
	}
	*dest = value
	return nil
}

// cStrtol is C's strtol(s + start, &end, 10): skip whitespace, an optional sign, then digits; no
// digits leaves end at start. erange reports a saturated overflow.
func cStrtol(s string, start int) (val int64, end int, erange bool) {
	i := start
	for i < len(s) && isSpaceC(s[i]) {
		i++
	}
	neg := false
	if i < len(s) && (s[i] == '+' || s[i] == '-') {
		neg = s[i] == '-'
		i++
	}
	ds := i
	var acc uint64
	limit := uint64(math.MaxInt64)
	if neg {
		limit++
	}
	over := false
	for i < len(s) && isDigitC(s[i]) {
		d := uint64(s[i] - '0')
		if !over && acc > (limit-d)/10 {
			over = true
		}
		if !over {
			acc = acc*10 + d
		}
		i++
	}
	if i == ds {
		return 0, start, false
	}
	if over {
		if neg {
			return math.MinInt64, i, true
		}
		return math.MaxInt64, i, true
	}
	if neg {
		return -int64(acc), i, false
	}
	return int64(acc), i, false
}

// dchReader is DCH_from_char's state: the input, the position, and the node list.
type dchReader struct {
	in    string
	pos   int
	nodes []fmtNode
}

// isNextSeparator is formatting.c's is_next_separator: whether the field at node i is followed by a
// non-digit, so a fixed-width read may take as many digits as are present.
func (r *dchReader) isNextSeparator(i int) bool {
	n := &r.nodes[i]
	if n.typ == fnEnd {
		return false
	}
	if n.typ == fnAction && n.suffix&(dchSuffixTH|dchSuffixth) != 0 {
		return true
	}
	next := &r.nodes[i+1]
	if next.typ == fnEnd {
		return true
	}
	if next.typ == fnAction {
		return !next.key.isDigit
	}
	if len(next.chars) == 1 && isDigitC(next.chars[0]) {
		return false
	}
	return true
}

// parseIntLen is formatting.c's from_char_parse_int_len: read an integer field of width ln (any
// width in fill mode or before a separator). dest may be nil (the value is checked, then dropped).
func (r *dchReader) parseIntLen(dest *int32, ln int, i int) (int, error) {
	n := &r.nodes[i]
	init := r.pos
	for r.pos < len(r.in) && isSpaceC(r.in[r.pos]) {
		r.pos++
	}
	end := r.pos + ln
	if end > len(r.in) {
		end = len(r.in)
	}
	cp := r.in[r.pos:end]
	used := len(r.in) - r.pos
	var result int64
	var erange bool
	if n.suffix&dchSuffixFM != 0 || r.isNextSeparator(i) {
		var e int
		result, e, erange = cStrtol(r.in, init)
		r.pos = e
	} else {
		if used < ln {
			return -1, newError(InvalidDatetimeFormat, "source string too short for \""+n.key.name+"\" formatting field")
		}
		var last int
		result, last, erange = cStrtol(cp, 0)
		if last > 0 && last < ln {
			return -1, newError(InvalidDatetimeFormat, "invalid value \""+cp+"\" for \""+n.key.name+"\"")
		}
		r.pos += last
	}
	if r.pos == init {
		return -1, newError(InvalidDatetimeFormat, "invalid value \""+cp+"\" for \""+n.key.name+"\"")
	}
	if erange || result < math.MinInt32 || result > math.MaxInt32 {
		return -1, newError(DatetimeFieldOverflow, "value for \""+n.key.name+"\" in source string is out of range")
	}
	if dest != nil {
		if err := fromCharSetInt(dest, int32(result), n); err != nil {
			return -1, err
		}
	}
	return r.pos - init, nil
}

func (r *dchReader) parseInt(dest *int32, i int) (int, error) {
	return r.parseIntLen(dest, len(r.nodes[i].key.name), i)
}

// seqSearchASCII is formatting.c's seq_search_ascii: the first word that, ASCII case-insensitively,
// is a prefix of s; its index and length, or -1.
func seqSearchASCII(s string, words []string) (int, int) {
	if s == "" {
		return -1, 0
	}
	for wi, w := range words {
		if len(w) <= len(s) && asciiLowerStr(s[:len(w)]) == asciiLowerStr(w) {
			return wi, len(w)
		}
	}
	return -1, 0
}

// seqSearch is formatting.c's from_char_seq_search. The TM (translated) form searches the same
// English words — jed's only locale is C — with ASCII case folding.
func (r *dchReader) seqSearch(words []string, i int) (int, error) {
	rest := r.in[r.pos:]
	idx, ln := seqSearchASCII(rest, words)
	if ln <= 0 {
		cp := rest
		for k := 0; k < len(cp); k++ {
			if isSpaceC(cp[k]) {
				cp = cp[:k]
				break
			}
		}
		return 0, newError(InvalidDatetimeFormat, "invalid value \""+cp+"\" for \""+r.nodes[i].key.name+"\"")
	}
	r.pos += ln
	return idx, nil
}

// skipTH is formatting.c's SKIP_THth: an ordinal suffix on a field skips two input characters.
func (r *dchReader) skipTH(suffix int) {
	if suffix&(dchSuffixTH|dchSuffixth) != 0 {
		for k := 0; k < 2; k++ {
			if r.pos < len(r.in) {
				r.pos += pgMblen(r.in, r.pos)
			}
		}
	}
}

// scanYYYY is sscanf(s, "%d,%03d%n"): the matched count, both numbers, and the bytes consumed.
func scanYYYY(s string) (matched int, millennia, years int32, nch int) {
	v, e, _ := cStrtol(s, 0)
	if e == 0 {
		return 0, 0, 0, 0
	}
	millennia = int32(v)
	if e >= len(s) || s[e] != ',' {
		return 1, millennia, 0, 0
	}
	i := e + 1
	for i < len(s) && isSpaceC(s[i]) {
		i++
	}
	end := i + 3
	if end > len(s) {
		end = len(s)
	}
	v2, e2, _ := cStrtol(s[:end], i)
	if e2 == i {
		return 1, millennia, 0, 0
	}
	return 2, millennia, int32(v2), e2
}

// dchFromChar is formatting.c's DCH_from_char (non-standard mode).
func dchFromChar(nodes []fmtNode, in string, out *tmFromChar) error {
	r := &dchReader{in: in, nodes: nodes}
	fxMode := false
	extraSkip := 0
	for i := 0; nodes[i].typ != fnEnd && r.pos < len(in); i++ {
		n := &nodes[i]
		if !fxMode && (n.typ != fnAction || n.key.id != dchFX) && (n.typ == fnAction || i == 0) {
			for r.pos < len(in) && isSpaceC(in[r.pos]) {
				r.pos++
				extraSkip++
			}
		}
		if n.typ == fnSpace || n.typ == fnSeparator {
			if !fxMode {
				extraSkip--
				if r.pos < len(in) && (isSpaceC(in[r.pos]) || isSeparatorChar(in[r.pos])) {
					r.pos++
					extraSkip++
				}
			} else if r.pos < len(in) {
				r.pos += pgMblen(in, r.pos)
			}
			continue
		} else if n.typ != fnAction {
			if !fxMode && extraSkip > 0 {
				extraSkip--
			} else if r.pos < len(in) {
				r.pos += pgMblen(in, r.pos)
			}
			continue
		}
		if err := out.setMode(n.key.dateMode); err != nil {
			return err
		}
		var err error
		var ln int
		switch n.key.id {
		case dchFX:
			fxMode = true
		case dchA_M, dchP_M, dcha_m, dchp_m, dchAM, dchPM, dcham, dchpm:
			words := fmtWordAmpm
			if n.key.id == dchA_M || n.key.id == dchP_M || n.key.id == dcha_m || n.key.id == dchp_m {
				words = fmtWordAmpmLong
			}
			var v int
			if v, err = r.seqSearch(words, i); err == nil {
				err = fromCharSetInt(&out.pm, int32(v%2), n)
				out.clock = 12
			}
		case dchHH, dchHH12:
			if _, err = r.parseIntLen(&out.hh, 2, i); err == nil {
				out.clock = 12
				r.skipTH(n.suffix)
			}
		case dchHH24:
			if _, err = r.parseIntLen(&out.hh, 2, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchMI:
			if _, err = r.parseInt(&out.mi, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchSS:
			if _, err = r.parseInt(&out.ss, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchMS:
			if ln, err = r.parseIntLen(&out.ms, 3, i); err == nil {
				switch ln {
				case 1:
					out.ms *= 100
				case 2:
					out.ms *= 10
				}
				r.skipTH(n.suffix)
			}
		case dchFF1, dchFF2, dchFF3, dchFF4, dchFF5, dchFF6, dchUS:
			w := 6
			if n.key.id != dchUS {
				out.ff = int32(n.key.id - dchFF1 + 1)
				w = int(out.ff)
			}
			if ln, err = r.parseIntLen(&out.us, w, i); err == nil {
				switch ln {
				case 1:
					out.us *= 100000
				case 2:
					out.us *= 10000
				case 3:
					out.us *= 1000
				case 4:
					out.us *= 100
				case 5:
					out.us *= 10
				}
				r.skipTH(n.suffix)
			}
		case dchSSSS:
			if _, err = r.parseInt(&out.ssss, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchtz, dchTZ, dchOF, dchTZH:
			if (n.key.id == dchtz || n.key.id == dchTZ) && r.pos < len(in) && isAlphaC(in[r.pos]) {
				// A zone abbreviation: jed has no abbreviation table (timezones.md §15).
				return newError(FeatureNotSupported, "time zone abbreviations are not supported in to_timestamp input")
			}
			// OF / TZ read TZH[:TZM]; a sign may have been skipped as a separator (TZH).
			if r.pos < len(in) && (in[r.pos] == '+' || in[r.pos] == '-' || in[r.pos] == ' ') {
				out.tzsign = 1
				if in[r.pos] == '-' {
					out.tzsign = -1
				}
				r.pos++
			} else if extraSkip > 0 && r.pos > 0 && in[r.pos-1] == '-' {
				out.tzsign = -1
			} else {
				out.tzsign = 1
			}
			if _, err = r.parseIntLen(&out.tzh, 2, i); err == nil && n.key.id != dchTZH {
				if r.pos < len(in) && in[r.pos] == ':' {
					r.pos++
					_, err = r.parseIntLen(&out.tzm, 2, i)
				}
			}
		case dchTZM:
			if out.tzsign == 0 {
				out.tzsign = 1
			}
			_, err = r.parseIntLen(&out.tzm, 2, i)
		case dchA_D, dchB_C, dcha_d, dchb_c, dchAD, dchBC, dchad, dchbc:
			words := fmtWordAdbc
			if n.key.id == dchA_D || n.key.id == dchB_C || n.key.id == dcha_d || n.key.id == dchb_c {
				words = fmtWordAdbcLong
			}
			var v int
			if v, err = r.seqSearch(words, i); err == nil {
				err = fromCharSetInt(&out.bc, int32(v%2), n)
			}
		case dchMONTH, dchMonth, dchmonth:
			var v int
			if v, err = r.seqSearch(fmtWordMonthsFull, i); err == nil {
				err = fromCharSetInt(&out.mm, int32(v+1), n)
			}
		case dchMON, dchMon, dchmon:
			var v int
			if v, err = r.seqSearch(fmtWordMonthsAbbrev, i); err == nil {
				err = fromCharSetInt(&out.mm, int32(v+1), n)
			}
		case dchMM:
			if _, err = r.parseInt(&out.mm, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchDAY, dchDay, dchday, dchDY, dchDy, dchdy:
			words := fmtWordDaysFull
			if n.key.id == dchDY || n.key.id == dchDy || n.key.id == dchdy {
				words = fmtWordDaysAbbrev
			}
			var v int
			if v, err = r.seqSearch(words, i); err == nil {
				if err = fromCharSetInt(&out.d, int32(v), n); err == nil {
					out.d++
				}
			}
		case dchDDD:
			if _, err = r.parseInt(&out.ddd, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchIDDD:
			if _, err = r.parseIntLen(&out.ddd, 3, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchDD:
			if _, err = r.parseInt(&out.dd, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchD:
			if _, err = r.parseInt(&out.d, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchID:
			if _, err = r.parseIntLen(&out.d, 1, i); err == nil {
				// Shift to the Gregorian numbering, Sunday = 1.
				out.d++
				if out.d > 7 {
					out.d = 1
				}
				r.skipTH(n.suffix)
			}
		case dchWW, dchIW:
			if _, err = r.parseInt(&out.ww, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchQ:
			// Q is read and checked but ignored (PostgreSQL).
			if _, err = r.parseInt(nil, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchCC:
			if _, err = r.parseInt(&out.cc, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchY_YYY:
			matched, millennia, years, nch := scanYYYY(in[r.pos:])
			if matched < 2 {
				return newError(InvalidDatetimeFormat, "invalid value \""+in[r.pos:]+"\" for \"Y,YYY\"")
			}
			m64 := int64(millennia) * 1000
			y64 := int64(years) + m64
			if m64 != int64(int32(m64)) || y64 != int64(int32(y64)) {
				return newError(DatetimeFieldOverflow, "value for \"Y,YYY\" in source string is out of range")
			}
			if err = fromCharSetInt(&out.year, int32(y64), n); err == nil {
				out.yysz = 4
				r.pos += nch
				r.skipTH(n.suffix)
			}
		case dchYYYY, dchIYYY:
			if _, err = r.parseInt(&out.year, i); err == nil {
				out.yysz = 4
				r.skipTH(n.suffix)
			}
		case dchYYY, dchIYY, dchYY, dchIY, dchY, dchI:
			if ln, err = r.parseInt(&out.year, i); err == nil {
				if ln < 4 {
					out.year = adjustPartialYear(out.year)
				}
				out.yysz = int32(len(n.key.name))
				r.skipTH(n.suffix)
			}
		case dchRM, dchrm:
			var v int
			if v, err = r.seqSearch(fmtWordRmMonthsLower, i); err == nil {
				err = fromCharSetInt(&out.mm, int32(12-v), n)
			}
		case dchW:
			if _, err = r.parseInt(&out.w, i); err == nil {
				r.skipTH(n.suffix)
			}
		case dchJ:
			if _, err = r.parseInt(&out.j, i); err == nil {
				r.skipTH(n.suffix)
			}
		}
		if err != nil {
			return err
		}
		// Ignore the spaces after a field.
		if !fxMode {
			extraSkip = 0
			for r.pos < len(in) && isSpaceC(in[r.pos]) {
				r.pos++
				extraSkip++
			}
		}
	}
	return nil
}

// adjustPartialYear is formatting.c's adjust_partial_year_to_2020.
func adjustPartialYear(year int32) int32 {
	switch {
	case year < 70:
		return year + 2000
	case year < 100:
		return year + 1900
	case year < 520:
		return year + 2000
	case year < 1000:
		return year + 1000
	}
	return year
}

// pgTm is the broken-down result of do_to_timestamp (PostgreSQL's pg_tm subset).
type pgTm struct {
	year, mon, mday, hour, min, sec int32
}

func fieldOverflow(input string) error {
	return newError(DatetimeFieldOverflow, "date/time field value out of range: \""+input+"\"")
}

func mulS32(a, b int32) (int32, bool) {
	r := int64(a) * int64(b)
	return int32(r), r != int64(int32(r))
}

func addS32(a, b int32) (int32, bool) {
	r := int64(a) + int64(b)
	return int32(r), r != int64(int32(r))
}

// toTimestampParts is formatting.c's do_to_timestamp: the date/time fields, the microseconds, the
// fractional precision FFn asked for (0 = none), and the explicit zone offset (east of UTC) if the
// template read one.
func toTimestampParts(input, tmpl string) (tm pgTm, fsec int32, fprec int32, hasTz bool, tzEast int64, err error) {
	var tmfc tmFromChar
	tm = pgTm{mon: 1, mday: 1}
	if tmpl != "" {
		nodes, perr := parseFormat(tmpl, true, nil)
		if perr != nil {
			return tm, 0, 0, false, 0, perr
		}
		if err = dchFromChar(nodes, input, &tmfc); err != nil {
			return
		}
	}
	const (
		mYear = 1 << iota
		mMonth
		mDay
	)
	fmask := 0
	if tmfc.ssss != 0 {
		x := tmfc.ssss
		tm.hour = x / 3600
		x %= 3600
		tm.min = x / 60
		x %= 60
		tm.sec = x
	}
	if tmfc.ss != 0 {
		tm.sec = tmfc.ss
	}
	if tmfc.mi != 0 {
		tm.min = tmfc.mi
	}
	if tmfc.hh != 0 {
		tm.hour = tmfc.hh
	}
	if tmfc.clock == 12 {
		if tm.hour < 1 || tm.hour > 12 {
			err = newError(InvalidDatetimeFormat, "hour \""+strconv.Itoa(int(tm.hour))+"\" is invalid for the 12-hour clock")
			return
		}
		if tmfc.pm != 0 && tm.hour < 12 {
			tm.hour += 12
		} else if tmfc.pm == 0 && tm.hour == 12 {
			tm.hour = 0
		}
	}
	if tmfc.year != 0 {
		// CC with YY (or Y) puts the two low-order digits in that century; a 4-digit year wins.
		if tmfc.cc != 0 && tmfc.yysz <= 2 {
			if tmfc.bc != 0 {
				tmfc.cc = -tmfc.cc
			}
			tm.year = tmfc.year % 100
			if tm.year != 0 {
				if tmfc.cc >= 0 {
					tmp, o1 := mulS32(tmfc.cc-1, 100)
					y, o2 := addS32(tm.year, tmp)
					if o1 || o2 {
						err = fieldOverflow(input)
						return
					}
					tm.year = y
				} else {
					tmp, o1 := mulS32(tmfc.cc+1, 100)
					tmp2, o2 := addS32(tmp, -tm.year)
					y, o3 := addS32(tmp2, 1)
					if o1 || o2 || o3 {
						err = fieldOverflow(input)
						return
					}
					tm.year = y
				}
			} else {
				// The century year of a date ending in "00".
				adj := int32(0)
				if tmfc.cc < 0 {
					adj = 1
				}
				tm.year = tmfc.cc*100 + adj
			}
		} else {
			tm.year = tmfc.year
			if tmfc.bc != 0 {
				tm.year = -tm.year
			}
			if tm.year < 0 {
				tm.year++
			}
		}
		fmask |= mYear
	} else if tmfc.cc != 0 {
		// The first year of the century.
		if tmfc.bc != 0 {
			tmfc.cc = -tmfc.cc
		}
		var o1, o2 bool
		var y int32
		if tmfc.cc >= 0 {
			y, o1 = mulS32(tmfc.cc-1, 100)
		} else {
			y, o1 = mulS32(tmfc.cc, 100)
		}
		tm.year, o2 = addS32(y, 1)
		if o1 || o2 {
			err = fieldOverflow(input)
			return
		}
		fmask |= mYear
	}
	if tmfc.j != 0 {
		tm.year, tm.mon, tm.mday = pgJ2date(tmfc.j)
		fmask |= mYear | mMonth | mDay
	}
	if tmfc.ww != 0 {
		if tmfc.mode == fmtModeIsoweek {
			// No day of week leaves the date at the start (Monday) of the ISO week.
			if tmfc.d != 0 {
				jday := pgIsoweek2j(tm.year, tmfc.ww)
				if tmfc.d > 1 {
					jday += tmfc.d - 2
				} else {
					jday += 6
				}
				tm.year, tm.mon, tm.mday = pgJ2date(jday)
			} else {
				tm.year, tm.mon, tm.mday = pgJ2date(pgIsoweek2j(tm.year, tmfc.ww))
			}
			fmask |= mYear | mMonth | mDay
		} else {
			a, o1 := addS32(tmfc.ww, -1)
			b, o2 := mulS32(a, 7)
			c, o3 := addS32(b, 1)
			if o1 || o2 || o3 {
				err = fieldOverflow(input)
				return
			}
			tmfc.ddd = c
		}
	}
	if tmfc.w != 0 {
		a, o1 := addS32(tmfc.w, -1)
		b, o2 := mulS32(a, 7)
		c, o3 := addS32(b, 1)
		if o1 || o2 || o3 {
			err = fieldOverflow(input)
			return
		}
		tmfc.dd = c
	}
	if tmfc.dd != 0 {
		tm.mday = tmfc.dd
		fmask |= mDay
	}
	if tmfc.mm != 0 {
		tm.mon = tmfc.mm
		fmask |= mMonth
	}
	if tmfc.ddd != 0 && (tm.mon <= 1 || tm.mday <= 1) {
		// The month and day come from the day of the year, Gregorian or ISO.
		if tm.year == 0 && tmfc.bc == 0 {
			err = newError(InvalidDatetimeFormat, "cannot calculate day of year without year information")
			return
		}
		if tmfc.mode == fmtModeIsoweek {
			j0 := pgIsoweek2j(tm.year, 1) - 1
			tm.year, tm.mon, tm.mday = pgJ2date(j0 + tmfc.ddd)
			fmask |= mYear | mMonth | mDay
		} else {
			ysum := [2][13]int32{
				{0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365},
				{0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366},
			}
			leap := 0
			if pgIsLeap(tm.year) {
				leap = 1
			}
			y := ysum[leap]
			i := 1
			for ; i <= 12; i++ {
				if tmfc.ddd <= y[i] {
					break
				}
			}
			if tm.mon <= 1 {
				tm.mon = int32(i)
			}
			if tm.mday <= 1 {
				tm.mday = tmfc.ddd - y[i-1]
			}
			fmask |= mMonth | mDay
		}
	}
	if tmfc.ms != 0 {
		tmp, o1 := mulS32(tmfc.ms, 1000)
		f, o2 := addS32(fsec, tmp)
		if o1 || o2 {
			err = fieldOverflow(input)
			return
		}
		fsec = f
	}
	if tmfc.us != 0 {
		fsec += tmfc.us
	}
	fprec = tmfc.ff
	if fmask != 0 {
		// PostgreSQL's ValidateDate (the year was already adjusted for AD/BC).
		bad := (fmask&mMonth != 0 && (tm.mon < 1 || tm.mon > 12)) ||
			(fmask&mDay != 0 && (tm.mday < 1 || tm.mday > 31))
		if !bad && fmask == mYear|mMonth|mDay {
			leap := 0
			if pgIsLeap(tm.year) {
				leap = 1
			}
			bad = tm.mday > pgDayTab[leap][tm.mon-1]
		}
		if bad {
			err = fieldOverflow(input)
			return
		}
	}
	if tm.hour < 0 || tm.hour >= 24 || tm.min < 0 || tm.min >= 60 || tm.sec < 0 || tm.sec >= 60 ||
		fsec < 0 || fsec >= 1_000_000 {
		err = fieldOverflow(input)
		return
	}
	if tmfc.tzsign != 0 {
		if tmfc.tzh < 0 || tmfc.tzh > 15 || tmfc.tzm < 0 || tmfc.tzm >= 60 {
			err = newError(InvalidTimeZoneDisplacementValue, "time zone displacement out of range: \""+input+"\"")
			return
		}
		hasTz = true
		tzEast = (int64(tmfc.tzh)*60 + int64(tmfc.tzm)) * 60
		if tmfc.tzsign < 0 {
			tzEast = -tzEast
		}
	}
	return
}

// wallFromTm is the wall-clock microsecond reading of the broken-down fields, or false when it
// leaves the finite timestamp range.
func wallFromTm(tm pgTm, fsec int32) (int64, bool) {
	days := daysFromCivil(int64(tm.year), int64(tm.mon), int64(tm.mday))
	secs, ok := mulAdd(days, 86_400, int64(tm.hour)*3600+int64(tm.min)*60+int64(tm.sec))
	if !ok {
		return 0, false
	}
	t, ok := mulAdd(secs, 1_000_000, int64(fsec))
	if !ok || t == posInfinity || t == negInfinity {
		return 0, false
	}
	return t, true
}

// pgEpochShiftMicros is PostgreSQL's 2000-01-01 epoch, in jed's microseconds since 1970-01-01.
const pgEpochShiftMicros int64 = 946_684_800 * 1_000_000

// roundToPrecision is PostgreSQL's AdjustTimestampForTypmod for 0 < prec < 6: round half away from
// zero to prec fractional digits, the sign taken relative to PostgreSQL's 2000-01-01 epoch.
func roundToPrecision(t int64, prec int32) (int64, bool) {
	if prec <= 0 || prec >= 6 {
		return t, true
	}
	scale := int64(1)
	for k := prec; k < 6; k++ {
		scale *= 10
	}
	offset := scale / 2
	pg, ok := sub64(t, pgEpochShiftMicros)
	if !ok {
		return 0, false
	}
	if pg >= 0 {
		pg = (pg + offset) / scale * scale
	} else {
		pg = -((-pg + offset) / scale * scale)
	}
	r, ok := add64(pg, pgEpochShiftMicros)
	if !ok || r == posInfinity || r == negInfinity {
		return 0, false
	}
	return r, true
}

// toTimestampText is to_timestamp(text, text) → timestamptz: an explicit TZH / TZM / OF offset fixes
// the instant; otherwise the wall clock is read in zone (one timezone unit).
func toTimestampText(input, tmpl string, zone ZoneRef, m *costMeter) (int64, error) {
	tm, fsec, fprec, hasTz, tzEast, err := toTimestampParts(input, tmpl)
	if err != nil {
		return 0, err
	}
	outOfRange := newError(DatetimeFieldOverflow, "timestamp out of range")
	wall, ok := wallFromTm(tm, fsec)
	if !ok {
		return 0, outOfRange
	}
	var inst int64
	if hasTz {
		inst, ok = finiteShift(sub64(wall, tzEast*1_000_000))
	} else {
		m.Charge(costs.Timezone)
		if err := m.Guard(); err != nil {
			return 0, err
		}
		inst, ok = localToInstantMicros(zone, wall)
	}
	if !ok {
		return 0, outOfRange
	}
	if fprec != 0 {
		if inst, ok = roundToPrecision(inst, fprec); !ok {
			return 0, outOfRange
		}
	}
	return inst, nil
}

// toDateText is to_date(text, text) → date.
func toDateText(input, tmpl string) (int32, error) {
	tm, _, _, _, _, err := toTimestampParts(input, tmpl)
	if err != nil {
		return 0, err
	}
	days := daysFromCivil(int64(tm.year), int64(tm.mon), int64(tm.mday))
	if days <= int64(dateNegInfinity) || days >= int64(datePosInfinity) {
		return 0, newError(DatetimeFieldOverflow, "date out of range: \""+input+"\"")
	}
	return int32(days), nil
}

// toTimestampFloat is to_timestamp(double precision): Unix-epoch seconds, PostgreSQL's
// float8_timestamptz — computed in PostgreSQL's 2000 epoch with its float operations, so the
// rounding matches to the microsecond.
func toTimestampFloat(secs float64) (int64, error) {
	if math.IsNaN(secs) {
		return 0, newError(DatetimeFieldOverflow, "timestamp cannot be NaN")
	}
	if math.IsInf(secs, 1) {
		return posInfinity, nil
	}
	if math.IsInf(secs, -1) {
		return negInfinity, nil
	}
	tooFar := newError(DatetimeFieldOverflow, "timestamp out of range: \""+renderFloatPG(secs, 64)+"\"")
	// jed's range is the finite int64 microseconds, checked on the rounded count.
	us := math.RoundToEven((secs - 946_684_800) * 1_000_000)
	if us < -9_223_372_036_854_775_808.0 || us >= 9_223_372_036_854_775_808.0 {
		return 0, tooFar
	}
	r, ok := add64(int64(us), pgEpochShiftMicros)
	if !ok || r == posInfinity || r == negInfinity {
		return 0, tooFar
	}
	return r, nil
}
