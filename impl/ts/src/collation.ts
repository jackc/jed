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
import { VENDORED_COLL } from "./collationdata/vendored.ts";

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
    if (toks[i].op === undefined) throw syntaxErr(`collation: expected a relation operator: ${line}`);
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
  for (const c of coll.contractions) if (c.seq.length > maxContraction) maxContraction = c.seq.length;
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
  const out: number[] = [];
  out.push(1); // layout_version
  pushU32(out, coll.singles.length);
  pushU32(out, coll.contractions.length);
  for (const s of coll.singles) {
    pushU32(out, s.cp);
    out.push(s.ces.length);
    for (const c of s.ces) pushCe(out, c);
  }
  for (const c of coll.contractions) {
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
  if (!(magic[0] === 0x4a && magic[1] === 0x43 && magic[2] === 0x4f && magic[3] === 0x4c && magic[4] === 0x4c && magic[5] === 0x00)) {
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

// --- Vendored collation set (spec/design/collation.md §2/§9) ------------------------------------
//
// In the reference-only model the engine reads collations from a set VENDORED into the binary, not
// from the database file (the file only references a collation by name + version). Production
// openCollations these embedded .coll artifacts once and serves every later use from them. This is
// the dev fixture set (dev-root, dev-nordic); the real version-pinned DUCET + curated tailorings and
// the embedder-chosen footprint tiers (§13) are later slices (§14, 2a/2f). The bytes are inlined as
// base64 (browser-safe — no node:fs) in the generated collationdata/vendored.ts, synced from
// spec/collation/fixtures by scripts/vendor_collations.rb and byte-identical across cores (§9/§10).

let vendoredCache: Map<string, Collation> | null = null;

function vendored(): Map<string, Collation> {
  if (vendoredCache === null) {
    vendoredCache = new Map();
    for (const b64 of Object.values(VENDORED_COLL)) {
      const bytes = Uint8Array.from(atob(b64), (ch) => ch.charCodeAt(0));
      const coll = openCollation(bytes);
      vendoredCache.set(coll.name, coll);
    }
  }
  return vendoredCache;
}

// vendoredCollation looks up a collation VENDORED into this binary by its exact (case-sensitive)
// name (spec/design/collation.md §2/§9). undefined ⇒ not vendored. "C" is never here (table-free,
// built in). The resolver consults the database's referenced collations first, then this set.
export function vendoredCollation(name: string): Collation | undefined {
  return vendored().get(name);
}

// vendoredCollations returns every vendored collation, ascending by name — a deterministic order
// with no hash-iteration leak (CLAUDE.md §8). Used by introspection (db.collations).
export function vendoredCollations(): Collation[] {
  return [...vendored().values()].sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
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
    for (let j = 0; j < n; j++) ces.push({ flags: this.u8(), l1: this.u16(), l2: this.u16(), l3: this.u16() });
    return ces;
  }
  str(): string {
    const n = this.u16();
    return new TextDecoder().decode(this.take(n));
  }
}
