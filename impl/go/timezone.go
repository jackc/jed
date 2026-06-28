package jed

// Time zones: the JTZ bundle codec + an RFC 8536 TZif reader + the engine-global loaded zone set.
// The cross-core contract for time-zone conversion (spec/design/timezones.md §4/§5): the reader is
// hand-written per core (CLAUDE.md §5) and byte-identical given identical input — it reads the
// standardized TZif layout, so cores agree by construction (§3.4). Byte formats: spec/tz/README.md.
// Mirrors impl/rust/src/timezone.rs and impl/ts/src/timezone.ts.

import (
	"fmt"
	"sort"
	"strings"
	"sync"
)

// secsPerDay and floorMod are defined in timestamp.go (the calendar core this reader reuses).

// --- in-memory zone representation (the parsed TZif file, spec/tz/README.md §2) ---

// LocalTimeType is one RFC 8536 local-time type: the offset east of UT, the DST flag, the abbrev.
type localTimeType struct {
	Utoff  int32
	IsDst  bool
	Abbrev string
}

// TzData is a parsed TZif file's tables (§2): the transition table + types + the optional POSIX footer.
type tzData struct {
	Trans     []int64
	TransType []byte
	Types     []localTimeType
	Footer    *posixTz
}

// Zone is a loaded named zone: its tables plus the bundle metadata it arrived with.
type zone struct {
	Name          string
	TzdataVersion string
	Data          tzData
}

// Offset is the local-time type in effect at an instant (the reader output, §4).
type offset struct {
	Utoff  int32
	Abbrev string
	IsDst  bool
}

// ZoneRef is a resolved zone reference: a built-in fixed offset (UTC / ±HH:MM, §3.2) or a loaded zone.
type ZoneRef struct {
	Fixed bool
	Off   int32 // valid when Fixed
	zone  *zone // valid when !Fixed
}

// TimeZoneInfo is an introspection row for db.LoadedTimeZones (timezones.md §3.3).
type timeZoneInfo struct {
	Name          string
	TzdataVersion string
}

// --- the POSIX TZ footer rule (spec/tz/README.md §5) ---

// PosixRule is a Mm.w.d DST transition rule (§5): month 1–12, week 1–5 (5 = last), day 0–6 (0 = Sun).
// The Jn / n julian forms are a deferred follow-on (timezones.md §14).
type posixRule struct {
	M, W, D uint8
}

// PosixDst is the DST half of a POSIX TZ string: abbrev/offset (east-positive) + start/end rules.
type posixDst struct {
	Abbr      string
	Utoff     int32
	Start     posixRule
	StartTime int32
	End       posixRule
	EndTime   int32
}

// PosixTz is a parsed POSIX TZ string (§5). Offsets are east-positive (negated from POSIX west).
type posixTz struct {
	StdAbbr  string
	StdUtoff int32
	Dst      *posixDst
}

// parsePosixTz parses a POSIX TZ string (§5): std offset[dst[offset][,start[/time],end[/time]]].
func parsePosixTz(s string) (*posixTz, error) {
	b := []byte(s)
	i := 0
	stdAbbr, ok := parsePosixAbbr(b, &i)
	if !ok {
		return nil, fmt.Errorf("posix: missing std abbreviation")
	}
	stdPosix, ok := parsePosixOffset(b, &i)
	if !ok {
		return nil, fmt.Errorf("posix: missing std offset")
	}
	stdUtoff := -stdPosix

	if i >= len(b) {
		return &posixTz{StdAbbr: stdAbbr, StdUtoff: stdUtoff}, nil
	}

	abbr, ok := parsePosixAbbr(b, &i)
	if !ok {
		return nil, fmt.Errorf("posix: malformed dst abbreviation")
	}
	var dstUtoff int32
	if i < len(b) && b[i] != ',' {
		p, ok := parsePosixOffset(b, &i)
		if !ok {
			return nil, fmt.Errorf("posix: malformed dst offset")
		}
		dstUtoff = -p
	} else {
		dstUtoff = stdUtoff + 3600
	}

	if i >= len(b) || b[i] != ',' {
		return nil, fmt.Errorf("posix: dst without transition rules")
	}
	i++
	start, startTime, err := parsePosixRule(b, &i)
	if err != nil {
		return nil, err
	}
	if i >= len(b) || b[i] != ',' {
		return nil, fmt.Errorf("posix: missing dst end rule")
	}
	i++
	end, endTime, err := parsePosixRule(b, &i)
	if err != nil {
		return nil, err
	}
	return &posixTz{
		StdAbbr:  stdAbbr,
		StdUtoff: stdUtoff,
		Dst: &posixDst{
			Abbr: abbr, Utoff: dstUtoff,
			Start: start, StartTime: startTime,
			End: end, EndTime: endTime,
		},
	}, nil
}

func parsePosixAbbr(b []byte, i *int) (string, bool) {
	if *i < len(b) && b[*i] == '<' {
		*i++
		start := *i
		for *i < len(b) && b[*i] != '>' {
			*i++
		}
		if *i >= len(b) {
			return "", false
		}
		s := string(b[start:*i])
		*i++ // consume '>'
		return s, s != ""
	}
	start := *i
	for *i < len(b) && isAsciiAlpha(b[*i]) {
		*i++
	}
	if *i == start {
		return "", false
	}
	return string(b[start:*i]), true
}

// parsePosixOffset parses [+|-]hh[:mm[:ss]] → seconds (POSIX raw, west-positive). Caller negates.
func parsePosixOffset(b []byte, i *int) (int32, bool) {
	neg := false
	if *i < len(b) && (b[*i] == '+' || b[*i] == '-') {
		neg = b[*i] == '-'
		*i++
	}
	hh, ok := parseUint(b, i)
	if !ok {
		return 0, false
	}
	secs := int32(hh) * 3600
	if *i < len(b) && b[*i] == ':' {
		*i++
		mm, ok := parseUint(b, i)
		if !ok {
			return 0, false
		}
		secs += int32(mm) * 60
		if *i < len(b) && b[*i] == ':' {
			*i++
			ss, ok := parseUint(b, i)
			if !ok {
				return 0, false
			}
			secs += int32(ss)
		}
	}
	if neg {
		return -secs, true
	}
	return secs, true
}

// parsePosixRule parses Mm.w.d with an optional /time (§5). Jn / n are a deferred error.
func parsePosixRule(b []byte, i *int) (posixRule, int32, error) {
	if *i >= len(b) {
		return posixRule{}, 0, fmt.Errorf("posix: missing transition rule")
	}
	if b[*i] != 'M' {
		return posixRule{}, 0, fmt.Errorf("posix: Jn/n julian-day transition rules are not yet supported")
	}
	*i++
	m, ok := parseUint(b, i)
	if !ok || *i >= len(b) || b[*i] != '.' {
		return posixRule{}, 0, fmt.Errorf("posix: bad month")
	}
	*i++
	w, ok := parseUint(b, i)
	if !ok || *i >= len(b) || b[*i] != '.' {
		return posixRule{}, 0, fmt.Errorf("posix: bad week")
	}
	*i++
	d, ok := parseUint(b, i)
	if !ok {
		return posixRule{}, 0, fmt.Errorf("posix: bad day")
	}
	if m < 1 || m > 12 || w < 1 || w > 5 || d > 6 {
		return posixRule{}, 0, fmt.Errorf("posix: Mm.w.d out of range")
	}
	time := int32(7200)
	if *i < len(b) && b[*i] == '/' {
		*i++
		t, ok := parsePosixOffset(b, i)
		if !ok {
			return posixRule{}, 0, fmt.Errorf("posix: bad transition time")
		}
		time = t
	}
	return posixRule{M: uint8(m), W: uint8(w), D: uint8(d)}, time, nil
}

func parseUint(b []byte, i *int) (uint32, bool) {
	start := *i
	var v uint32
	for *i < len(b) && b[*i] >= '0' && b[*i] <= '9' {
		v = v*10 + uint32(b[*i]-'0')
		*i++
	}
	return v, *i != start
}

func isAsciiAlpha(c byte) bool {
	return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
}

// --- the reader (spec/tz/README.md §4) ---

// mwdDay is the day-of-month of the w-th weekday d (0=Sun) of month m in year (w==5 = last).
func mwdDay(year int64, m, w, d uint8) int64 {
	mm := int64(m)
	first := daysFromCivil(year, mm, 1)
	firstDow := floorMod(first+4, 7) // 0 = Sunday (1970-01-01 was Thursday)
	offset := floorMod(int64(d)-firstDow, 7)
	day := 1 + offset + (int64(w)-1)*7
	var next int64
	if m == 12 {
		next = daysFromCivil(year+1, 1, 1)
	} else {
		next = daysFromCivil(year, mm+1, 1)
	}
	dim := next - first
	if day > dim {
		day -= 7
	}
	return day
}

// ruleInstant is the UT seconds of a footer transition: the rule at localTime in year, using the
// offset in effect just before the transition (utoffBefore).
func ruleInstant(rule posixRule, localTime int32, year int64, utoffBefore int32) int64 {
	day := mwdDay(year, rule.M, rule.W, rule.D)
	localEpoch := daysFromCivil(year, int64(rule.M), day)*secsPerDay + int64(localTime)
	return localEpoch - int64(utoffBefore)
}

// evalPosix evaluates the POSIX footer (§5) at instantSecs.
func evalPosix(tz *posixTz, instantSecs int64) offset {
	if tz.Dst == nil {
		return offset{Utoff: tz.StdUtoff, Abbrev: tz.StdAbbr, IsDst: false}
	}
	d := tz.Dst
	year, _, _ := civilFromDays(floorDiv(instantSecs, secsPerDay))
	startUt := ruleInstant(d.Start, d.StartTime, year, tz.StdUtoff)
	endUt := ruleInstant(d.End, d.EndTime, year, d.Utoff)
	var inDst bool
	if startUt < endUt {
		inDst = instantSecs >= startUt && instantSecs < endUt
	} else {
		// Southern hemisphere: the DST interval wraps the year boundary.
		inDst = instantSecs >= startUt || instantSecs < endUt
	}
	if inDst {
		return offset{Utoff: d.Utoff, Abbrev: d.Abbr, IsDst: true}
	}
	return offset{Utoff: tz.StdUtoff, Abbrev: tz.StdAbbr, IsDst: false}
}

func firstStdOffset(data *tzData) offset {
	for _, t := range data.Types {
		if !t.IsDst {
			return offset{Utoff: t.Utoff, Abbrev: t.Abbrev, IsDst: false}
		}
	}
	if len(data.Types) > 0 {
		t := data.Types[0]
		return offset{Utoff: t.Utoff, Abbrev: t.Abbrev, IsDst: t.IsDst}
	}
	return offset{}
}

// OffsetAt is the reader (§4): the local-time type in effect at instantSecs (UT seconds). Pure/total.
func offsetAt(data *tzData, instantSecs int64) offset {
	n := len(data.Trans)
	if n == 0 {
		if data.Footer != nil {
			return evalPosix(data.Footer, instantSecs)
		}
		return firstStdOffset(data)
	}
	if instantSecs < data.Trans[0] {
		return firstStdOffset(data)
	}
	if data.Footer != nil && instantSecs >= data.Trans[n-1] {
		return evalPosix(data.Footer, instantSecs)
	}
	// largest i with Trans[i] <= instantSecs
	i := sort.Search(n, func(k int) bool { return data.Trans[k] > instantSecs }) - 1
	t := data.Types[data.TransType[i]]
	return offset{Utoff: t.Utoff, Abbrev: t.Abbrev, IsDst: t.IsDst}
}

// --- TZif parsing (RFC 8536 / spec/tz/README.md §2) ---

type tzCounts struct {
	isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt int
}

func readTzHeader(r *reader) (byte, tzCounts, error) {
	magic, err := r.take(4)
	if err != nil {
		return 0, tzCounts{}, err
	}
	if string(magic) != "TZif" {
		return 0, tzCounts{}, corruptErr("tzif: bad magic")
	}
	version, err := r.u8()
	if err != nil {
		return 0, tzCounts{}, err
	}
	if err := r.skip(15); err != nil {
		return 0, tzCounts{}, err
	}
	var c tzCounts
	for _, p := range []*int{&c.isutcnt, &c.isstdcnt, &c.leapcnt, &c.timecnt, &c.typecnt, &c.charcnt} {
		v, err := r.u32()
		if err != nil {
			return 0, tzCounts{}, err
		}
		*p = int(v)
	}
	return version, c, nil
}

func tzBlockSize(c tzCounts, timeSize int) int {
	return c.timecnt*timeSize + c.timecnt + c.typecnt*6 + c.charcnt + c.leapcnt*(timeSize+4) + c.isstdcnt + c.isutcnt
}

func abbrevAt(desig []byte, idx byte) (string, error) {
	start := int(idx)
	if start > len(desig) {
		return "", corruptErr("tzif: designation index out of range")
	}
	end := start
	for end < len(desig) && desig[end] != 0 {
		end++
	}
	return string(desig[start:end]), nil
}

func readTzBlock(r *reader, timeSize int, c tzCounts) (tzData, error) {
	trans := make([]int64, c.timecnt)
	for i := range trans {
		if timeSize == 8 {
			v, err := r.i64()
			if err != nil {
				return tzData{}, err
			}
			trans[i] = v
		} else {
			v, err := r.i32()
			if err != nil {
				return tzData{}, err
			}
			trans[i] = int64(v)
		}
	}
	transType := make([]byte, c.timecnt)
	for i := range transType {
		v, err := r.u8()
		if err != nil {
			return tzData{}, err
		}
		transType[i] = v
	}
	type rawType struct {
		utoff int32
		isDst bool
		idx   byte
	}
	raws := make([]rawType, c.typecnt)
	for i := range raws {
		off, err := r.i32()
		if err != nil {
			return tzData{}, err
		}
		dst, err := r.u8()
		if err != nil {
			return tzData{}, err
		}
		idx, err := r.u8()
		if err != nil {
			return tzData{}, err
		}
		raws[i] = rawType{off, dst != 0, idx}
	}
	desig, err := r.take(c.charcnt)
	if err != nil {
		return tzData{}, err
	}
	desigCopy := append([]byte(nil), desig...)
	// leap seconds (occ width = timeSize, corr = 4) + std/wall + ut/local — skipped (§2).
	if err := r.skip(c.leapcnt * (timeSize + 4)); err != nil {
		return tzData{}, err
	}
	if err := r.skip(c.isstdcnt); err != nil {
		return tzData{}, err
	}
	if err := r.skip(c.isutcnt); err != nil {
		return tzData{}, err
	}
	types := make([]localTimeType, c.typecnt)
	for i, raw := range raws {
		ab, err := abbrevAt(desigCopy, raw.idx)
		if err != nil {
			return tzData{}, err
		}
		types[i] = localTimeType{Utoff: raw.utoff, IsDst: raw.isDst, Abbrev: ab}
	}
	if len(types) == 0 {
		return tzData{}, corruptErr("tzif: no local time types")
	}
	for _, t := range transType {
		if int(t) >= len(types) {
			return tzData{}, corruptErr("tzif: transition type index out of range")
		}
	}
	return tzData{Trans: trans, TransType: transType, Types: types}, nil
}

// ParseTzif parses a TZif file (§2): a v1 block is read directly; a v2+ file skips the v1 block,
// reads the 64-bit block, then the POSIX footer. A malformed file is XX001.
func parseTzif(data []byte) (tzData, error) {
	r := &reader{b: data}
	version, c1, err := readTzHeader(r)
	if err != nil {
		return tzData{}, err
	}
	if version == 0 {
		return readTzBlock(r, 4, c1)
	}
	if err := r.skip(tzBlockSize(c1, 4)); err != nil {
		return tzData{}, err
	}
	_, c2, err := readTzHeader(r)
	if err != nil {
		return tzData{}, err
	}
	td, err := readTzBlock(r, 8, c2)
	if err != nil {
		return tzData{}, err
	}
	footer, err := readTzFooter(r)
	if err != nil {
		return tzData{}, err
	}
	td.Footer = footer
	return td, nil
}

func readTzFooter(r *reader) (*posixTz, error) {
	rest := r.b[min(r.i, len(r.b)):]
	s := strings.Trim(string(rest), "\n")
	if s == "" {
		return nil, nil
	}
	tz, err := parsePosixTz(s)
	if err != nil {
		return nil, corruptErr("%s", err.Error())
	}
	return tz, nil
}

// --- the JTZ bundle codec (spec/tz/README.md §3) ---

// TzBundle is a parsed JTZ bundle (README §3).
type tzBundle struct {
	TzdataVersion string
	Description   string
	Zones         []tzZoneSection // (name, raw TZif bytes), ascending by name
	Links         []tzLink        // (alias, target), ascending by alias
}

type tzZoneSection struct {
	Name string
	Raw  []byte
}

type tzLink struct {
	Alias  string
	Target string
}

var tzBundleMagic = []byte("JTZ\x00\x00\x00")

// SaveTzBundle serializes a JTZ bundle (README §3).
func saveTzBundle(b *tzBundle) []byte {
	type packed struct {
		name   string
		hash   uint32
		rawLen uint32
		comp   []byte
	}
	ps := make([]packed, len(b.Zones))
	for i, z := range b.Zones {
		ps[i] = packed{z.Name, crc32IEEE(z.Raw), uint32(len(z.Raw)), lz4Compress(z.Raw)}
	}

	var header []byte
	header = append(header, tzBundleMagic...)
	header = appendU16(header, 1) // format_version
	header = pushStr(header, b.TzdataVersion)
	header = pushStr(header, b.Description)
	header = appendU16(header, uint16(len(ps)))
	header = appendU16(header, uint16(len(b.Links)))

	zoneManifestLen := 0
	for _, p := range ps {
		zoneManifestLen += 2 + len(p.name) + 16
	}
	linkLen := 0
	for _, l := range b.Links {
		linkLen += 2 + len(l.Alias) + 2 + len(l.Target)
	}
	bodyStart := len(header) + zoneManifestLen + linkLen

	var manifest []byte
	off := bodyStart
	for _, p := range ps {
		manifest = pushStr(manifest, p.name)
		manifest = appendU32(manifest, p.hash)
		manifest = appendU32(manifest, p.rawLen)
		manifest = appendU32(manifest, uint32(len(p.comp)))
		manifest = appendU32(manifest, uint32(off))
		off += len(p.comp)
	}
	for _, l := range b.Links {
		manifest = pushStr(manifest, l.Alias)
		manifest = pushStr(manifest, l.Target)
	}

	out := append(header, manifest...)
	for _, p := range ps {
		out = append(out, p.comp...)
	}
	out = appendU32(out, crc32IEEE(out))
	return out
}

// OpenTzBundle reads a JTZ bundle (README §3), verifying the CRC, magic, format, and each zone's hash.
func openTzBundle(data []byte) (*tzBundle, error) {
	if len(data) < 4 {
		return nil, corruptErr("tz bundle: truncated")
	}
	body := data[:len(data)-4]
	want := uint32(data[len(data)-4])<<24 | uint32(data[len(data)-3])<<16 | uint32(data[len(data)-2])<<8 | uint32(data[len(data)-1])
	if crc32IEEE(body) != want {
		return nil, corruptErr("tz bundle: trailer checksum mismatch")
	}
	r := &reader{b: data}
	magic, err := r.take(6)
	if err != nil {
		return nil, err
	}
	if string(magic) != "JTZ\x00\x00\x00" {
		return nil, corruptErr("tz bundle: bad magic")
	}
	fmtVer, err := r.u16()
	if err != nil {
		return nil, err
	}
	if fmtVer != 1 {
		return nil, corruptErr("tz bundle: unsupported format_version %d", fmtVer)
	}
	version, err := r.str()
	if err != nil {
		return nil, err
	}
	desc, err := r.str()
	if err != nil {
		return nil, err
	}
	zoneCount, err := r.u16()
	if err != nil {
		return nil, err
	}
	linkCount, err := r.u16()
	if err != nil {
		return nil, err
	}
	type meta struct {
		name                  string
		hash                  uint32
		rawLen, compLen, offs int
	}
	metas := make([]meta, zoneCount)
	for i := range metas {
		name, err := r.str()
		if err != nil {
			return nil, err
		}
		h, err := r.u32()
		if err != nil {
			return nil, err
		}
		rl, err := r.u32()
		if err != nil {
			return nil, err
		}
		cl, err := r.u32()
		if err != nil {
			return nil, err
		}
		of, err := r.u32()
		if err != nil {
			return nil, err
		}
		metas[i] = meta{name, h, int(rl), int(cl), int(of)}
	}
	links := make([]tzLink, linkCount)
	for i := range links {
		alias, err := r.str()
		if err != nil {
			return nil, err
		}
		target, err := r.str()
		if err != nil {
			return nil, err
		}
		links[i] = tzLink{alias, target}
	}
	zones := make([]tzZoneSection, zoneCount)
	for i, m := range metas {
		if m.offs > len(body) || m.offs+m.compLen > len(body) {
			return nil, corruptErr("tz bundle: section body out of range")
		}
		raw, err := lz4Decompress(data[m.offs:m.offs+m.compLen], m.rawLen)
		if err != nil {
			return nil, err
		}
		if crc32IEEE(raw) != m.hash {
			return nil, corruptErr("tz bundle: section content hash mismatch")
		}
		zones[i] = tzZoneSection{Name: m.name, Raw: raw}
	}
	return &tzBundle{TzdataVersion: version, Description: desc, Zones: zones, Links: links}, nil
}

// --- the engine-global loaded zone set + the load seam (timezones.md §3.3) ---

var (
	loadedTzMu sync.RWMutex
	loadedTz   = map[string]*zone{}
)

// LoadTimeZoneData loads a JTZ bundle into the engine-global loaded set (§3.3/§4): parse each zone's
// TZif, register by name, then resolve each link alias onto its target's tables. ADDITIVE /
// FIRST-WINS — a name already present is not replaced (idempotent re-load). A malformed bundle (or
// TZif) is XX001. The engine primitive behind db.LoadTimeZoneData; may be called before opening any
// file, reads no path, reaches no host data (§10).
func LoadTimeZoneData(data []byte) error {
	bundle, err := openTzBundle(data)
	if err != nil {
		return err
	}
	parsed := make(map[string]*zone, len(bundle.Zones))
	for _, z := range bundle.Zones {
		td, err := parseTzif(z.Raw)
		if err != nil {
			return err
		}
		parsed[z.Name] = &zone{Name: z.Name, TzdataVersion: bundle.TzdataVersion, Data: td}
	}
	loadedTzMu.Lock()
	defer loadedTzMu.Unlock()
	for name, z := range parsed {
		if _, ok := loadedTz[name]; !ok {
			loadedTz[name] = z
		}
	}
	for _, l := range bundle.Links {
		if z, ok := parsed[l.Target]; ok {
			if _, exists := loadedTz[l.Alias]; !exists {
				loadedTz[l.Alias] = &zone{Name: l.Alias, TzdataVersion: bundle.TzdataVersion, Data: z.Data}
			}
		}
	}
	return nil
}

// LoadedZone looks up a loaded named zone by exact name (nil ⇒ no loaded bundle provides it).
func loadedZone(name string) *zone {
	loadedTzMu.RLock()
	defer loadedTzMu.RUnlock()
	return loadedTz[name]
}

// LoadedTimeZones introspects the engine-global loaded zone set (db.LoadedTimeZones, §3.3): every
// zone + alias a loaded bundle provides, ascending by name.
func loadedTimeZones() []timeZoneInfo {
	loadedTzMu.RLock()
	defer loadedTzMu.RUnlock()
	names := make([]string, 0, len(loadedTz))
	for n := range loadedTz {
		names = append(names, n)
	}
	sort.Strings(names)
	out := make([]timeZoneInfo, len(names))
	for i, n := range names {
		out[i] = timeZoneInfo{Name: n, TzdataVersion: loadedTz[n].TzdataVersion}
	}
	return out
}

// ResolveZone resolves a zone name to a built-in fixed offset (UTC / ±HH[:MM[:SS]], §3.2) or a
// loaded named zone. Returns (ZoneRef, true) or (_, false) ⇒ unknown (the caller raises 22023).
func ResolveZone(name string) (ZoneRef, bool) {
	if name == "UTC" {
		return ZoneRef{Fixed: true, Off: 0}, true
	}
	if off, ok := parseFixedOffset(name); ok {
		return ZoneRef{Fixed: true, Off: off}, true
	}
	if z := loadedZone(name); z != nil {
		return ZoneRef{zone: z}, true
	}
	return ZoneRef{}, false
}

// OffsetAtRef is the offset in effect at instantSecs for a resolved zone reference.
func offsetAtRef(zr ZoneRef, instantSecs int64) offset {
	if zr.Fixed {
		return offset{Utoff: zr.Off, Abbrev: fixedAbbrev(zr.Off), IsDst: false}
	}
	return offsetAt(&zr.zone.Data, instantSecs)
}

// InstantToLocalMicros is timestamptz AT TIME ZONE zone (§4): local = instant + utoff.
func instantToLocalMicros(zr ZoneRef, instantMicros int64) int64 {
	off := offsetAtRef(zr, floorDiv(instantMicros, 1_000_000))
	return instantMicros + int64(off.Utoff)*1_000_000
}

// LocalToInstantMicros is timestamp AT TIME ZONE zone (§4): instant = wall − utoff. The offset is
// chosen by determineLocalOffset, matching PostgreSQL's DetermineTimeZoneOffset at a DST gap/overlap
// (oracle-pinned, timezones.md §6).
func localToInstantMicros(zr ZoneRef, wallMicros int64) int64 {
	wallSecs := floorDiv(wallMicros, 1_000_000)
	chosen := determineLocalOffset(zr, wallSecs)
	return wallMicros - chosen*1_000_000
}

// determineLocalOffset chooses the UT offset (seconds) to interpret a wall-clock wallSecs reading
// with, matching PostgreSQL's DetermineTimeZoneOffset (src/timezone/pgtz.c) at a DST boundary. For a
// normal time both candidate offsets agree; for a spring-forward GAP (a nonexistent wall clock) PG
// uses the *before* (earlier) offset; for a fall-back OVERLAP (a doubled wall clock) PG uses the
// *after* (later) offset. A fixed-offset zone has no boundary, so the single offset is returned.
func determineLocalOffset(zr ZoneRef, wallSecs int64) int64 {
	const day int64 = 86_400
	// The offsets a day before / after wallSecs (taken as if UTC). A DST transition is never less
	// than a day apart, so at most one boundary lies in this 2-day window; if both ends agree there
	// is no boundary near wallSecs and the time is unambiguous.
	offLo := int64(offsetAtRef(zr, wallSecs-day).Utoff)
	offHi := int64(offsetAtRef(zr, wallSecs+day).Utoff)
	if offLo == offHi {
		return offLo
	}
	// Binary-search the boundary: the smallest instant in (wall-day, wall+day] whose offset is no
	// longer offLo (i.e. has become offHi).
	lo, hi := wallSecs-day, wallSecs+day
	for lo < hi {
		mid := lo + (hi-lo)/2
		if int64(offsetAtRef(zr, mid).Utoff) == offLo {
			lo = mid + 1
		} else {
			hi = mid
		}
	}
	boundary := lo
	beforeTime := wallSecs - offLo
	afterTime := wallSecs - offHi
	beforeSide := beforeTime < boundary
	afterSide := afterTime < boundary
	switch {
	case beforeSide == afterSide:
		// Both candidate instants fall on the same side of the boundary — an ordinary time.
		if beforeSide {
			return offLo
		}
		return offHi
	case beforeTime > afterTime:
		return offLo // gap: the before (earlier) offset
	default:
		return offHi // overlap: the after (later) offset
	}
}

// parseFixedOffset parses [+|-]HH[:MM[:SS]] (the WHOLE string). Requires a leading sign. POSIX sign
// (positive = WEST), matching PG's AT TIME ZONE '+05:30' (= UTC−5:30 — oracle-pinned), so the
// east-positive utoff is the negation of the written value.
func parseFixedOffset(name string) (int32, bool) {
	b := []byte(name)
	if len(b) == 0 || (b[0] != '+' && b[0] != '-') {
		return 0, false
	}
	i := 0
	posix, ok := parsePosixOffset(b, &i)
	if !ok || i != len(b) {
		return 0, false
	}
	return -posix, true
}

func fixedAbbrev(utoff int32) string {
	if utoff == 0 {
		return "UTC"
	}
	sign := "+"
	a := utoff
	if utoff < 0 {
		sign = "-"
		a = -utoff
	}
	h, m, s := a/3600, (a%3600)/60, a%60
	if s == 0 {
		return fmt.Sprintf("%s%02d:%02d", sign, h, m)
	}
	return fmt.Sprintf("%s%02d:%02d:%02d", sign, h, m, s)
}

// --- reader extensions for TZif (signed big-endian + skip) ---

func (r *reader) i32() (int32, error) {
	v, err := r.u32()
	return int32(v), err
}

func (r *reader) i64() (int64, error) {
	s, err := r.take(8)
	if err != nil {
		return 0, err
	}
	return int64(uint64(s[0])<<56 | uint64(s[1])<<48 | uint64(s[2])<<40 | uint64(s[3])<<32 |
		uint64(s[4])<<24 | uint64(s[5])<<16 | uint64(s[6])<<8 | uint64(s[7])), nil
}

func (r *reader) skip(n int) error {
	_, err := r.take(n)
	return err
}
