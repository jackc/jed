// Collation: a hand-written Unicode Collation Algorithm (UTS #10) — a compiler (a canonical
// definition → jed's own compiled table) and an executor (table + string → a memcmp-ordered sort
// key), plus the portable .coll artifact codec. The cross-core contract for linguistic ordering
// (spec/design/collation.md §2/§6): both routines are hand-written per core (CLAUDE.md §5 forbids
// codegenning them) and byte-identical given identical input, pinned by spec/collation/vectors/.
// Byte formats: spec/collation/README.md (definition §1, table §2, artifact §3, sort key §4).
// Mirrors impl/rust/src/collation.rs and impl/go/collation.go.
//
// Slice 1b: host-free — compileCollation (DUCET allkeys root + LDML tailoring), sortKey (the
// executor), and saveCollation/openCollation (the artifact round-trip). No SQL surface, no
// persistence, no host seam. Only deterministic collations and non-ignorable variable weighting.
//
// Code points are handled by `for...of` / codePointAt — NOT charCodeAt — so astral characters
// (e.g. 😀 U+1F600) work despite JS's UTF-16 strings (types.md §11, the TS trap).

import { engineError } from "./errors.ts";
import { crc32Ieee } from "./format.ts";
import { lz4Compress, lz4Decompress } from "./lz4.ts";
import { encodeTerminated } from "./encoding.ts";

// One collation element — a weight triple plus a flags byte. 7 bytes on disk
// (spec/collation/README.md §2). A 0x0000 weight is ignorable at that level (skipped, §4).
export interface Ce {
  flags: number; // bit0 = variable; non-ignorable in slice 1 (§6)
  l1: number;
  l2: number;
  l3: number;
}

const CE_VARIABLE = 0x01; // flags bit0 — a variable collation element (DUCET marker '*')

function ce(l1: number, l2: number, l3: number): Ce {
  return { flags: 0, l1, l2, l3 };
}

interface SingleEntry {
  cp: number;
  ces: Ce[];
}
interface ContractionEntry {
  seq: number[];
  ces: Ce[];
}

// A compiled, fully-resolved collation: jed's own table plus its metadata, database-independent
// (collation.md §4). The arrays are kept sorted (the §2 contract) so the serialized bytes are
// deterministic.
export interface Collation {
  name: string;
  unicodeVersion: string; // from the definition's @version record ("" if none)
  cldrVersion: string; // "" — root-only / unset in slice 1b
  // optional human-readable provenance; excluded from the content hash (§3). Only
  // ExtractHostCollation (a later slice) generates one; compileCollation leaves it "".
  description: string;
  singles: SingleEntry[]; // ascending by code point
  contractions: ContractionEntry[]; // lexicographic by code-point sequence
}

// the dev tailoring weight allocator (spec/collation/README.md §1.2) — fixed constants so every
// core allocates identical weights from identical rules. A real ICU-faithful allocator replaces
// this with the version-pinned DUCET follow-on (collation.md §14).
const BASE_L2 = 0x0020;
const BASE_L3 = 0x0002;
const PRIMARY_GAP = 0x0200;
const SECONDARY_GAP = 0x0020;
const TERTIARY_GAP = 0x0006;

function featureErr(msg: string): Error {
  return engineError("feature_not_supported", msg);
}
function syntaxErr(msg: string): Error {
  return engineError("syntax_error", msg);
}
function corruptErr(msg: string): Error {
  return engineError("data_corrupted", msg);
}

// ============================================================================================
// Compiler: a canonical definition → a Collation.
// ============================================================================================

// compileCollation parses a collation definition (spec/collation/README.md §1) and compiles it
// into a jed table. The definition is a single stream, line-dispatched: @… records, allkeys
// mapping lines (codepoints ; elements), and LDML rule lines (&anchor < x …). Host-free.
export function compileCollation(name: string, definition: string): Collation {
  let unicodeVersion = "";
  // working map: code-point sequence → CEs, ordered by first insertion.
  const keys: number[][] = [];
  const cesByKey = new Map<string, Ce[]>();

  const setMapping = (seq: number[], ces: Ce[], replace: boolean): void => {
    const k = seqKey(seq);
    if (cesByKey.has(k)) {
      if (!replace) throw syntaxErr(`collation: duplicate mapping for ${seq}`);
      cesByKey.set(k, ces);
      return;
    }
    keys.push(seq);
    cesByKey.set(k, ces);
  };

  for (const raw of definition.split("\n")) {
    const line = stripComment(raw).trim();
    if (line === "") continue;
    if (line.startsWith("@")) {
      const fields = line.slice(1).trim().split(/\s+/);
      if (fields.length >= 2 && fields[0] === "version") unicodeVersion = fields[1];
      // other records ignored in slice 1b.
      continue;
    }
    if (line.startsWith("&")) {
      applyTailoring(keys, cesByKey, setMapping, line);
      continue;
    }
    parseMapping(setMapping, line);
  }

  const singles: SingleEntry[] = [];
  const contractions: ContractionEntry[] = [];
  for (const seq of keys) {
    const ces = cesByKey.get(seqKey(seq))!;
    if (seq.length === 1) singles.push({ cp: seq[0], ces });
    else contractions.push({ seq, ces });
  }
  singles.sort((a, b) => a.cp - b.cp);
  contractions.sort((a, b) => (seqLess(a.seq, b.seq) ? -1 : seqLess(b.seq, a.seq) ? 1 : 0));

  return { name, unicodeVersion, cldrVersion: "", description: "", singles, contractions };
}

function seqKey(seq: number[]): string {
  return seq.join(",");
}

function stripComment(line: string): string {
  const i = line.indexOf("#");
  return i >= 0 ? line.slice(0, i) : line;
}

type SetMapping = (seq: number[], ces: Ce[], replace: boolean) => void;

function parseMapping(set: SetMapping, line: string): void {
  const i = line.indexOf(";");
  if (i < 0) throw syntaxErr(`collation: mapping line has no ';': ${line}`);
  const seq: number[] = [];
  for (const tok of line.slice(0, i).trim().split(/\s+/)) {
    if (tok === "") continue;
    seq.push(parseHex(tok));
  }
  if (seq.length === 0) throw syntaxErr(`collation: mapping with no code point: ${line}`);
  const ces = parseElements(line.slice(i + 1).trim());
  if (ces.length === 0) throw syntaxErr(`collation: mapping with no element: ${line}`);
  set(seq, ces, false);
}

// parseElements parses [*0209.0020.0002][.0000.0047.0002]… into collation elements.
function parseElements(s: string): Ce[] {
  const ces: Ce[] = [];
  let i = 0;
  while (i < s.length) {
    if (s[i] === " " || s[i] === "\t") {
      i++;
      continue;
    }
    if (s[i] !== "[") throw syntaxErr(`collation: expected '[' in elements: ${s}`);
    const end = s.indexOf("]", i);
    if (end < 0) throw syntaxErr(`collation: unterminated element: ${s}`);
    const inner = s.slice(i + 1, end);
    if (inner === "") throw syntaxErr(`collation: empty element: ${s}`);
    let flags = 0;
    if (inner[0] === ".") {
      // non-ignorable
    } else if (inner[0] === "*") {
      flags |= CE_VARIABLE;
    } else {
      throw syntaxErr(`collation: bad element marker: ${inner}`);
    }
    const parts = inner.slice(1).split(".");
    ces.push({
      flags,
      l1: parseHex16(parts[0] ?? ""),
      l2: parseHex16(parts[1] ?? ""),
      l3: parseHex16(parts[2] ?? ""),
    });
    i = end + 1;
  }
  return ces;
}

// --- LDML tailoring ---------------------------------------------------------------------------

type Rel = "primary" | "secondary" | "tertiary" | "identical";
interface Tok {
  op?: Rel;
  cp?: number;
}

// applyTailoring applies one LDML rule line: &anchor REL target (REL target)*. Single-character
// anchor/targets only in slice 1b.
function applyTailoring(
  keys: number[][],
  cesByKey: Map<string, Ce[]>,
  set: SetMapping,
  line: string,
): void {
  const toks = tokenizeRule(line.slice(1).trim());
  if (toks.length === 0 || toks[0].op !== undefined) {
    throw syntaxErr(`collation: rule must start with an anchor: ${line}`);
  }
  let cur = singleCe(cesByKey, toks[0].cp!);
  if (cur === null) {
    throw syntaxErr(`collation: rule anchor U+${toks[0].cp!.toString(16)} not a single element`);
  }
  let i = 1;
  while (i < toks.length) {
    if (toks[i].op === undefined)
      throw syntaxErr(`collation: expected a relation operator: ${line}`);
    const op = toks[i].op!;
    i++;
    if (i >= toks.length || toks[i].op !== undefined) {
      throw syntaxErr(`collation: relation needs a target: ${line}`);
    }
    const target = toks[i].cp!;
    i++;
    const newCe = allocAfter(keys, cesByKey, cur, op);
    set([target], [newCe], true);
    cur = newCe;
  }
}

function tokenizeRule(s: string): Tok[] {
  const out: Tok[] = [];
  const runes = Array.from(s); // code points, not UTF-16 units
  let i = 0;
  while (i < runes.length) {
    const c = runes[i];
    if (c === " " || c === "\t") {
      i++;
    } else if (c === "<") {
      let n = 0;
      while (i < runes.length && runes[i] === "<") {
        n++;
        i++;
      }
      if (n === 1) out.push({ op: "primary" });
      else if (n === 2) out.push({ op: "secondary" });
      else if (n === 3) out.push({ op: "tertiary" });
      else throw syntaxErr("collation: '<<<<' (quaternary) not supported");
    } else if (c === "=") {
      out.push({ op: "identical" });
      i++;
    } else {
      out.push({ cp: c.codePointAt(0)! });
      i++;
    }
  }
  return out;
}

// singleCe returns the CE of a single-code-point mapping with exactly one element, else null.
function singleCe(cesByKey: Map<string, Ce[]>, cp: number): Ce | null {
  const ces = cesByKey.get(seqKey([cp]));
  if (ces === undefined || ces.length !== 1) return null;
  return ces[0];
}

// allocAfter allocates a fresh CE placed after cur at the given relation level (the dev allocator).
function allocAfter(keys: number[][], cesByKey: Map<string, Ce[]>, cur: Ce, rel: Rel): Ce {
  if (rel === "identical") return { flags: cur.flags, l1: cur.l1, l2: cur.l2, l3: cur.l3 };
  if (rel === "primary") {
    const succ = minWeightAbove(keys, cesByKey, (c) => (c.l1 > cur.l1 ? c.l1 : null));
    return ce(freshGap(cur.l1, succ, PRIMARY_GAP), BASE_L2, BASE_L3);
  }
  if (rel === "secondary") {
    const succ = minWeightAbove(keys, cesByKey, (c) =>
      c.l1 === cur.l1 && c.l2 > cur.l2 ? c.l2 : null,
    );
    return ce(cur.l1, freshGap(cur.l2, succ, SECONDARY_GAP), BASE_L3);
  }
  // tertiary
  const succ = minWeightAbove(keys, cesByKey, (c) =>
    c.l1 === cur.l1 && c.l2 === cur.l2 && c.l3 > cur.l3 ? c.l3 : null,
  );
  return ce(cur.l1, cur.l2, freshGap(cur.l3, succ, TERTIARY_GAP));
}

// minWeightAbove returns the smallest weight matching f across every CE in the table, or null.
function minWeightAbove(
  keys: number[][],
  cesByKey: Map<string, Ce[]>,
  f: (c: Ce) => number | null,
): number | null {
  let best: number | null = null;
  for (const seq of keys) {
    for (const c of cesByKey.get(seqKey(seq))!) {
      const w = f(c);
      if (w !== null && (best === null || w < best)) best = w;
    }
  }
  return best;
}

// freshGap: the midpoint to succ if one exists (needs room ≥ 2), else lo + gap (append).
function freshGap(lo: number, succ: number | null, gap: number): number {
  if (succ !== null) {
    if (succ - lo < 2) {
      throw featureErr(
        "collation: tailoring weight space exhausted (dense-insertion allocator deferred)",
      );
    }
    return lo + Math.floor((succ - lo) / 2);
  }
  if (lo + gap > 0xffff) {
    throw featureErr("collation: tailoring weight overflow (allocator deferred)");
  }
  return lo + gap;
}

function parseHex(s: string): number {
  const v = Number.parseInt(s.trim(), 16);
  if (!Number.isInteger(v)) throw syntaxErr(`collation: bad code point hex: ${JSON.stringify(s)}`);
  return v;
}
function parseHex16(s: string): number {
  const v = Number.parseInt(s.trim(), 16);
  if (!Number.isInteger(v) || v < 0 || v > 0xffff) {
    throw syntaxErr(`collation: bad weight hex: ${JSON.stringify(s)}`);
  }
  return v;
}

// ============================================================================================
// Executor: (Collation, string) → sort key (spec/collation/README.md §4).
// ============================================================================================

const UTF8 = new TextEncoder();

// sortKey is the byte string whose memcmp order equals the collation's logical order:
// L1-weights ‖ 0x0000 ‖ L2-weights ‖ 0x0000 ‖ L3-weights ‖ 0x0000 ‖ Ckey(original).
export function sortKey(coll: Collation, s: string): Uint8Array {
  const cps: number[] = [];
  for (const chr of s) cps.push(chr.codePointAt(0)!); // code points, not UTF-16 units
  const ces = collationElements(coll, cps);

  const out: number[] = [];
  for (const c of ces) if (c.l1 !== 0) pushU16(out, c.l1);
  out.push(0, 0);
  for (const c of ces) if (c.l2 !== 0) pushU16(out, c.l2);
  out.push(0, 0);
  for (const c of ces) if (c.l3 !== 0) pushU16(out, c.l3);
  out.push(0, 0);
  // identical level: the §2.4 C-key of the original UTF-8 string.
  for (const b of encodeTerminated(UTF8.encode(s))) out.push(b);
  return Uint8Array.from(out);
}

function collationElements(coll: Collation, cps: number[]): Ce[] {
  let maxContraction = 0;
  for (const c of coll.contractions)
    if (c.seq.length > maxContraction) maxContraction = c.seq.length;
  const out: Ce[] = [];
  let i = 0;
  while (i < cps.length) {
    let matched = false;
    let clen = Math.min(maxContraction, cps.length - i);
    while (clen >= 2) {
      const ces = lookupContraction(coll, cps.slice(i, i + clen));
      if (ces !== null) {
        out.push(...ces);
        i += clen;
        matched = true;
        break;
      }
      clen--;
    }
    if (matched) continue;
    const ces = lookupSingle(coll, cps[i]);
    if (ces !== null) {
      out.push(...ces);
      i++;
      continue;
    }
    throw featureErr(
      `collation: code point U+${cps[i].toString(16).toUpperCase()} has no mapping (implicit weights deferred)`,
    );
  }
  return out;
}

function lookupSingle(coll: Collation, cp: number): Ce[] | null {
  for (const s of coll.singles) if (s.cp === cp) return s.ces;
  return null;
}

function lookupContraction(coll: Collation, seq: number[]): Ce[] | null {
  for (const c of coll.contractions) if (eqSeq(c.seq, seq)) return c.ces;
  return null;
}

function eqSeq(a: number[], b: number[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function seqLess(a: number[], b: number[]): boolean {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i] < b[i];
  return a.length < b.length;
}

function pushU16(out: number[], v: number): void {
  out.push((v >> 8) & 0xff, v & 0xff);
}
function pushU32(out: number[], v: number): void {
  out.push((v >>> 24) & 0xff, (v >>> 16) & 0xff, (v >>> 8) & 0xff, v & 0xff);
}

// ============================================================================================
// Compiled-table bytes (spec/collation/README.md §2) and the .coll artifact (§3).
// ============================================================================================

// serializeTable serializes the compiled table (§2) — the bytes the content hash covers.
export function serializeTable(coll: Collation): Uint8Array {
  return serializeEntries(coll.singles, coll.contractions);
}

// serializeEntries writes the §2 table-entry bytes — shared by the .coll artifact (a full table)
// and the JUCD bundle's root / tailoring sections (a full table or a sparse override, README §2/§5).
function serializeEntries(singles: SingleEntry[], contractions: ContractionEntry[]): Uint8Array {
  const out: number[] = [];
  out.push(1); // layout_version
  pushU32(out, singles.length);
  pushU32(out, contractions.length);
  for (const s of singles) {
    pushU32(out, s.cp);
    out.push(s.ces.length);
    for (const c of s.ces) pushCe(out, c);
  }
  for (const c of contractions) {
    out.push(c.seq.length);
    for (const cp of c.seq) pushU32(out, cp);
    out.push(c.ces.length);
    for (const e of c.ces) pushCe(out, e);
  }
  return Uint8Array.from(out);
}

function pushCe(out: number[], c: Ce): void {
  out.push(c.flags);
  pushU16(out, c.l1);
  pushU16(out, c.l2);
  pushU16(out, c.l3);
}

// saveCollation writes the portable .coll artifact (§3): magic + metadata + provenance + CRC-32 +
// the LZ4-compressed table. openCollation is its exact inverse; the round-trip is byte-identical
// on every core (collation.md §10).
export function saveCollation(coll: Collation): Uint8Array {
  const table = serializeTable(coll);
  const hash = crc32Ieee(table);
  const comp = lz4Compress(table);

  const out: number[] = [];
  out.push(0x4a, 0x43, 0x4f, 0x4c, 0x4c, 0x00); // "JCOLL\0" magic
  pushU16(out, 1); // format_version
  pushStr(out, coll.name);
  pushStr(out, coll.unicodeVersion);
  pushStr(out, coll.cldrVersion);
  pushStr(out, coll.description);
  pushU32(out, hash);
  pushU32(out, table.length);
  pushU32(out, comp.length);
  for (const b of comp) out.push(b);
  return Uint8Array.from(out);
}

function pushStr(out: number[], s: string): void {
  const bytes = UTF8.encode(s);
  pushU16(out, bytes.length);
  for (const b of bytes) out.push(b);
}

// openCollation reads a .coll artifact (§3) back into a Collation. Verifies the magic, the format
// version, and the content hash; a malformed or tampered artifact is XX001 (data_corrupted).
export function openCollation(bytes: Uint8Array): Collation {
  const r = new Reader(bytes);
  const magic = r.take(6);
  if (
    !(
      magic[0] === 0x4a &&
      magic[1] === 0x43 &&
      magic[2] === 0x4f &&
      magic[3] === 0x4c &&
      magic[4] === 0x4c &&
      magic[5] === 0x00
    )
  ) {
    throw corruptErr("collation: bad artifact magic");
  }
  const fmt = r.u16();
  if (fmt !== 1) throw corruptErr(`collation: unsupported artifact format_version ${fmt}`);
  const name = r.str();
  const unicodeVersion = r.str();
  const cldrVersion = r.str();
  const description = r.str();
  const hash = r.u32();
  const rawLen = r.u32();
  const compLen = r.u32();
  const comp = r.take(compLen);
  if (r.pos !== bytes.length) throw corruptErr("collation: trailing bytes after artifact");
  const table = lz4Decompress(comp, rawLen);
  if (crc32Ieee(table) !== hash) throw corruptErr("collation: artifact content hash mismatch");
  const [singles, contractions] = deserializeTable(table);
  return { name, unicodeVersion, cldrVersion, description, singles, contractions };
}

// --- The engine-global loaded collation set (spec/design/collation.md §4/§9) --------------------
//
// The bare binary carries NO Unicode data — no embedded .coll, no casing tables (§9/§16, the SQLite
// model). All collations arrive at runtime: a host hands the engine a JUCD bundle's bytes via
// loadUnicodeData (db.loadUnicodeData), the engine merges its root + per-locale deltas (§5.1) and
// adds the resulting collations here. This set is PROCESS-GLOBAL (module-scoped) — a property of the
// running engine, not of one Engine handle (the spec's "loaded set available to any database on
// this handle", §4.2). Global is what lets a file REFERENCING a collation be opened after the bundle
// is loaded: open resolves the referenced table from here (format.ts), and open mints the handle, so
// the data cannot live on the handle. "C" is never here (table-free, built in).
//
// The bytes are jed's OWN pinned tables (byte-identical across cores, §9/§10), so loading restores no
// nondeterminism — a use stays pure regardless of where the host sourced the bytes (file / fetch /
// compiled-in asset / fetched in the browser). The real version-pinned production bundle is
// spec/collation/fixtures/unicode.jucd (unicode = the CLDR-DUCET root, es = root + the Spanish ñ
// tailoring); the dev-* fixtures are not part of it (they only drive the cross-core vectors).

const loadedColl = new Map<string, Collation>();

// loadedProp is the engine-global Unicode property/casing table (spec/design/collation.md §16),
// undefined until a bundle carrying a property section is loaded. Its presence is the binary "casing
// regime" the C/ASCII baseline flips on: undefined ⇒ casing is ASCII-only, table-free, version-free.
// FIRST-WINS, like loadedColl. (Module-scoped, so a let with a private accessor.)
let loadedProp: PropertyTable | undefined;

// loadUnicodeData loads a JUCD Unicode-data bundle into the engine-global loaded set
// (spec/design/collation.md §4/§9): parse the bundle, merge the root + each per-locale delta (§5.1),
// register every collation by name, and store the property/casing section (§16). ADDITIVE /
// FIRST-WINS — a collation name already present is NOT replaced (the first bundle to provide it wins;
// resolution is by name in load order, §4.2), and a property table is only stored if none is loaded
// yet, so re-loading the same bundle is an idempotent no-op. A malformed bundle is XX001.
//
// This is the engine primitive behind db.loadUnicodeData. Because the set is process-global it may be
// called BEFORE opening any file (which is required: opening a file that references a collation
// resolves its table from this set). Privileged host op — the engine reads no file path and reaches
// no host data (§11); the host sources the bytes (browser-safe — Uint8Array, no node:fs).
export function loadUnicodeData(data: Uint8Array): void {
  const { collations, property } = loadBundle(openBundle(data));
  for (const c of collations) {
    if (!loadedColl.has(c.name)) loadedColl.set(c.name, c);
  }
  if (property !== undefined && loadedProp === undefined) loadedProp = property;
}

// loadedProperty returns the engine-global property/casing table, or undefined if no bundle providing
// one has been loaded (§16) — undefined ⇒ the ASCII-casing baseline. The casing functions look this up
// ONCE per evaluation and pass it to the pure kernels below, which keeps the un-loaded regime testable.
export function loadedProperty(): PropertyTable | undefined {
  return loadedProp;
}

// loadedCollation looks up a collation in the engine-global LOADED set by its exact (case-sensitive)
// name (spec/design/collation.md §4/§9). undefined ⇒ no loaded bundle provides it. "C" is never here
// (table-free, built in). The resolver consults the database's referenced collations first, then this
// set.
export function loadedCollation(name: string): Collation | undefined {
  return loadedColl.get(name);
}

// versionSkew is the slice-2d version-skew verdict (spec/design/collation.md §12, compatibility.md
// §7): given a collation name and the (unicode, cldr) version the FILE pinned its keys under (§5), it
// returns [loadedUnicode, loadedCldr] when a loaded bundle provides name at a DIFFERENT version — the
// object using it is read-only (XX002 on write) — else undefined (Full: the same version, or no
// loaded table to disagree). A pure, total comparison so EVERY core computes the identical verdict
// (the §10 cross-core contract). The read side never consults this — a skewed read recomputes against
// the loaded table (the heap-scan fallback, compatibility.md §8).
export function versionSkew(
  name: string,
  fileUnicode: string,
  fileCldr: string,
): [string, string] | undefined {
  const loaded = loadedColl.get(name);
  if (loaded === undefined) return undefined;
  if (loaded.unicodeVersion !== fileUnicode || loaded.cldrVersion !== fileCldr) {
    return [loaded.unicodeVersion, loaded.cldrVersion];
  }
  return undefined;
}

// loadedCollationTables returns every loaded collation, ascending by name — a deterministic order
// with no hash-iteration leak (CLAUDE.md §8). The raw tables; the public CollationInfo view is the
// Engine.loadedCollations method (executor.ts).
export function loadedCollationTables(): Collation[] {
  return [...loadedColl.values()].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
}

function deserializeTable(table: Uint8Array): [SingleEntry[], ContractionEntry[]] {
  const r = new Reader(table);
  const layout = r.u8();
  if (layout !== 1) throw corruptErr(`collation: unsupported table layout_version ${layout}`);
  const numSingles = r.u32();
  const numContractions = r.u32();
  const singles: SingleEntry[] = [];
  for (let n = 0; n < numSingles; n++) {
    const cp = r.u32();
    singles.push({ cp, ces: r.ces() });
  }
  const contractions: ContractionEntry[] = [];
  for (let n = 0; n < numContractions; n++) {
    const seqLen = r.u8();
    const seq: number[] = [];
    for (let j = 0; j < seqLen; j++) seq.push(r.u32());
    contractions.push({ seq, ces: r.ces() });
  }
  if (r.pos !== table.length) throw corruptErr("collation: trailing bytes after table");
  return [singles, contractions];
}

class Reader {
  b: Uint8Array;
  pos = 0;
  constructor(b: Uint8Array) {
    this.b = b;
  }
  take(n: number): Uint8Array {
    if (this.pos + n > this.b.length) throw corruptErr("collation: artifact truncated");
    const s = this.b.subarray(this.pos, this.pos + n);
    this.pos += n;
    return s;
  }
  u8(): number {
    return this.take(1)[0];
  }
  u16(): number {
    const s = this.take(2);
    return (s[0] << 8) | s[1];
  }
  u32(): number {
    const s = this.take(4);
    return ((s[0] << 24) | (s[1] << 16) | (s[2] << 8) | s[3]) >>> 0;
  }
  ces(): Ce[] {
    const n = this.u8();
    const ces: Ce[] = [];
    for (let j = 0; j < n; j++)
      ces.push({ flags: this.u8(), l1: this.u16(), l2: this.u16(), l3: this.u16() });
    return ces;
  }
  str(): string {
    const n = this.u16();
    return new TextDecoder().decode(this.take(n));
  }
  // cps reads a u8-length-prefixed run of u32 code points (the JUCD property section, README §5).
  cps(): number[] {
    const n = this.u8();
    const out: number[] = [];
    for (let j = 0; j < n; j++) out.push(this.u32());
    return out;
  }
}

// ============================================================================================
// The JUCD Unicode-data bundle (spec/collation/README.md §5) — the host-loaded container.
// A manifest-indexed container of sections: the Unicode property/casing tables, the shared DUCET
// root (a full §2 table, stored once, and itself a usable collation under its name), and per-locale
// tailoring sections (sparse overrides merged onto the root at load — §5.1). Mirror of
// impl/rust/src/collation.rs; byte-identical by construction (CLAUDE.md §8).
// ============================================================================================

// A simple 1:1 case mapping (a field equal to cp is the identity mapping).
export interface SimpleCase {
  cp: number;
  upper: number;
  lower: number;
  title: number;
}

// A full (multi-code-point) case mapping (README §5). Conditional/locale context is reserved.
export interface SpecialCasing {
  cp: number;
  upper: number[];
  lower: number[];
  title: number[];
}

// The Unicode property/casing data (README §5). First cut: case mappings only (normalization is a
// reserved later sub-table). simple is ascending by code point; special likewise.
export interface PropertyTable {
  simple: SimpleCase[];
  special: SpecialCasing[];
}

// ============================================================================================
// Casing kernels (spec/design/collation.md §16) — the production upper/lower/ILIKE folds. Each takes
// the resolved property table EXPLICITLY (undefined ⇒ the ASCII baseline), so the evaluator does the
// one engine-global loadedProperty() lookup and the kernels stay pure — making the un-loaded (ASCII)
// regime deterministically testable. CODE-POINT iteration (for...of / fromCodePoint), so an astral
// cased letter folds correctly despite TS's UTF-16 strings (the cross-core trap, types.md §11).
// ============================================================================================

// foldCase folds a string's case. prop === undefined is the ASCII baseline (fold a–z/A–Z, pass other
// code points through — the SQLite default, version-independent). A defined prop folds via the loaded
// Unicode tables — full mappings incl. SpecialCasing expansions (ß→SS). upper selects the direction.
// Backs the upper(text)/lower(text) functions (functions.md §9).
export function foldCase(s: string, upper: boolean, prop: PropertyTable | undefined): string {
  let out = "";
  for (const ch of s) {
    const cp = ch.codePointAt(0)!;
    if (prop === undefined) {
      out += asciiFold(ch, cp, upper);
      continue;
    }
    const sc = propLookupSpecial(prop, cp);
    if (sc !== undefined) {
      for (const m of upper ? sc.upper : sc.lower) out += String.fromCodePoint(m);
      continue;
    }
    const sm = propLookupSimple(prop, cp);
    out += sm !== undefined ? String.fromCodePoint(upper ? sm.upper : sm.lower) : ch;
  }
  return out;
}

// foldLowerSimple folds to lowercase for case-insensitive matching (ILIKE) — SIMPLE 1:1 mappings only
// (never the expanding SpecialCasing forms), so every code point stays one code point and the
// matcher's _/length semantics are preserved (grammar.md §22). ASCII baseline when prop is undefined.
export function foldLowerSimple(s: string, prop: PropertyTable | undefined): string {
  let out = "";
  for (const ch of s) {
    const cp = ch.codePointAt(0)!;
    if (prop === undefined) {
      out += asciiFold(ch, cp, false);
      continue;
    }
    const sm = propLookupSimple(prop, cp);
    out += sm !== undefined ? String.fromCodePoint(sm.lower) : ch;
  }
  return out;
}

// asciiFold folds ASCII a–z/A–Z only, returning the source character for any other code point.
function asciiFold(ch: string, cp: number, upper: boolean): string {
  if (upper) {
    if (cp >= 0x61 && cp <= 0x7a) return String.fromCodePoint(cp - 32);
  } else if (cp >= 0x41 && cp <= 0x5a) {
    return String.fromCodePoint(cp + 32);
  }
  return ch;
}

function propLookupSimple(p: PropertyTable, cp: number): SimpleCase | undefined {
  let lo = 0;
  let hi = p.simple.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (p.simple[mid].cp < cp) lo = mid + 1;
    else hi = mid;
  }
  return lo < p.simple.length && p.simple[lo].cp === cp ? p.simple[lo] : undefined;
}

function propLookupSpecial(p: PropertyTable, cp: number): SpecialCasing | undefined {
  let lo = 0;
  let hi = p.special.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (p.special[mid].cp < cp) lo = mid + 1;
    else hi = mid;
  }
  return lo < p.special.length && p.special[lo].cp === cp ? p.special[lo] : undefined;
}

// A §2 table — a full table for a root, a sparse override for a tailoring — plus the collation name
// the manifest records.
interface BundleEntries {
  name: string;
  singles: SingleEntry[];
  contractions: ContractionEntry[];
}

// One bundle section: the property tables (kind 0), the shared root (kind 1), or a per-locale
// override (kind 2).
export type Section =
  | { kind: 0; property: PropertyTable }
  | { kind: 1; entries: BundleEntries }
  | { kind: 2; entries: BundleEntries };

// A parsed JUCD bundle (README §5): the shared header version axis + its sections.
export interface Bundle {
  unicodeVersion: string;
  cldrVersion: string;
  description: string;
  sections: Section[];
}

// saveBundle serializes a JUCD bundle (README §5): header, manifest (a TOC with per-section
// offsets), the LZ4-compressed section bodies, and a trailing CRC-32 over everything before it.
export function saveBundle(b: Bundle): Uint8Array {
  interface Packed {
    kind: number;
    name: string;
    hash: number;
    rawLen: number;
    comp: Uint8Array;
  }
  const packed: Packed[] = b.sections.map((s) => {
    let kind: number;
    let name: string;
    let raw: Uint8Array;
    if (s.kind === 0) {
      kind = 0;
      name = "";
      raw = serializeProperty(s.property);
    } else {
      kind = s.kind;
      name = s.entries.name;
      raw = serializeEntries(s.entries.singles, s.entries.contractions);
    }
    return { kind, name, hash: crc32Ieee(raw), rawLen: raw.length, comp: lz4Compress(raw) };
  });

  const header: number[] = [];
  header.push(0x4a, 0x55, 0x43, 0x44, 0x00, 0x00); // "JUCD\0\0" magic
  pushU16(header, 1); // format_version
  pushStr(header, b.unicodeVersion);
  pushStr(header, b.cldrVersion);
  pushStr(header, b.description);

  // Manifest length is fixed once the names are known (per entry: kind 1 + name 2+len + hash 4 +
  // raw_len 4 + comp_len 4 + offset 4), so the body offsets can be computed up front.
  let manifestLen = 2;
  for (const p of packed) manifestLen += 1 + 2 + UTF8.encode(p.name).length + 4 + 4 + 4 + 4;
  const bodyStart = header.length + manifestLen;

  const manifest: number[] = [];
  pushU16(manifest, packed.length);
  let off = bodyStart;
  for (const p of packed) {
    manifest.push(p.kind);
    pushStr(manifest, p.name);
    pushU32(manifest, p.hash);
    pushU32(manifest, p.rawLen);
    pushU32(manifest, p.comp.length);
    pushU32(manifest, off);
    off += p.comp.length;
  }

  const out: number[] = [...header, ...manifest];
  for (const p of packed) for (const x of p.comp) out.push(x);
  const crc = crc32Ieee(Uint8Array.from(out));
  pushU32(out, crc);
  return Uint8Array.from(out);
}

// openBundle reads a JUCD bundle (README §5). Verifies the trailing CRC, the magic, the format
// version, and each section's content hash; a malformed bundle is XX001 (data_corrupted).
export function openBundle(data: Uint8Array): Bundle {
  if (data.length < 4) throw corruptErr("bundle: truncated");
  const body = data.subarray(0, data.length - 4);
  const want =
    ((data[data.length - 4] << 24) |
      (data[data.length - 3] << 16) |
      (data[data.length - 2] << 8) |
      data[data.length - 1]) >>>
    0;
  if (crc32Ieee(body) !== want) throw corruptErr("bundle: trailer checksum mismatch");

  const r = new Reader(data);
  const magic = r.take(6);
  if (
    !(
      magic[0] === 0x4a &&
      magic[1] === 0x55 &&
      magic[2] === 0x43 &&
      magic[3] === 0x44 &&
      magic[4] === 0x00 &&
      magic[5] === 0x00
    )
  ) {
    throw corruptErr("bundle: bad magic");
  }
  const fmt = r.u16();
  if (fmt !== 1) throw corruptErr(`bundle: unsupported format_version ${fmt}`);
  const unicodeVersion = r.str();
  const cldrVersion = r.str();
  const description = r.str();
  const count = r.u16();

  interface M {
    kind: number;
    name: string;
    hash: number;
    rawLen: number;
    compLen: number;
    offset: number;
  }
  const ms: M[] = [];
  for (let i = 0; i < count; i++) {
    // Object property values are evaluated in source order, so the reads stay in byte order.
    ms.push({
      kind: r.u8(),
      name: r.str(),
      hash: r.u32(),
      rawLen: r.u32(),
      compLen: r.u32(),
      offset: r.u32(),
    });
  }

  const sections: Section[] = [];
  for (const m of ms) {
    if (m.offset > body.length || m.offset + m.compLen > body.length) {
      throw corruptErr("bundle: section body out of range");
    }
    const raw = lz4Decompress(data.subarray(m.offset, m.offset + m.compLen), m.rawLen);
    if (crc32Ieee(raw) !== m.hash) throw corruptErr("bundle: section content hash mismatch");
    if (m.kind === 0) {
      sections.push({ kind: 0, property: deserializeProperty(raw) });
    } else if (m.kind === 1 || m.kind === 2) {
      const [singles, contractions] = deserializeTable(raw);
      sections.push({ kind: m.kind, entries: { name: m.name, singles, contractions } });
    } else {
      throw corruptErr(`bundle: unknown section kind ${m.kind}`);
    }
  }
  return { unicodeVersion, cldrVersion, description, sections };
}

// loadBundle loads a bundle (README §5.1): the root section is a usable collation; each tailoring is
// merged onto the root (byte-identical to its fully-resolved .coll table). Every collation takes the
// bundle header's (unicode, cldr) version + description.
export function loadBundle(b: Bundle): {
  collations: Collation[];
  property: PropertyTable | undefined;
} {
  let root: BundleEntries | undefined;
  for (const s of b.sections) {
    if (s.kind === 1) {
      root = s.entries;
      break;
    }
  }
  const mk = (
    name: string,
    singles: SingleEntry[],
    contractions: ContractionEntry[],
  ): Collation => ({
    name,
    unicodeVersion: b.unicodeVersion,
    cldrVersion: b.cldrVersion,
    description: b.description,
    singles,
    contractions,
  });
  const collations: Collation[] = [];
  let property: PropertyTable | undefined;
  if (root) collations.push(mk(root.name, root.singles, root.contractions));
  for (const s of b.sections) {
    if (s.kind === 0) {
      property = s.property;
    } else if (s.kind === 2) {
      if (!root) throw corruptErr("bundle: tailoring without a root section");
      const [singles, contractions] = mergeOntoRoot(root, s.entries);
      collations.push(mk(s.entries.name, singles, contractions));
    }
  }
  return { collations, property };
}

// buildBundle builds a JUCD bundle (README §5) from a root collation, per-locale tailorings (each
// diffed against the root into a sparse override), and an optional property table — the builder
// tool's core. The header (unicode, cldr) version is the root's.
export function buildBundle(
  root: Collation,
  tailorings: Collation[],
  property: PropertyTable | undefined,
  description: string,
): Bundle {
  const sections: Section[] = [];
  if (property) sections.push({ kind: 0, property });
  sections.push({
    kind: 1,
    entries: { name: root.name, singles: root.singles, contractions: root.contractions },
  });
  for (const t of tailorings) {
    const [singles, contractions] = diffAgainstRoot(t, root);
    sections.push({ kind: 2, entries: { name: t.name, singles, contractions } });
  }
  return {
    unicodeVersion: root.unicodeVersion,
    cldrVersion: root.cldrVersion,
    description,
    sections,
  };
}

// mergeOntoRoot merges a tailoring's sparse override onto the root table (README §5.1): start from
// the root maps, replace-or-add each override by key, re-sort (ascending by code point / lexicographic
// by sequence — the §2 total order), so the result is byte-identical to the full .coll table.
function mergeOntoRoot(
  root: BundleEntries,
  delta: BundleEntries,
): [SingleEntry[], ContractionEntry[]] {
  const singleByCp = new Map<number, Ce[]>();
  for (const s of root.singles) singleByCp.set(s.cp, s.ces);
  for (const s of delta.singles) singleByCp.set(s.cp, s.ces);
  const singles: SingleEntry[] = [...singleByCp.entries()].map(([cp, ces]) => ({ cp, ces }));
  singles.sort((a, b) => a.cp - b.cp);

  const contrByKey = new Map<string, ContractionEntry>();
  for (const c of root.contractions) contrByKey.set(seqKey(c.seq), c);
  for (const c of delta.contractions) contrByKey.set(seqKey(c.seq), c);
  const contractions = [...contrByKey.values()];
  contractions.sort((a, b) => compareSeq(a.seq, b.seq));
  return [singles, contractions];
}

// diffAgainstRoot is the sparse override (README §5): the full table's singles/contractions that the
// root lacks or maps differently. The current LDML subset only adds or replaces (no removals), so
// applying this back onto the root reproduces the full table (§5.1).
function diffAgainstRoot(full: Collation, root: Collation): [SingleEntry[], ContractionEntry[]] {
  const rootSingles = new Map<number, Ce[]>();
  for (const s of root.singles) rootSingles.set(s.cp, s.ces);
  const singles = full.singles.filter((s) => {
    const rc = rootSingles.get(s.cp);
    return rc === undefined || !equalCes(rc, s.ces);
  });
  const rootContr = new Map<string, Ce[]>();
  for (const c of root.contractions) rootContr.set(seqKey(c.seq), c.ces);
  const contractions = full.contractions.filter((c) => {
    const rc = rootContr.get(seqKey(c.seq));
    return rc === undefined || !equalCes(rc, c.ces);
  });
  return [singles, contractions];
}

function equalCes(a: Ce[], b: Ce[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    const x = a[i];
    const y = b[i];
    if (x.flags !== y.flags || x.l1 !== y.l1 || x.l2 !== y.l2 || x.l3 !== y.l3) return false;
  }
  return true;
}

function compareSeq(a: number[], b: number[]): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i] - b[i];
  return a.length - b.length;
}

function serializeProperty(p: PropertyTable): Uint8Array {
  const out: number[] = [1]; // layout_version
  pushU32(out, p.simple.length);
  for (const s of p.simple) {
    pushU32(out, s.cp);
    pushU32(out, s.upper);
    pushU32(out, s.lower);
    pushU32(out, s.title);
  }
  pushU32(out, p.special.length);
  for (const sc of p.special) {
    pushU32(out, sc.cp);
    pushCps(out, sc.upper);
    pushCps(out, sc.lower);
    pushCps(out, sc.title);
  }
  return Uint8Array.from(out);
}

function pushCps(out: number[], cps: number[]): void {
  out.push(cps.length);
  for (const cp of cps) pushU32(out, cp);
}

function deserializeProperty(raw: Uint8Array): PropertyTable {
  const r = new Reader(raw);
  const layout = r.u8();
  if (layout !== 1) throw corruptErr(`bundle: unsupported property layout_version ${layout}`);
  const numSimple = r.u32();
  const simple: SimpleCase[] = [];
  for (let n = 0; n < numSimple; n++) {
    simple.push({ cp: r.u32(), upper: r.u32(), lower: r.u32(), title: r.u32() });
  }
  const numSpecial = r.u32();
  const special: SpecialCasing[] = [];
  for (let n = 0; n < numSpecial; n++) {
    special.push({ cp: r.u32(), upper: r.cps(), lower: r.cps(), title: r.cps() });
  }
  if (r.pos !== raw.length) throw corruptErr("bundle: trailing bytes after property table");
  return { simple, special };
}
