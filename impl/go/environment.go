package jed

import "sort"

// The Environment (spec/design/environment.md) — the one explicit home for every input the engine
// takes from outside itself except storage: the loaded Unicode data (collations + the property/casing
// table), the loaded time-zone data, the host functions, and the random + clock sources. The
// database handle and its sessions keep policy and resource settings (max_cost, work_mem, limits).
//
// An EnvironmentBuilder does all loading, registration, and source choice; Build returns an
// Environment with no mutating methods, so a database's outside inputs never change while it is
// open. One Environment per handle: the attachments, the session-local temp domains, and every
// session of that handle use it. Passing nil (or omitting it) at open/create gives a FRESH empty
// environment — there is no process-wide default, so libraries and tests in one process stay
// isolated (environment.md §7).

// Environment is the immutable set of inputs a database takes from the host rather than from its
// file: loaded Unicode data (collations and case mapping), loaded time-zone data, host functions,
// and the random and clock sources. Build one with an [EnvironmentBuilder] (or [NewEnvironment]
// for an empty one) and pass it in [OpenOptions], [CreateOptions] or [DeserializeOptions].
// Opening without one gives the database a fresh, empty Environment; there is no process-wide
// default.
//
// An Environment never changes after it is built, so it is safe to share across goroutines and
// databases. The random and clock sources it holds are shared objects the host may still control
// (reseed a [SeededRandom], advance a [ManualClock]).
type Environment struct {
	// collations is the loaded collation set keyed by exact (case-sensitive) name. "C" is never here
	// (table-free, built in).
	collations map[string]*compiledCollation
	// property is the Unicode property/casing table (collation.md §16), nil ⇒ the ASCII-casing
	// baseline (casing is table-free, version-free).
	property *propertyTable
	// zones is the loaded zone set (and link aliases) keyed by exact name. UTC and fixed offsets are
	// built in and never here.
	zones map[string]*zone
	// zonesFolded indexes zones by ASCII-folded name for the case-insensitive lookup (timezones.md
	// §3.2); built once by Build.
	zonesFolded map[string]*zone
	// hosts is the frozen host function registry (extensibility.md §7). Never nil.
	hosts  *hostRegistry
	random RandomSource
	clock  ClockSource
	// keySeed, when set, makes encrypted databases draw their key material and nonces from an
	// INSECURE seeded stream instead of the OS CSPRNG (spec/fileformat/encrypted.md §8).
	keySeed *uint64
}

// EnvironmentBuilder collects the contents of an [Environment]. Each load and registration is
// validated when it is made. Loads are first-wins: a collation, case-mapping table or time zone
// already present is not replaced, so loading the same bundle twice has no further effect.
// [EnvironmentBuilder.Build] takes a snapshot, so later calls on the builder never change an
// Environment already built.
//
// An EnvironmentBuilder is not safe for concurrent use.
type EnvironmentBuilder struct {
	collations map[string]*compiledCollation
	property   *propertyTable
	zones      map[string]*zone
	functions  []hostFuncEntry
	random     RandomSource
	clock      ClockSource
	keySeed    *uint64
}

// NewEnvironmentBuilder returns an empty builder. An Environment built from it has only the
// built-in data: the C collation and ASCII-only case mapping, the UTC and fixed-offset time
// zones, no host functions, the operating system's random source ([OSRandomSource]) and the wall
// clock ([WallClock]).
func NewEnvironmentBuilder() *EnvironmentBuilder {
	return &EnvironmentBuilder{
		collations: map[string]*compiledCollation{},
		zones:      map[string]*zone{},
		random:     OSRandomSource(),
		clock:      WallClock(),
	}
}

// NewEnvironment returns an empty Environment, the same as NewEnvironmentBuilder().Build(). It is
// what a database uses when none is given.
func NewEnvironment() *Environment { return NewEnvironmentBuilder().Build() }

// LoadUnicodeData loads a jed Unicode-data bundle (JUCD) from data: it adds every collation the
// bundle provides by name, and its Unicode case-mapping and character-property table. Names
// already loaded are kept (first wins). A malformed bundle fails with SQLSTATE XX001 and leaves
// the builder unchanged. The engine never reads files itself; the host supplies the bytes.
func (b *EnvironmentBuilder) LoadUnicodeData(data []byte) error {
	bundle, err := openBundle(data)
	if err != nil {
		return err
	}
	colls, prop, err := loadBundle(bundle)
	if err != nil {
		return err
	}
	for _, c := range colls {
		if _, ok := b.collations[c.Name]; !ok {
			b.collations[c.Name] = c
		}
	}
	if prop != nil && b.property == nil {
		b.property = prop
	}
	return nil
}

// LoadTimeZoneData loads a jed time-zone bundle (JTZ) from data, adding each zone and each alias
// (link) the bundle provides by name. Names already loaded are kept (first wins). A malformed
// bundle fails with SQLSTATE XX001 and leaves the builder unchanged. UTC and fixed offsets are
// built in and need no data.
func (b *EnvironmentBuilder) LoadTimeZoneData(data []byte) error {
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
		parsed[z.Name] = &zone{Name: z.Name, TzdataVersion: bundle.TzdataVersion, Data: td, Checksum: crc32IEEE(z.Raw)}
	}
	for name, z := range parsed {
		if _, ok := b.zones[name]; !ok {
			b.zones[name] = z
		}
	}
	for _, l := range bundle.Links {
		if z, ok := parsed[l.Target]; ok {
			if _, exists := b.zones[l.Alias]; !exists {
				b.zones[l.Alias] = &zone{Name: l.Alias, TzdataVersion: bundle.TzdataVersion, Data: z.Data, Checksum: z.Checksum}
			}
		}
	}
	return nil
}

// RegisterFunction adds the host scalar function f, callable from SQL by its name. A name may be
// overloaded across argument types. It fails with SQLSTATE 22023 for a negative cost, 42704 for
// an unknown argument or result type name, and 42723 when a function with the same name and
// argument types is already registered. A function with the same signature as a built-in is
// accepted but never called: the built-in wins.
func (b *EnvironmentBuilder) RegisterFunction(f *HostFunction) error {
	entry, err := resolveHostFunction(f)
	if err != nil {
		return err
	}
	for i := range b.functions {
		if b.functions[i].name == entry.name && sameHostSig(b.functions[i].argTypes, entry.argTypes) {
			return newError(DuplicateFunction, "host function "+f.name+" already registered with this signature")
		}
	}
	b.functions = append(b.functions, entry)
	return nil
}

// SetRandomSource sets the source of random bytes for the uuid generators (gen_random_uuid,
// uuidv4, uuidv7). nil restores the default, [OSRandomSource]. Pass a [SeededRandom] for a
// reproducible sequence.
func (b *EnvironmentBuilder) SetRandomSource(s RandomSource) {
	if s == nil {
		s = OSRandomSource()
	}
	b.random = s
}

// SetClockSource sets the clock that SQL functions such as now() and uuidv7 read. nil restores
// the default, [WallClock]. Pass a [ManualClock] or [AdvancingClock] for a clock the host
// controls.
func (b *EnvironmentBuilder) SetClockSource(c ClockSource) {
	if c == nil {
		c = WallClock()
	}
	b.clock = c
}

// SetInsecureSeededKeyEntropy makes encrypted databases draw their key material and nonces from a
// deterministic stream seeded with seed instead of the operating system's secure random source.
// Each database opened or created with the Environment starts its own stream from seed, so the
// same operations write identical encrypted files. It is independent of [SetRandomSource]. Use it
// only for tests and fixtures: the keys it produces are predictable and offer no security.
func (b *EnvironmentBuilder) SetInsecureSeededKeyEntropy(seed uint64) {
	b.keySeed = &seed
}

// Build returns an Environment holding the builder's current contents. Later calls on the
// builder do not affect the returned Environment, and Build may be called again.
func (b *EnvironmentBuilder) Build() *Environment {
	collations := make(map[string]*compiledCollation, len(b.collations))
	for k, v := range b.collations {
		collations[k] = v
	}
	zones := make(map[string]*zone, len(b.zones))
	for k, v := range b.zones {
		zones[k] = v
	}
	// The ASCII-case-folded index for case-insensitive lookup (resolveZone): when two loaded names
	// fold to one key the byte-smaller canonical name wins, so the choice is deterministic.
	names := make([]string, 0, len(zones))
	for k := range zones {
		names = append(names, k)
	}
	sort.Strings(names)
	zonesFolded := make(map[string]*zone, len(zones))
	for _, k := range names {
		f := foldZoneName(k)
		if _, taken := zonesFolded[f]; !taken {
			zonesFolded[f] = zones[k]
		}
	}
	functions := make([]hostFuncEntry, len(b.functions))
	copy(functions, b.functions)
	return &Environment{
		collations:  collations,
		property:    b.property,
		zones:       zones,
		zonesFolded: zonesFolded,
		hosts:       &hostRegistry{functions: functions},
		random:      b.random,
		clock:       b.clock,
		keySeed:     b.keySeed,
	}
}

// ToBuilder returns a builder pre-filled with e's contents, including the same random and clock
// source objects. Use it to derive a new Environment with more data or functions or different
// sources; e itself is unaffected.
func (e *Environment) ToBuilder() *EnvironmentBuilder {
	b := &EnvironmentBuilder{
		collations: make(map[string]*compiledCollation, len(e.collations)),
		property:   e.property,
		zones:      make(map[string]*zone, len(e.zones)),
		functions:  make([]hostFuncEntry, len(e.hosts.functions)),
		random:     e.random,
		clock:      e.clock,
		keySeed:    e.keySeed,
	}
	for k, v := range e.collations {
		b.collations[k] = v
	}
	for k, v := range e.zones {
		b.zones[k] = v
	}
	copy(b.functions, e.hosts.functions)
	return b
}

// LoadedCollations returns every collation the loaded Unicode data provides, sorted by name. The
// built-in C collation is not listed. IsDefault is always false and Verdict is always
// CollationFull, since the default collation and version skew are properties of a database.
func (e *Environment) LoadedCollations() []CollationInfo {
	colls := e.collationTables()
	out := make([]CollationInfo, len(colls))
	for i, c := range colls {
		out[i] = CollationInfo{
			Name:           c.Name,
			UnicodeVersion: c.UnicodeVersion,
			CLDRVersion:    c.CldrVersion,
			ContentHash:    crc32IEEE(serializeTable(c)),
			Description:    c.Description,
			IsDefault:      false,
			// The loaded set IS the version reference — it can never be skewed against itself.
			Verdict: CollationFull,
		}
	}
	return out
}

// LoadedTimeZones returns every time zone and alias the loaded time-zone data provides, sorted by
// name. The built-in UTC and fixed offsets are not listed.
func (e *Environment) LoadedTimeZones() []TimeZoneInfo {
	names := make([]string, 0, len(e.zones))
	for n := range e.zones {
		names = append(names, n)
	}
	sort.Strings(names)
	out := make([]TimeZoneInfo, len(names))
	for i, n := range names {
		out[i] = TimeZoneInfo{Name: n, TzdataVersion: e.zones[n].TzdataVersion}
	}
	return out
}

// collation looks up a loaded collation by exact name; nil ⇒ no loaded bundle provides it.
func (e *Environment) collation(name string) *compiledCollation { return e.collations[name] }

// collationTables returns every loaded collation, ascending by name.
func (e *Environment) collationTables() []*compiledCollation {
	out := make([]*compiledCollation, 0, len(e.collations))
	for _, c := range e.collations {
		out = append(out, c)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
}

// propertyTable returns the loaded property/casing table, or nil (the ASCII-casing baseline). The
// casing functions look it up once per evaluation and pass it to the pure kernels.
func (e *Environment) propertyTable() *propertyTable { return e.property }

// versionSkew is the slice-2d version-skew verdict (spec/design/collation.md §12, compatibility.md
// §7): given a collation name and the (unicode, cldr) version the FILE pinned its keys under (§5), it
// returns (loadedUnicode, loadedCldr, true) when the loaded bundle provides name at a DIFFERENT
// version — the object using it is read-only (XX002 on write) — else ("", "", false) (Full: the same
// version, or no loaded table to disagree). A pure, total comparison so every core computes the
// identical verdict. The read side never consults this — a skewed read recomputes against the loaded
// table (the heap-scan fallback, compatibility.md §8).
func (e *Environment) versionSkew(name, fileUnicode, fileCldr string) (loadedUnicode, loadedCldr string, skewed bool) {
	loaded := e.collation(name)
	if loaded == nil {
		return "", "", false
	}
	if loaded.UnicodeVersion != fileUnicode || loaded.CldrVersion != fileCldr {
		return loaded.UnicodeVersion, loaded.CldrVersion, true
	}
	return "", "", false
}

// resolveZone resolves a zone name to a built-in fixed offset (UTC / ±HH[:MM[:SS]], §3.2) or a loaded
// named zone. false ⇒ unknown (the caller raises 22023).
func (e *Environment) resolveZone(name string) (zoneRef, bool) {
	if zr, ok := builtinZone(name); ok {
		return zr, true
	}
	if z := e.zones[name]; z != nil {
		return zoneRef{zone: z}, true
	}
	// Case-insensitive like PostgreSQL's pg_tzset (timezones.md §3.2): an exact name wins, else the
	// ASCII-folded name. The resolved zone keeps its canonical name (the one an index dependency
	// records).
	if z := e.zonesFolded[foldZoneName(name)]; z != nil {
		return zoneRef{zone: z}, true
	}
	return zoneRef{}, false
}

// zoneDeps is every loaded zone as an index dependency (index-dependencies.md), ascending by name.
func (e *Environment) zoneDeps() []timeZoneDep {
	out := make([]timeZoneDep, 0, len(e.zones))
	for _, z := range e.zones {
		out = append(out, timeZoneDep{Name: z.Name, Version: z.TzdataVersion, Checksum: z.Checksum})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
}

// fill writes len(buf) random bytes from the installed random source. A source error that is not
// already an engine error surfaces as 58030 (the OS-entropy-unavailable class).
func (e *Environment) fill(buf []byte) error {
	if err := e.random.Fill(buf); err != nil {
		if _, ok := err.(*EngineError); ok {
			return err
		}
		return newError(IOError, "random source failed: "+err.Error())
	}
	return nil
}

// nowMicros reads the installed clock: micros since the Unix epoch.
func (e *Environment) nowMicros() int64 { return e.clock.NowMicros() }

// keyStream is a fresh key-entropy stream for one encrypted database (encrypted.md §8): the OS
// CSPRNG, or the insecure seeded stream SetInsecureSeededKeyEntropy installed.
func (e *Environment) keyStream() RandomSource {
	if e.keySeed != nil {
		return NewSeededRandom(*e.keySeed)
	}
	return OSRandomSource()
}
