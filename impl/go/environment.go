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

// Environment is an immutable set of outside inputs (environment.md §3). Build one with
// NewEnvironmentBuilder (or NewEnvironment for the empty default) and pass it in OpenOptions /
// CreateOptions. It is safe to share across goroutines and databases: its contents never change
// after Build (the sources it names are themselves goroutine-safe objects a host may control).
type Environment struct {
	// collations is the loaded collation set keyed by exact (case-sensitive) name. "C" is never here
	// (table-free, built in).
	collations map[string]*Collation
	// property is the Unicode property/casing table (collation.md §16), nil ⇒ the ASCII-casing
	// baseline (casing is table-free, version-free).
	property *propertyTable
	// zones is the loaded zone set (and link aliases) keyed by exact name. UTC and fixed offsets are
	// built in and never here.
	zones map[string]*zone
	// hosts is the frozen host function registry (extensibility.md §7). Never nil.
	hosts  *hostRegistry
	random RandomSource
	clock  ClockSource
}

// EnvironmentBuilder accumulates an Environment's contents (environment.md §3). Every load and
// registration validates as it is made (a malformed bundle is XX001, a conflicting function
// registration 42723). Loads are FIRST-WINS: a collation, property table, or zone name already
// present is not replaced, so re-loading the same bundle is an idempotent no-op. Build snapshots the
// builder, so later builder calls never affect an Environment already built. Not goroutine-safe.
type EnvironmentBuilder struct {
	collations map[string]*Collation
	property   *propertyTable
	zones      map[string]*zone
	functions  []hostFuncEntry
	random     RandomSource
	clock      ClockSource
}

// NewEnvironmentBuilder returns an empty builder: the built-in C collation and ASCII casing, UTC and
// fixed offsets, no host functions, the OS random source, and the wall clock.
func NewEnvironmentBuilder() *EnvironmentBuilder {
	return &EnvironmentBuilder{
		collations: map[string]*Collation{},
		zones:      map[string]*zone{},
		random:     OSRandomSource(),
		clock:      WallClock(),
	}
}

// NewEnvironment returns the empty environment (NewEnvironmentBuilder().Build()) — what open/create
// use when no environment is given.
func NewEnvironment() *Environment { return NewEnvironmentBuilder().Build() }

// LoadUnicodeData loads a JUCD Unicode-data bundle (spec/design/collation.md §4/§9): parse the bundle,
// merge the root + each per-locale delta (§5.1), add every collation by name, and keep the
// property/casing section (§16). First-wins, as above. A malformed bundle is XX001 and leaves the
// builder unchanged. Privileged host op — the engine reads no file path (§11); the host sources the
// bytes.
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

// LoadTimeZoneData loads a JTZ bundle (spec/design/timezones.md §3.3/§4): parse each zone's TZif,
// add it by name, then resolve each link alias onto its target's tables. First-wins, as above. A
// malformed bundle (or TZif) is XX001 and leaves the builder unchanged. UTC and fixed offsets are
// built in and need no load.
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

// RegisterFunction registers a host scalar function (extensibility.md §4.2). Errors on a negative
// cost (22023), an unknown argument/result type name (42704), or a second function with an identical
// (name, arg-types) signature (42723 — signature-level, not name-level: a host may overload a name
// across argument types). A signature that shadows a built-in is accepted but never reached
// (built-ins win).
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

// SetRandomSource installs the random source the uuid generators draw from (entropy.md §6); nil
// restores the default OS random source. Pass a *SeededRandom for a byte-identical cross-core stream.
func (b *EnvironmentBuilder) SetRandomSource(s RandomSource) {
	if s == nil {
		s = OSRandomSource()
	}
	b.random = s
}

// SetClockSource installs the clock the clock functions and uuidv7 read (entropy.md §6); nil
// restores the default wall clock. Pass a *ManualClock or *AdvancingClock for a controllable clock.
func (b *EnvironmentBuilder) SetClockSource(c ClockSource) {
	if c == nil {
		c = WallClock()
	}
	b.clock = c
}

// Build returns an Environment holding the builder's current contents. The maps and the function
// list are copied (their entries are shared by reference — they are immutable), so continuing to use
// the builder never changes the returned Environment.
func (b *EnvironmentBuilder) Build() *Environment {
	collations := make(map[string]*Collation, len(b.collations))
	for k, v := range b.collations {
		collations[k] = v
	}
	zones := make(map[string]*zone, len(b.zones))
	for k, v := range b.zones {
		zones[k] = v
	}
	functions := make([]hostFuncEntry, len(b.functions))
	copy(functions, b.functions)
	return &Environment{
		collations: collations,
		property:   b.property,
		zones:      zones,
		hosts:      &hostRegistry{functions: functions},
		random:     b.random,
		clock:      b.clock,
	}
}

// ToBuilder returns a builder pre-filled with this environment's contents (environment.md §3): the
// loaded data and host functions are shared by reference, and the sources are the same objects. A
// derived builder may add bundles and functions and replace sources; e itself is unaffected.
func (e *Environment) ToBuilder() *EnvironmentBuilder {
	b := &EnvironmentBuilder{
		collations: make(map[string]*Collation, len(e.collations)),
		property:   e.property,
		zones:      make(map[string]*zone, len(e.zones)),
		functions:  make([]hostFuncEntry, len(e.hosts.functions)),
		random:     e.random,
		clock:      e.clock,
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

// LoadedCollations introspects the loaded collation set (collation.md §4.2) — every collation a
// loaded bundle provides, ascending by name (no hash-iteration leak, CLAUDE.md §8). IsDefault is
// always false here (a per-database property). C is built in and not listed.
func (e *Environment) LoadedCollations() []collationInfo {
	colls := e.collationTables()
	out := make([]collationInfo, len(colls))
	for i, c := range colls {
		out[i] = collationInfo{
			Name:           c.Name,
			UnicodeVersion: c.UnicodeVersion,
			CLDRVersion:    c.CldrVersion,
			ContentHash:    crc32IEEE(serializeTable(c)),
			Description:    c.Description,
			IsDefault:      false,
			// The loaded set IS the version reference — it can never be skewed against itself.
			Verdict: verdictFull,
		}
	}
	return out
}

// LoadedTimeZones introspects the loaded zone set (timezones.md §3.3): every zone + alias a loaded
// bundle provides, ascending by name. UTC and fixed offsets are built in and not listed.
func (e *Environment) LoadedTimeZones() []timeZoneInfo {
	names := make([]string, 0, len(e.zones))
	for n := range e.zones {
		names = append(names, n)
	}
	sort.Strings(names)
	out := make([]timeZoneInfo, len(names))
	for i, n := range names {
		out[i] = timeZoneInfo{Name: n, TzdataVersion: e.zones[n].TzdataVersion}
	}
	return out
}

// collation looks up a loaded collation by exact name; nil ⇒ no loaded bundle provides it.
func (e *Environment) collation(name string) *Collation { return e.collations[name] }

// collationTables returns every loaded collation, ascending by name.
func (e *Environment) collationTables() []*Collation {
	out := make([]*Collation, 0, len(e.collations))
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
func (e *Environment) resolveZone(name string) (ZoneRef, bool) {
	if zr, ok := builtinZone(name); ok {
		return zr, true
	}
	if z := e.zones[name]; z != nil {
		return ZoneRef{zone: z}, true
	}
	return ZoneRef{}, false
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
		return newError(IoError, "random source failed: "+err.Error())
	}
	return nil
}

// nowMicros reads the installed clock: micros since the Unix epoch.
func (e *Environment) nowMicros() int64 { return e.clock.NowMicros() }
