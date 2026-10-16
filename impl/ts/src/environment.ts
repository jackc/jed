// The Environment (spec/design/environment.md) — the one explicit home for every input the engine
// takes from outside itself, except storage: the loaded Unicode data (collations + the casing
// table), the loaded time-zone data, the host functions, and the random and clock sources.
//
// An EnvironmentBuilder performs every load, registration, and source choice; build() returns an
// Environment with NO mutating methods, so a database's outside inputs never change while it is open
// (§3). An environment belongs to whoever builds it — never to the process: a host that wants one set
// for everything passes one environment to every createDatabase/openDatabase, and a library or test
// builds its own and stays isolated. Each database handle holds exactly one environment; its
// sessions, attached databases, and temp domains all use it (§3). An absent environment is a FRESH
// empty one (§7): the built-in `C` collation and ASCII casing, `UTC` and fixed offsets, no host
// functions, OS randomness, and the wall clock.
//
// Contents are immutable, so environments share them: build() and toBuilder() copy only the name
// maps, never a collation table, zone, or function entry. A derived builder may only ADD (bundles,
// functions — first-wins, as in any builder) and REPLACE sources; nothing can be removed (§10).

import {
  type Collation,
  loadBundle,
  openBundle,
  type PropertyTable,
  serializeTable,
} from "./collation.ts";
import type { CollationInfo } from "./executor.ts";
import {
  type HostFuncEntry,
  type HostFunctionSpec,
  HostFunctions,
  newHostFunction,
} from "./extension.ts";
import { crc32Ieee } from "./format.ts";
import { type ClockSource, osRandomSource, type RandomSource, wallClock } from "./seam.ts";
import { type TimeZoneInfo, timeZoneBundleZones, type Zone } from "./timezone.ts";

// The contents an Environment is built from. Internal: build() hands the builder's copies over.
type Contents = {
  collations: Map<string, Collation>;
  property: PropertyTable | undefined;
  zones: Map<string, Zone>;
  functions: readonly HostFuncEntry[];
  random: RandomSource;
  clock: ClockSource;
};

function emptyContents(): Contents {
  return {
    collations: new Map(),
    property: undefined,
    zones: new Map(),
    functions: [],
    random: osRandomSource(),
    clock: wallClock(),
  };
}

function compareNames(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

// Environment is an immutable set of outside inputs (spec/design/environment.md §3). `new
// Environment()` (or Environment.empty()) is the default; Environment.builder() starts a new one and
// env.toBuilder() derives one from env.
export class Environment {
  private readonly collationSet: ReadonlyMap<string, Collation>;
  private readonly propertyTable: PropertyTable | undefined;
  private readonly zoneSet: ReadonlyMap<string, Zone>;
  // null when no host function is registered — the built-in-only fast path.
  private readonly hostFunctionSet: HostFunctions | null;
  private readonly random: RandomSource;
  private readonly clock: ClockSource;

  // The `contents` parameter is internal (EnvironmentBuilder.build); a host calls `new Environment()`
  // for the empty default.
  constructor(contents?: Contents) {
    const c = contents ?? emptyContents();
    this.collationSet = c.collations;
    this.propertyTable = c.property;
    this.zoneSet = c.zones;
    this.hostFunctionSet = c.functions.length > 0 ? new HostFunctions(c.functions) : null;
    this.random = c.random;
    this.clock = c.clock;
  }

  // empty returns a fresh default environment (§7).
  static empty(): Environment {
    return new Environment();
  }

  // builder starts an empty EnvironmentBuilder.
  static builder(): EnvironmentBuilder {
    return new EnvironmentBuilder();
  }

  // toBuilder returns a builder pre-filled with this environment's contents (shared by reference, not
  // copied). Building it unchanged yields an equivalent environment; this one is never affected.
  toBuilder(): EnvironmentBuilder {
    return new EnvironmentBuilder({
      collations: new Map(this.collationSet),
      property: this.propertyTable,
      zones: new Map(this.zoneSet),
      functions: this.hostFunctionSet?.entries() ?? [],
      random: this.random,
      clock: this.clock,
    });
  }

  // loadedCollations lists every collation a loaded bundle provides, ascending by name (no
  // hash-iteration leak, CLAUDE.md §8). isDefault is always false (a per-database property) and the
  // verdict always "full" (the loaded set IS the version reference). C is built in and not listed.
  loadedCollations(): CollationInfo[] {
    return this.collationTables().map((c) => ({
      name: c.name,
      unicodeVersion: c.unicodeVersion,
      cldrVersion: c.cldrVersion,
      contentHash: crc32Ieee(serializeTable(c)),
      description: c.description,
      isDefault: false,
      verdict: "full" as const,
    }));
  }

  // loadedTimeZones lists every named zone (and alias) a loaded bundle provides, ascending by name.
  // UTC and fixed offsets are built in and not listed.
  loadedTimeZones(): TimeZoneInfo[] {
    return this.zones().map((z) => ({ name: z.name, tzdataVersion: z.tzdataVersion }));
  }

  // randomSource is the installed random source (§4).
  randomSource(): RandomSource {
    return this.random;
  }

  // clockSource is the installed clock source (§4).
  clockSource(): ClockSource {
    return this.clock;
  }

  // --- engine-internal lookups (not part of the embedding surface) ---

  // collation looks up a loaded collation by its exact (case-sensitive) name; undefined ⇒ no loaded
  // bundle provides it. "C" is never here (built in).
  collation(name: string): Collation | undefined {
    return this.collationSet.get(name);
  }

  // collationTables returns every loaded collation table, ascending by name.
  collationTables(): Collation[] {
    return [...this.collationSet.values()].sort((a, b) => compareNames(a.name, b.name));
  }

  // property returns the loaded Unicode property/casing table (collation.md §16), or undefined when
  // none is loaded — the ASCII-casing baseline.
  property(): PropertyTable | undefined {
    return this.propertyTable;
  }

  // zone looks up a loaded named zone (or alias) by exact name; undefined ⇒ not loaded.
  zone(name: string): Zone | undefined {
    return this.zoneSet.get(name);
  }

  // zones returns every loaded zone (aliases included), ascending by name.
  zones(): Zone[] {
    return [...this.zoneSet.values()].sort((a, b) => compareNames(a.name, b.name));
  }

  // hostFunctions is the frozen host-function set, or null when none is registered.
  hostFunctions(): HostFunctions | null {
    return this.hostFunctionSet;
  }
}

// EnvironmentBuilder accumulates an Environment's contents (spec/design/environment.md §3). Every
// method validates as it is called — a malformed bundle is XX001, a conflicting function registration
// 42723 — and returns the builder for chaining. build() snapshots the contents, so later builder calls
// never affect an environment already built.
export class EnvironmentBuilder {
  private readonly contents: Contents;

  // The `contents` parameter is internal (Environment.toBuilder); a host calls
  // `new EnvironmentBuilder()` or Environment.builder().
  constructor(contents?: Contents) {
    this.contents = contents ?? emptyContents();
  }

  // loadUnicodeData loads a JUCD Unicode-data bundle (spec/design/collation.md §4/§9): parse the
  // bundle, merge the root + each per-locale delta (§5.1), add every collation by name, and keep the
  // property/casing section (§16). FIRST-WINS — a collation name already present is NOT replaced, and
  // a property table is only kept if none is loaded yet, so re-loading the same bundle is a no-op.
  // Browser-safe (Uint8Array, no node:fs); the engine reads no path (§11).
  loadUnicodeData(data: Uint8Array): this {
    const { collations, property } = loadBundle(openBundle(data));
    for (const c of collations) {
      if (!this.contents.collations.has(c.name)) this.contents.collations.set(c.name, c);
    }
    if (property !== undefined && this.contents.property === undefined) {
      this.contents.property = property;
    }
    return this;
  }

  // loadTimeZoneData loads a JTZ time-zone bundle (spec/design/timezones.md §3.3): each zone and each
  // link alias becomes usable by AT TIME ZONE and the session time zone. FIRST-WINS — a name already
  // present is not replaced. UTC and fixed offsets are built in and need no load.
  loadTimeZoneData(data: Uint8Array): this {
    for (const z of timeZoneBundleZones(data)) {
      if (!this.contents.zones.has(z.name)) this.contents.zones.set(z.name, z);
    }
    return this;
  }

  // registerFunction registers a host scalar function (spec/design/extensibility.md §4.2/§7): exact
  // signatures, strict, a single-row or batch kernel. 22023 on a negative cost or not exactly one
  // kernel, 42704 on an unknown type, 42723 on a duplicate (name, argTypes) signature.
  registerFunction(spec: HostFunctionSpec): this {
    const entry = newHostFunction(this.contents.functions, spec);
    this.contents.functions = [...this.contents.functions, entry];
    return this;
  }

  // randomSource installs the random source (default: osRandomSource()).
  randomSource(source: RandomSource): this {
    this.contents.random = source;
    return this;
  }

  // clockSource installs the clock source (default: wallClock()).
  clockSource(source: ClockSource): this {
    this.contents.clock = source;
    return this;
  }

  // build returns an immutable Environment holding a snapshot of the builder's contents.
  build(): Environment {
    const c = this.contents;
    return new Environment({
      collations: new Map(c.collations),
      property: c.property,
      zones: new Map(c.zones),
      functions: c.functions,
      random: c.random,
      clock: c.clock,
    });
  }
}
