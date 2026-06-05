// A deliberately tiny TOML reader for the cross-check tests ONLY (mirrors Go's
// tomlmini_test.go). It understands just enough of the spec tables' shape — arrays of
// tables (`[[type]]`), scalar key = value pairs, inline string arrays, and one level of
// inline table (read via dotted access) — plus a dedicated scanner for the encoding
// fixtures' inline-table case arrays. It is NOT a general TOML parser; keeping it
// dependency-free preserves the "TOML is test-time only, no runtime dependency" rule
// (CLAUDE.md §5). Not a `*.test.ts` file, so the test runner does not execute it.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";

// specPath resolves a path under spec/ by walking up from this test directory.
export function specPath(rel: string): string {
  let dir = import.meta.dirname; // .../impl/ts/tests
  for (;;) {
    const candidate = join(dir, "spec", rel);
    try {
      readFileSync(candidate);
      return candidate;
    } catch {
      const parent = dirname(dir);
      if (parent === dir) throw new Error(`could not locate spec/${rel}`);
      dir = parent;
    }
  }
}

function stripComment(s: string): string {
  let inStr = false;
  for (let i = 0; i < s.length; i++) {
    const ch = s[i];
    if (ch === '"') inStr = !inStr;
    else if (ch === "#" && !inStr) return s.slice(0, i);
  }
  return s;
}

function unquote(s: string): string {
  const t = s.trim();
  if (t.length >= 2 && t.startsWith('"') && t.endsWith('"')) return t.slice(1, -1);
  return t;
}

function parseInlineStringArray(s: string): string[] {
  const inner = s.trim().replace(/^\[/, "").replace(/\]$/, "");
  return inner
    .split(",")
    .map((p) => p.trim())
    .filter((p) => p.length > 0)
    .map(unquote);
}

function parseInlineTable(s: string): Map<string, string> {
  const inner = s.trim().replace(/^\{/, "").replace(/\}$/, "");
  const out = new Map<string, string>();
  for (const part of inner.split(",")) {
    const idx = part.indexOf("=");
    if (idx < 0) continue;
    out.set(part.slice(0, idx).trim(), unquote(part.slice(idx + 1).trim()));
  }
  return out;
}

// TomlRow is one `[[section]]` entry's directly-nested keys.
export class TomlRow {
  private vals = new Map<string, string>();
  private arrVals = new Map<string, string[]>();

  set(key: string, val: string): void {
    this.vals.set(key, val);
  }
  setArr(key: string, val: string[]): void {
    this.arrVals.set(key, val);
  }

  str(key: string): string {
    const v = this.vals.get(key);
    if (v === undefined) throw new Error(`missing key ${key}`);
    return v;
  }
  big(key: string): bigint {
    return BigInt(this.str(key));
  }
  num(key: string): number {
    return Number(this.str(key));
  }
  strs(key: string): string[] {
    return this.arrVals.get(key) ?? [];
  }
  has(key: string): boolean {
    return this.vals.has(key);
  }
  bool(key: string): boolean {
    return this.str(key) === "true";
  }
}

// readTomlTables parses every `[[section]]` array-of-tables entry from a TOML file.
export function readTomlTables(path: string, section: string): TomlRow[] {
  const data = readFileSync(path, "utf8");
  const rows: TomlRow[] = [];
  let cur: TomlRow | null = null;
  const header = `[[${section}]]`;

  for (const raw of data.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    if (line === header) {
      cur = new TomlRow();
      rows.push(cur);
      continue;
    }
    if (line.startsWith("[[") || line.startsWith("[")) {
      cur = null; // a different section starts
      continue;
    }
    if (cur === null) continue;
    const idx = line.indexOf("=");
    if (idx < 0) continue;
    const key = line.slice(0, idx).trim();
    const val = stripComment(line.slice(idx + 1)).trim();
    if (val.startsWith("[")) {
      cur.setArr(key, parseInlineStringArray(val));
    } else if (val.startsWith("{")) {
      for (const [k, v] of parseInlineTable(val)) cur.set(`${key}.${k}`, v);
    } else {
      cur.set(key, unquote(val));
    }
  }
  return rows;
}

// EncCase is one encoding fixture row (spec/encoding/integers.toml).
export type EncCase = {
  kind: "bare" | "nullable" | "descending";
  typ: string;
  value: bigint;
  strValue: string; // a quoted value (uuid's canonical string); "" for integer cases
  isNull: boolean;
  bytes: string;
};

// readEncodingCases scans the inline-table case rows under each [[bare]] / [[nullable]]
// / [[descending]] group (the tiny reader above captures scalar keys but not these
// nested inline-table arrays).
export function readEncodingCases(path: string): EncCase[] {
  const data = readFileSync(path, "utf8");
  const out: EncCase[] = [];
  let kind: EncCase["kind"] | "" = "";
  let typ = "";
  for (const raw of data.split("\n")) {
    const line = raw.trim();
    if (line === "[[bare]]") {
      kind = "bare";
      typ = "";
    } else if (line === "[[nullable]]") {
      kind = "nullable";
      typ = "";
    } else if (line === "[[descending]]") {
      kind = "descending";
      typ = "";
    } else if (line.startsWith("type =")) {
      typ = unquote(stripComment(line.slice("type =".length)).trim());
    } else if (line.startsWith("{")) {
      const c = parseEncCaseLine(line, kind, typ);
      if (c) out.push(c);
    }
  }
  return out;
}

// A timestamp vector row (spec/encoding/timestamps.toml). `fields` holds the inline-table
// keys (input / micros / error / text) with surrounding quotes already stripped.
export type TsCase = {
  section: "parse" | "parse_error" | "render";
  typ: string;
  fields: Record<string, string>;
};

// readTimestampCases scans the inline-table case rows under each [[parse]] / [[parse_error]]
// / [[render]] group (the tiny reader above does not capture nested case arrays).
export function readTimestampCases(path: string): TsCase[] {
  const data = readFileSync(path, "utf8");
  const out: TsCase[] = [];
  let section: TsCase["section"] | "" = "";
  let typ = "";
  for (const raw of data.split("\n")) {
    const line = raw.trim();
    if (line === "[[parse]]") {
      section = "parse";
      typ = "";
    } else if (line === "[[parse_error]]") {
      section = "parse_error";
      typ = "";
    } else if (line === "[[render]]") {
      section = "render";
      typ = "";
    } else if (line.startsWith("type =")) {
      typ = unquote(stripComment(line.slice("type =".length)).trim());
    } else if (line.startsWith("{") && section !== "" && typ !== "") {
      let inner = line;
      const o = inner.indexOf("{");
      if (o >= 0) inner = inner.slice(o + 1);
      const cl = inner.indexOf("}");
      if (cl >= 0) inner = inner.slice(0, cl);
      const fields: Record<string, string> = {};
      for (const part of inner.split(",")) {
        const idx = part.indexOf("=");
        if (idx >= 0) fields[part.slice(0, idx).trim()] = unquote(part.slice(idx + 1).trim());
      }
      out.push({ section, typ, fields });
    }
  }
  return out;
}

function parseEncCaseLine(line: string, kind: EncCase["kind"] | "", typ: string): EncCase | null {
  if (kind === "" || typ === "") return null;
  let inner = line;
  const o = inner.indexOf("{");
  if (o >= 0) inner = inner.slice(o + 1);
  const cl = inner.indexOf("}");
  if (cl >= 0) inner = inner.slice(0, cl);
  let value = 0n;
  let strValue = "";
  let isNull = false;
  let bytes = "";
  for (const part of inner.split(",")) {
    const idx = part.indexOf("=");
    if (idx < 0) continue;
    const k = part.slice(0, idx).trim();
    const v = part.slice(idx + 1).trim();
    // A quoted value is a uuid's canonical string; an unquoted one is an integer.
    if (k === "value") {
      if (v.startsWith('"')) strValue = unquote(v);
      else value = BigInt(v);
    } else if (k === "null") isNull = v === "true";
    else if (k === "bytes") bytes = unquote(v);
  }
  if (bytes === "") return null;
  return { kind, typ, value, strValue, isNull, bytes };
}
