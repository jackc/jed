// Time zones: the JTZ bundle codec + an RFC 8536 TZif reader + the engine-global loaded zone set.
// The cross-core contract for time-zone conversion (spec/design/timezones.md §4/§5): the reader is
// hand-written per core (CLAUDE.md §5) and byte-identical given identical input — it reads the
// standardized TZif layout, so cores agree by construction (§3.4). Byte formats: spec/tz/README.md.
// Mirrors impl/rust/src/timezone.rs and impl/go/timezone.go. i64 instants are bigint (JS numbers are
// f64); TZif bytes are read big-endian via DataView.

import { engineError } from "./errors.ts";
import { crc32Ieee } from "./format.ts";
import { lz4Compress, lz4Decompress } from "./lz4.ts";
import { civilFromDays, daysFromCivil } from "./timestamp.ts";

const SECS_PER_DAY = 86_400n;

function corruptErr(msg: string): Error {
  return engineError("data_corrupted", msg);
}

function floorDivBig(a: bigint, b: bigint): bigint {
  let q = a / b;
  if (a % b !== 0n && a < 0n !== b < 0n) q -= 1n;
  return q;
}

// --- in-memory zone representation (the parsed TZif file, spec/tz/README.md §2) ---

export interface LocalTimeType {
  utoff: number; // seconds east of UT
  isDst: boolean;
  abbrev: string;
}

export interface TzData {
  trans: bigint[]; // UT seconds of each transition, ascending
  transType: number[]; // index into types[]
  types: LocalTimeType[];
  footer?: PosixTz; // governs instants at/after the last transition (§5)
}

export interface Zone {
  name: string;
  tzdataVersion: string;
  data: TzData;
}

export interface Offset {
  utoff: number;
  abbrev: string;
  isDst: boolean;
}

export type ZoneRef = { fixed: true; off: number } | { fixed: false; zone: Zone };

export interface TimeZoneInfo {
  name: string;
  tzdataVersion: string;
}

// --- the POSIX TZ footer rule (spec/tz/README.md §5) ---

// Mm.w.d only (§5): month 1–12, week 1–5 (5 = last), day 0–6 (0 = Sunday). Jn / n are deferred.
export interface PosixRule {
  m: number;
  w: number;
  d: number;
}

export interface PosixDst {
  abbr: string;
  utoff: number; // east-positive
  start: PosixRule;
  startTime: number;
  end: PosixRule;
  endTime: number;
}

export interface PosixTz {
  stdAbbr: string;
  stdUtoff: number; // east-positive
  dst?: PosixDst;
}

interface Cur {
  s: string;
  i: number;
}

// parsePosixTz parses a POSIX TZ string (§5): std offset[dst[offset][,start[/time],end[/time]]].
function parsePosixTz(s: string): PosixTz {
  const c: Cur = { s, i: 0 };
  const stdAbbr = parsePosixAbbr(c);
  if (stdAbbr === undefined) throw corruptErr("posix: missing std abbreviation");
  const stdPosix = parsePosixOffset(c);
  if (stdPosix === undefined) throw corruptErr("posix: missing std offset");
  const stdUtoff = -stdPosix;

  if (c.i >= c.s.length) return { stdAbbr, stdUtoff };

  const abbr = parsePosixAbbr(c);
  if (abbr === undefined) throw corruptErr("posix: malformed dst abbreviation");
  let dstUtoff: number;
  if (c.i < c.s.length && c.s[c.i] !== ",") {
    const p = parsePosixOffset(c);
    if (p === undefined) throw corruptErr("posix: malformed dst offset");
    dstUtoff = -p;
  } else {
    dstUtoff = stdUtoff + 3600;
  }

  if (c.i >= c.s.length || c.s[c.i] !== ",")
    throw corruptErr("posix: dst without transition rules");
  c.i++;
  const [start, startTime] = parsePosixRule(c);
  if (c.i >= c.s.length || c.s[c.i] !== ",") throw corruptErr("posix: missing dst end rule");
  c.i++;
  const [end, endTime] = parsePosixRule(c);
  return { stdAbbr, stdUtoff, dst: { abbr, utoff: dstUtoff, start, startTime, end, endTime } };
}

function parsePosixAbbr(c: Cur): string | undefined {
  if (c.i < c.s.length && c.s[c.i] === "<") {
    c.i++;
    const start = c.i;
    while (c.i < c.s.length && c.s[c.i] !== ">") c.i++;
    if (c.i >= c.s.length) return undefined;
    const out = c.s.slice(start, c.i);
    c.i++; // consume '>'
    return out === "" ? undefined : out;
  }
  const start = c.i;
  while (c.i < c.s.length && isAsciiAlpha(c.s.charCodeAt(c.i))) c.i++;
  return c.i === start ? undefined : c.s.slice(start, c.i);
}

// parsePosixOffset parses [+|-]hh[:mm[:ss]] → seconds (POSIX raw, west-positive). Caller negates.
function parsePosixOffset(c: Cur): number | undefined {
  let neg = false;
  if (c.i < c.s.length && (c.s[c.i] === "+" || c.s[c.i] === "-")) {
    neg = c.s[c.i] === "-";
    c.i++;
  }
  const hh = parseUint(c);
  if (hh === undefined) return undefined;
  let secs = hh * 3600;
  if (c.i < c.s.length && c.s[c.i] === ":") {
    c.i++;
    const mm = parseUint(c);
    if (mm === undefined) return undefined;
    secs += mm * 60;
    if (c.i < c.s.length && c.s[c.i] === ":") {
      c.i++;
      const ss = parseUint(c);
      if (ss === undefined) return undefined;
      secs += ss;
    }
  }
  return neg ? -secs : secs;
}

// parsePosixRule parses Mm.w.d with an optional /time (§5). Jn / n are a deferred error.
function parsePosixRule(c: Cur): [PosixRule, number] {
  if (c.i >= c.s.length) throw corruptErr("posix: missing transition rule");
  if (c.s[c.i] !== "M") {
    throw corruptErr("posix: Jn/n julian-day transition rules are not yet supported");
  }
  c.i++;
  const m = parseUint(c);
  if (m === undefined || c.s[c.i] !== ".") throw corruptErr("posix: bad month");
  c.i++;
  const w = parseUint(c);
  if (w === undefined || c.s[c.i] !== ".") throw corruptErr("posix: bad week");
  c.i++;
  const d = parseUint(c);
  if (d === undefined) throw corruptErr("posix: bad day");
  if (m < 1 || m > 12 || w < 1 || w > 5 || d > 6) throw corruptErr("posix: Mm.w.d out of range");
  let time = 7200;
  if (c.i < c.s.length && c.s[c.i] === "/") {
    c.i++;
    const t = parsePosixOffset(c);
    if (t === undefined) throw corruptErr("posix: bad transition time");
    time = t;
  }
  return [{ m, w, d }, time];
}

function parseUint(c: Cur): number | undefined {
  const start = c.i;
  let v = 0;
  while (c.i < c.s.length && c.s.charCodeAt(c.i) >= 48 && c.s.charCodeAt(c.i) <= 57) {
    v = v * 10 + (c.s.charCodeAt(c.i) - 48);
    c.i++;
  }
  return c.i === start ? undefined : v;
}

function isAsciiAlpha(ch: number): boolean {
  return (ch >= 97 && ch <= 122) || (ch >= 65 && ch <= 90);
}

// --- the reader (spec/tz/README.md §4) ---

// mwdDay is the day-of-month of the w-th weekday d (0=Sun) of month m in year (w==5 = last).
function mwdDay(year: bigint, m: number, w: number, d: number): bigint {
  const mm = BigInt(m);
  const first = daysFromCivil(year, mm, 1n);
  const firstDow = floorModBig(first + 4n, 7n); // 0 = Sunday (1970-01-01 was Thursday)
  const offset = floorModBig(BigInt(d) - firstDow, 7n);
  let day = 1n + offset + BigInt(w - 1) * 7n;
  const next = m === 12 ? daysFromCivil(year + 1n, 1n, 1n) : daysFromCivil(year, mm + 1n, 1n);
  const dim = next - first;
  if (day > dim) day -= 7n;
  return day;
}

function floorModBig(a: bigint, b: bigint): bigint {
  let m = a % b;
  if (m < 0n) m += b;
  return m;
}

// ruleInstant is the UT seconds of a footer transition: the rule at localTime in year, using the
// offset in effect just before the transition (utoffBefore).
function ruleInstant(
  rule: PosixRule,
  localTime: number,
  year: bigint,
  utoffBefore: number,
): bigint {
  const day = mwdDay(year, rule.m, rule.w, rule.d);
  const localEpoch = daysFromCivil(year, BigInt(rule.m), day) * SECS_PER_DAY + BigInt(localTime);
  return localEpoch - BigInt(utoffBefore);
}

function evalPosix(tz: PosixTz, instantSecs: bigint): Offset {
  if (tz.dst === undefined) {
    return { utoff: tz.stdUtoff, abbrev: tz.stdAbbr, isDst: false };
  }
  const d = tz.dst;
  const [year] = civilFromDays(floorDivBig(instantSecs, SECS_PER_DAY));
  const startUt = ruleInstant(d.start, d.startTime, year, tz.stdUtoff);
  const endUt = ruleInstant(d.end, d.endTime, year, d.utoff);
  let inDst: boolean;
  if (startUt < endUt) {
    inDst = instantSecs >= startUt && instantSecs < endUt;
  } else {
    // Southern hemisphere: the DST interval wraps the year boundary.
    inDst = instantSecs >= startUt || instantSecs < endUt;
  }
  return inDst
    ? { utoff: d.utoff, abbrev: d.abbr, isDst: true }
    : { utoff: tz.stdUtoff, abbrev: tz.stdAbbr, isDst: false };
}

function firstStdOffset(data: TzData): Offset {
  for (const t of data.types) {
    if (!t.isDst) return { utoff: t.utoff, abbrev: t.abbrev, isDst: false };
  }
  if (data.types.length > 0) {
    const t = data.types[0];
    return { utoff: t.utoff, abbrev: t.abbrev, isDst: t.isDst };
  }
  return { utoff: 0, abbrev: "", isDst: false };
}

// offsetAt is the reader (§4): the local-time type in effect at instantSecs (UT seconds). Pure/total.
export function offsetAt(data: TzData, instantSecs: bigint): Offset {
  const n = data.trans.length;
  if (n === 0) {
    return data.footer !== undefined ? evalPosix(data.footer, instantSecs) : firstStdOffset(data);
  }
  if (instantSecs < data.trans[0]) return firstStdOffset(data);
  if (data.footer !== undefined && instantSecs >= data.trans[n - 1]) {
    return evalPosix(data.footer, instantSecs);
  }
  // largest i with trans[i] <= instantSecs (binary search)
  let lo = 0;
  let hi = n;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (data.trans[mid] <= instantSecs) lo = mid + 1;
    else hi = mid;
  }
  const t = data.types[data.transType[lo - 1]];
  return { utoff: t.utoff, abbrev: t.abbrev, isDst: t.isDst };
}

// --- TZif parsing (RFC 8536 / spec/tz/README.md §2) ---

class TzReader {
  b: Uint8Array;
  dv: DataView;
  pos = 0;
  constructor(b: Uint8Array) {
    this.b = b;
    this.dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  }
  take(n: number): Uint8Array {
    if (this.pos + n > this.b.length) throw corruptErr("tz: unexpected end of input");
    const s = this.b.subarray(this.pos, this.pos + n);
    this.pos += n;
    return s;
  }
  skip(n: number): void {
    if (this.pos + n > this.b.length) throw corruptErr("tz: unexpected end of input");
    this.pos += n;
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
  i32(): number {
    const v = this.dv.getInt32(this.pos, false);
    this.pos += 4;
    return v;
  }
  i64(): bigint {
    const v = this.dv.getBigInt64(this.pos, false);
    this.pos += 8;
    return v;
  }
  str(): string {
    const n = this.u16();
    return new TextDecoder().decode(this.take(n));
  }
}

interface TzCounts {
  isutcnt: number;
  isstdcnt: number;
  leapcnt: number;
  timecnt: number;
  typecnt: number;
  charcnt: number;
}

function readTzHeader(r: TzReader): [number, TzCounts] {
  const magic = r.take(4);
  if (magic[0] !== 0x54 || magic[1] !== 0x5a || magic[2] !== 0x69 || magic[3] !== 0x66) {
    throw corruptErr("tzif: bad magic");
  }
  const version = r.u8();
  r.skip(15);
  const counts: TzCounts = {
    isutcnt: r.u32(),
    isstdcnt: r.u32(),
    leapcnt: r.u32(),
    timecnt: r.u32(),
    typecnt: r.u32(),
    charcnt: r.u32(),
  };
  return [version, counts];
}

function tzBlockSize(c: TzCounts, timeSize: number): number {
  return (
    c.timecnt * timeSize +
    c.timecnt +
    c.typecnt * 6 +
    c.charcnt +
    c.leapcnt * (timeSize + 4) +
    c.isstdcnt +
    c.isutcnt
  );
}

function abbrevAt(desig: Uint8Array, idx: number): string {
  if (idx > desig.length) throw corruptErr("tzif: designation index out of range");
  let end = idx;
  while (end < desig.length && desig[end] !== 0) end++;
  return new TextDecoder().decode(desig.subarray(idx, end));
}

function readTzBlock(r: TzReader, timeSize: number, c: TzCounts): TzData {
  const trans: bigint[] = [];
  for (let i = 0; i < c.timecnt; i++) trans.push(timeSize === 8 ? r.i64() : BigInt(r.i32()));
  const transType: number[] = [];
  for (let i = 0; i < c.timecnt; i++) transType.push(r.u8());
  const raws: { utoff: number; isDst: boolean; idx: number }[] = [];
  for (let i = 0; i < c.typecnt; i++) {
    raws.push({ utoff: r.i32(), isDst: r.u8() !== 0, idx: r.u8() });
  }
  const desig = r.take(c.charcnt).slice(); // copy (the footer read needs the slice intact)
  // leap seconds (occ width = timeSize, corr = 4) + std/wall + ut/local — skipped (§2).
  r.skip(c.leapcnt * (timeSize + 4));
  r.skip(c.isstdcnt);
  r.skip(c.isutcnt);
  const types: LocalTimeType[] = raws.map((raw) => ({
    utoff: raw.utoff,
    isDst: raw.isDst,
    abbrev: abbrevAt(desig, raw.idx),
  }));
  if (types.length === 0) throw corruptErr("tzif: no local time types");
  for (const t of transType) {
    if (t >= types.length) throw corruptErr("tzif: transition type index out of range");
  }
  return { trans, transType, types };
}

// parseTzif parses a TZif file (§2): a v1 block is read directly; a v2+ file skips the v1 block,
// reads the 64-bit block, then the POSIX footer. A malformed file is XX001.
export function parseTzif(data: Uint8Array): TzData {
  const r = new TzReader(data);
  const [version, c1] = readTzHeader(r);
  if (version === 0) return readTzBlock(r, 4, c1);
  r.skip(tzBlockSize(c1, 4));
  const [, c2] = readTzHeader(r);
  const td = readTzBlock(r, 8, c2);
  td.footer = readTzFooter(r);
  return td;
}

function readTzFooter(r: TzReader): PosixTz | undefined {
  const rest = r.b.subarray(Math.min(r.pos, r.b.length));
  const s = new TextDecoder().decode(rest).replace(/^\n+|\n+$/g, "");
  return s === "" ? undefined : parsePosixTz(s);
}

// --- the JTZ bundle codec (spec/tz/README.md §3) ---

export interface TzZoneSection {
  name: string;
  raw: Uint8Array;
}

export interface TzLink {
  alias: string;
  target: string;
}

export interface TzBundle {
  tzdataVersion: string;
  description: string;
  zones: TzZoneSection[]; // ascending by name
  links: TzLink[]; // ascending by alias
}

const TZ_BUNDLE_MAGIC = [0x4a, 0x54, 0x5a, 0x00, 0x00, 0x00]; // "JTZ\0\0\0"

function pushStr(out: number[], s: string): void {
  const bytes = new TextEncoder().encode(s);
  out.push((bytes.length >> 8) & 0xff, bytes.length & 0xff);
  for (const b of bytes) out.push(b);
}

function pushU16(out: number[], v: number): void {
  out.push((v >> 8) & 0xff, v & 0xff);
}

function pushU32(out: number[], v: number): void {
  out.push((v >>> 24) & 0xff, (v >>> 16) & 0xff, (v >>> 8) & 0xff, v & 0xff);
}

// saveTzBundle serializes a JTZ bundle (README §3).
export function saveTzBundle(b: TzBundle): Uint8Array {
  const packed = b.zones.map((z) => ({
    name: z.name,
    hash: crc32Ieee(z.raw),
    rawLen: z.raw.length,
    comp: lz4Compress(z.raw),
  }));

  const header: number[] = [...TZ_BUNDLE_MAGIC];
  pushU16(header, 1); // format_version
  pushStr(header, b.tzdataVersion);
  pushStr(header, b.description);
  pushU16(header, packed.length);
  pushU16(header, b.links.length);

  let zoneManifestLen = 0;
  for (const p of packed) zoneManifestLen += 2 + new TextEncoder().encode(p.name).length + 16;
  let linkLen = 0;
  for (const l of b.links) {
    linkLen +=
      2 + new TextEncoder().encode(l.alias).length + 2 + new TextEncoder().encode(l.target).length;
  }
  const bodyStart = header.length + zoneManifestLen + linkLen;

  const manifest: number[] = [];
  let off = bodyStart;
  for (const p of packed) {
    pushStr(manifest, p.name);
    pushU32(manifest, p.hash);
    pushU32(manifest, p.rawLen);
    pushU32(manifest, p.comp.length);
    pushU32(manifest, off);
    off += p.comp.length;
  }
  for (const l of b.links) {
    pushStr(manifest, l.alias);
    pushStr(manifest, l.target);
  }

  const out: number[] = [...header, ...manifest];
  for (const p of packed) for (const byte of p.comp) out.push(byte);
  const crc = crc32Ieee(Uint8Array.from(out));
  pushU32(out, crc);
  return Uint8Array.from(out);
}

// openTzBundle reads a JTZ bundle (README §3), verifying the CRC, magic, format, and each zone's hash.
export function openTzBundle(data: Uint8Array): TzBundle {
  if (data.length < 4) throw corruptErr("tz bundle: truncated");
  const body = data.subarray(0, data.length - 4);
  const want =
    ((data[data.length - 4] << 24) |
      (data[data.length - 3] << 16) |
      (data[data.length - 2] << 8) |
      data[data.length - 1]) >>>
    0;
  if (crc32Ieee(body) !== want) throw corruptErr("tz bundle: trailer checksum mismatch");

  const r = new TzReader(data);
  const magic = r.take(6);
  if (!TZ_BUNDLE_MAGIC.every((v, i) => magic[i] === v)) throw corruptErr("tz bundle: bad magic");
  const fmt = r.u16();
  if (fmt !== 1) throw corruptErr(`tz bundle: unsupported format_version ${fmt}`);
  const tzdataVersion = r.str();
  const description = r.str();
  const zoneCount = r.u16();
  const linkCount = r.u16();

  const metas: { name: string; hash: number; rawLen: number; compLen: number; offset: number }[] =
    [];
  for (let i = 0; i < zoneCount; i++) {
    metas.push({
      name: r.str(),
      hash: r.u32(),
      rawLen: r.u32(),
      compLen: r.u32(),
      offset: r.u32(),
    });
  }
  const links: TzLink[] = [];
  for (let i = 0; i < linkCount; i++) links.push({ alias: r.str(), target: r.str() });

  const zones: TzZoneSection[] = metas.map((m) => {
    if (m.offset > body.length || m.offset + m.compLen > body.length) {
      throw corruptErr("tz bundle: section body out of range");
    }
    const raw = lz4Decompress(data.subarray(m.offset, m.offset + m.compLen), m.rawLen);
    if (crc32Ieee(raw) !== m.hash) throw corruptErr("tz bundle: section content hash mismatch");
    return { name: m.name, raw };
  });
  return { tzdataVersion, description, zones, links };
}

// --- the engine-global loaded zone set + the load seam (timezones.md §3.3) ---

const loadedTz = new Map<string, Zone>();

// loadTimeZoneData loads a JTZ bundle into the engine-global loaded set (§3.3/§4): parse each zone's
// TZif, register by name, then resolve each link alias onto its target's tables. ADDITIVE /
// FIRST-WINS — a name already present is not replaced (idempotent re-load). A malformed bundle (or
// TZif) is XX001. The engine primitive behind db.loadTimeZoneData; may be called before opening any
// file, reads no path, reaches no host data (§10). Browser-safe (Uint8Array, no node:fs).
export function loadTimeZoneData(data: Uint8Array): void {
  const bundle = openTzBundle(data);
  const parsed = new Map<string, Zone>();
  for (const z of bundle.zones) {
    parsed.set(z.name, {
      name: z.name,
      tzdataVersion: bundle.tzdataVersion,
      data: parseTzif(z.raw),
    });
  }
  for (const [name, z] of parsed) {
    if (!loadedTz.has(name)) loadedTz.set(name, z);
  }
  for (const l of bundle.links) {
    const target = parsed.get(l.target);
    if (target !== undefined && !loadedTz.has(l.alias)) {
      loadedTz.set(l.alias, {
        name: l.alias,
        tzdataVersion: bundle.tzdataVersion,
        data: target.data,
      });
    }
  }
}

// loadedZone looks up a loaded named zone by exact name (undefined ⇒ no loaded bundle provides it).
export function loadedZone(name: string): Zone | undefined {
  return loadedTz.get(name);
}

// loadedTimeZones introspects the engine-global loaded zone set (db.loadedTimeZones, §3.3): every
// zone + alias a loaded bundle provides, ascending by name.
export function loadedTimeZones(): TimeZoneInfo[] {
  return [...loadedTz.keys()]
    .sort()
    .map((name) => ({ name, tzdataVersion: loadedTz.get(name)!.tzdataVersion }));
}

// resolveZone resolves a zone name to a built-in fixed offset (UTC / ±HH[:MM[:SS]], §3.2) or a loaded
// named zone. undefined ⇒ unknown (the caller raises 22023).
export function resolveZone(name: string): ZoneRef | undefined {
  if (name === "UTC") return { fixed: true, off: 0 };
  const off = parseFixedOffset(name);
  if (off !== undefined) return { fixed: true, off };
  const z = loadedZone(name);
  return z !== undefined ? { fixed: false, zone: z } : undefined;
}

// offsetAtRef is the offset in effect at instantSecs for a resolved zone reference.
export function offsetAtRef(zr: ZoneRef, instantSecs: bigint): Offset {
  if (zr.fixed) return { utoff: zr.off, abbrev: fixedAbbrev(zr.off), isDst: false };
  return offsetAt(zr.zone.data, instantSecs);
}

// instantToLocalMicros is timestamptz AT TIME ZONE zone (§4): local = instant + utoff.
export function instantToLocalMicros(zr: ZoneRef, instantMicros: bigint): bigint {
  const off = offsetAtRef(zr, floorDivBig(instantMicros, 1_000_000n));
  return instantMicros + BigInt(off.utoff) * 1_000_000n;
}

// localToInstantMicros is timestamp AT TIME ZONE zone (§4): instant = wall − utoff. Two-probe
// resolution; at a DST gap/overlap the branch matches PostgreSQL (oracle-pinned, timezones.md §6).
export function localToInstantMicros(zr: ZoneRef, wallMicros: bigint): bigint {
  const wallSecs = floorDivBig(wallMicros, 1_000_000n);
  const off1 = BigInt(offsetAtRef(zr, wallSecs).utoff);
  const off2 = BigInt(offsetAtRef(zr, wallSecs - off1).utoff);
  return wallMicros - off2 * 1_000_000n;
}

// parseFixedOffset parses [+|-]HH[:MM[:SS]] (the WHOLE string). Requires a leading sign. POSIX sign
// (positive = WEST), matching PG's AT TIME ZONE '+05:30' (= UTC−5:30 — oracle-pinned), so the
// east-positive utoff is the negation of the written value. undefined ⇒ not a fixed offset.
function parseFixedOffset(name: string): number | undefined {
  if (name.length === 0 || (name[0] !== "+" && name[0] !== "-")) return undefined;
  const c: Cur = { s: name, i: 0 };
  const posix = parsePosixOffset(c);
  if (posix === undefined || c.i !== name.length) return undefined;
  return -posix;
}

function fixedAbbrev(utoff: number): string {
  if (utoff === 0) return "UTC";
  const sign = utoff < 0 ? "-" : "+";
  const a = Math.abs(utoff);
  const h = Math.floor(a / 3600);
  const m = Math.floor((a % 3600) / 60);
  const s = a % 60;
  const p2 = (n: number) => n.toString().padStart(2, "0");
  return s === 0 ? `${sign}${p2(h)}:${p2(m)}` : `${sign}${p2(h)}:${p2(m)}:${p2(s)}`;
}
