// JSON document types (spec/design/json.md): `json` (validated, stored verbatim as text) and
// `jsonb` (parsed, canonicalized, stored as a compact tagged-node tree). Numbers are exact
// `Decimal` (PG `numeric`, never binary float — CLAUDE.md §8); strings are UTF-8 `text`; `jsonb`
// objects keep their keys in a canonical sorted order (length-then-bytewise) with duplicates
// resolved last-wins, so the in-memory tree and the on-disk bytes are a pure function of the value
// (no hashmap-iteration-order leak — §2.3).
//
// Hand-written per CLAUDE.md §5 (a recursive tree codec/comparator/parser is irreducibly
// per-language), cross-checked across cores by the conformance corpus + golden fixtures. The
// TS-core hazards (CLAUDE.md §2): JSON numbers become the exact `Decimal` via decimalFromParts /
// Decimal.fromDigitsScale — NEVER parseFloat/Number (JS numbers are f64); object-key sort and string
// comparison are over UTF-8 BYTES (a JS string is UTF-16, so we compare its TextEncoder bytes, the
// same C-collation byte order text uses), and the parser iterates over UTF-8 bytes.

import { Decimal, EXP_LIMIT, decimalFromParts } from "./decimal.ts";
import { type EngineError, engineError } from "./errors.ts";

// JsonNode is a `jsonb` node — the in-memory canonical tree (spec/design/json.md §2). Object members
// are kept in canonical key order (shorter key first, ties bytewise) with duplicates removed
// (last-wins), so the structural form IS the correct value-level equality (consistent with
// jsonNodeCmp == 0 — §5). JSON `null` is the concrete `null` node, wholly distinct from a SQL NULL
// `jsonb` value. Modeled as a discriminated union (keyed on `kind`, like Value); free-function
// helpers below, never methods (the boring/explicit style — CLAUDE.md §10).
export type JsonNode =
  | { kind: "null" }
  | { kind: "bool"; value: boolean }
  // A JSON number, held EXACTLY as a Decimal (PG numeric); no binary float ever appears.
  | { kind: "number"; dec: Decimal }
  | { kind: "string"; value: string }
  | { kind: "array"; elements: JsonNode[] }
  // An object's members. For a `jsonb` node these are in canonical key order with unique keys (the
  // canonicalizer's invariant); a `json`-on-demand parse (§4) keeps input order + dupes.
  | { kind: "object"; members: JsonMember[] };

// JsonMember is one object member: a key string and its value node.
export interface JsonMember {
  key: string;
  value: JsonNode;
}

const UTF8 = new TextEncoder();

// utf8Bytes is the UTF-8 byte encoding of a string — the unit the canonical key sort and the jsonb
// string comparison operate over (NOT JS UTF-16 code units; CLAUDE.md §2 string hazard).
function utf8Bytes(s: string): Uint8Array {
  return UTF8.encode(s);
}

// byteCmp compares two byte arrays lexicographically (unsigned), <0/0/>0.
function byteCmp(a: Uint8Array, b: Uint8Array): number {
  const n = a.length < b.length ? a.length : b.length;
  for (let i = 0; i < n; i++) {
    if (a[i] !== b[i]) return a[i]! < b[i]! ? -1 : 1;
  }
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
}

// typeRank is the PG jsonb type-rank discriminator (spec/design/json.md §5): the outermost ordering
// key. Object > Array > Boolean > Number > String > Null.
function typeRank(n: JsonNode): number {
  switch (n.kind) {
    case "null":
      return 0;
    case "string":
      return 1;
    case "number":
      return 2;
    case "bool":
      return 3;
    case "array":
      return 4;
    case "object":
      return 5;
  }
}

// jsonNodeCmp is the PG jsonb total btree order (spec/design/json.md §5). A definite ordering (no
// SQL NULLs inside a document), driving both `<` and `ORDER BY` from one comparator so they agree by
// construction. Type rank first; within a type: booleans false<true, numbers by Decimal value,
// strings by collation-`C` UTF-8 byte order, arrays/objects by element/member COUNT first (PG
// compares container length before contents) then element-wise. Returns <0, 0, >0.
export function jsonNodeCmp(a: JsonNode, b: JsonNode): number {
  const ra = typeRank(a);
  const rb = typeRank(b);
  if (ra !== rb) return ra < rb ? -1 : 1;
  switch (a.kind) {
    case "null":
      return 0;
    case "bool": {
      const bb = b as { kind: "bool"; value: boolean };
      // false < true.
      return a.value === bb.value ? 0 : a.value ? 1 : -1;
    }
    case "number":
      return a.dec.cmpValue((b as { kind: "number"; dec: Decimal }).dec);
    case "string":
      return byteCmp(utf8Bytes(a.value), utf8Bytes((b as { kind: "string"; value: string }).value));
    case "array": {
      const be = (b as { kind: "array"; elements: JsonNode[] }).elements;
      if (a.elements.length !== be.length) return a.elements.length < be.length ? -1 : 1;
      for (let i = 0; i < a.elements.length; i++) {
        const c = jsonNodeCmp(a.elements[i]!, be[i]!);
        if (c !== 0) return c;
      }
      return 0;
    }
    case "object": {
      const bm = (b as { kind: "object"; members: JsonMember[] }).members;
      if (a.members.length !== bm.length) return a.members.length < bm.length ? -1 : 1;
      // Members are in canonical key order in both; compare keys then values pairwise.
      for (let i = 0; i < a.members.length; i++) {
        const kc = keyCmp(a.members[i]!.key, bm[i]!.key);
        if (kc !== 0) return kc;
        const vc = jsonNodeCmp(a.members[i]!.value, bm[i]!.value);
        if (vc !== 0) return vc;
      }
      return 0;
    }
  }
}

// keyCmp is the canonical object-key order (spec/design/json.md §2.3): shorter key first, ties broken
// bytewise (over UTF-8 BYTES) — PostgreSQL's jsonb key order. The canonicalizer sorts by this and the
// comparator compares keys by this.
export function keyCmp(a: string, b: string): number {
  const ba = utf8Bytes(a);
  const bb = utf8Bytes(b);
  if (ba.length !== bb.length) return ba.length < bb.length ? -1 : 1;
  return byteCmp(ba, bb);
}

// ---------------------------------------------------------------------------------------------
// Parsing (RFC 8259). `jsonbIn` canonicalizes; `validateJson` validates and the caller stores
// verbatim.
// ---------------------------------------------------------------------------------------------

function malformed(detail: string): Error {
  return engineError(
    "invalid_text_representation",
    "invalid input syntax for type json: " + detail,
  );
}

// jsonbIn parses + canonicalizes JSON text into a jsonb node tree (`jsonb_in` — spec/design/json.md
// §6.2): numbers → Decimal, object keys deduped last-wins then sorted length-then-bytewise.
// Malformed input → 22P02.
export function jsonbIn(input: string): JsonNode {
  return new JsonParser(UTF8.encode(input), true).parseDocument();
}

// validateJson validates JSON text well-formedness (`json_in` — spec/design/json.md §4); the caller
// stores the original bytes verbatim. Malformed input → 22P02.
export function validateJson(input: string): void {
  new JsonParser(UTF8.encode(input), false).parseDocument();
}

// parsePreservingJson parses JSON text into a node tree WITHOUT canonicalizing (object key order +
// duplicates preserved) — the on-demand structural parse a `json` operator needs
// (spec/design/json.md §4).
export function parsePreservingJson(input: string): JsonNode {
  return new JsonParser(UTF8.encode(input), false).parseDocument();
}

// utf8Len is the UTF-8 lead-byte length (1..4). A continuation/invalid lead byte returns 1 so the
// copy path's decode check rejects it.
function utf8Len(lead: number): number {
  if (lead < 0x80) return 1;
  if (lead >>> 5 === 0b110) return 2;
  if (lead >>> 4 === 0b1110) return 3;
  if (lead >>> 3 === 0b11110) return 4;
  return 1;
}

const ASCII_DECODER = new TextDecoder("utf-8", { fatal: true });

class JsonParser {
  private buf: Uint8Array;
  private pos = 0;
  // canonicalize: when true (jsonb), objects dedup last-wins and sort keys; when false (json
  // validation / on-demand parse), members are kept in input order with duplicates.
  private canonicalize: boolean;

  constructor(buf: Uint8Array, canonicalize: boolean) {
    this.buf = buf;
    this.canonicalize = canonicalize;
  }

  // parseDocument: a full JSON document — one value, surrounded by optional whitespace, nothing
  // trailing.
  parseDocument(): JsonNode {
    this.skipWs();
    const node = this.parseValue();
    this.skipWs();
    if (this.pos !== this.buf.length) {
      throw malformed("trailing characters after JSON value");
    }
    return node;
  }

  private skipWs(): void {
    while (this.pos < this.buf.length) {
      const c = this.buf[this.pos]!;
      if (c === 0x20 || c === 0x09 || c === 0x0a || c === 0x0d) this.pos++;
      else break;
    }
  }

  // peek returns the current byte, or -1 at end of input.
  private peek(): number {
    return this.pos < this.buf.length ? this.buf[this.pos]! : -1;
  }

  private parseValue(): JsonNode {
    const c = this.peek();
    if (c === -1) throw malformed("unexpected end of input");
    if (c === 0x7b /* { */) return this.parseObject();
    if (c === 0x5b /* [ */) return this.parseArray();
    if (c === 0x22 /* " */) return { kind: "string", value: this.parseString() };
    if (c === 0x74 /* t */) {
      this.expectKeyword("true");
      return { kind: "bool", value: true };
    }
    if (c === 0x66 /* f */) {
      this.expectKeyword("false");
      return { kind: "bool", value: false };
    }
    if (c === 0x6e /* n */) {
      this.expectKeyword("null");
      return { kind: "null" };
    }
    if (c === 0x2d /* - */ || (c >= 0x30 && c <= 0x39)) return this.parseNumber();
    throw malformed("unexpected character '" + String.fromCharCode(c) + "'");
  }

  private expectKeyword(kw: string): void {
    const end = this.pos + kw.length;
    if (end <= this.buf.length) {
      let ok = true;
      for (let i = 0; i < kw.length; i++) {
        if (this.buf[this.pos + i] !== kw.charCodeAt(i)) {
          ok = false;
          break;
        }
      }
      if (ok) {
        this.pos = end;
        return;
      }
    }
    throw malformed("expected '" + kw + "'");
  }

  private parseObject(): JsonNode {
    this.pos++; // consume '{'
    let members: JsonMember[] = [];
    this.skipWs();
    if (this.peek() === 0x7d /* } */) {
      this.pos++;
      return { kind: "object", members };
    }
    for (;;) {
      this.skipWs();
      if (this.peek() !== 0x22 /* " */) throw malformed("expected string key in object");
      const key = this.parseString();
      this.skipWs();
      if (this.peek() !== 0x3a /* : */) throw malformed("expected ':' after object key");
      this.pos++;
      this.skipWs();
      const value = this.parseValue();
      members.push({ key, value });
      this.skipWs();
      const c = this.peek();
      if (c === 0x2c /* , */) {
        this.pos++;
      } else if (c === 0x7d /* } */) {
        this.pos++;
        break;
      } else {
        throw malformed("expected ',' or '}' in object");
      }
    }
    if (this.canonicalize) members = canonicalizeObject(members);
    return { kind: "object", members };
  }

  private parseArray(): JsonNode {
    this.pos++; // consume '['
    const elements: JsonNode[] = [];
    this.skipWs();
    if (this.peek() === 0x5d /* ] */) {
      this.pos++;
      return { kind: "array", elements };
    }
    for (;;) {
      this.skipWs();
      elements.push(this.parseValue());
      this.skipWs();
      const c = this.peek();
      if (c === 0x2c /* , */) {
        this.pos++;
      } else if (c === 0x5d /* ] */) {
        this.pos++;
        break;
      } else {
        throw malformed("expected ',' or ']' in array");
      }
    }
    return { kind: "array", elements };
  }

  // parseString parses a JSON string token (the leading `"` is at this.pos), decoding escapes to the
  // actual UTF-8 content. RFC 8259: `\" \\ \/ \b \f \n \r \t` and `\uXXXX` (with surrogate pairs).
  // Unescaped control characters (< 0x20) are rejected.
  private parseString(): string {
    this.pos++; // consume opening '"'
    let out = "";
    for (;;) {
      const c = this.peek();
      if (c === -1) throw malformed("unterminated string");
      if (c === 0x22 /* " */) {
        this.pos++;
        return out;
      }
      if (c === 0x5c /* \ */) {
        this.pos++;
        const e = this.peek();
        if (e === -1) throw malformed("unterminated escape");
        switch (e) {
          case 0x22: // "
            out += '"';
            break;
          case 0x5c: // backslash
            out += "\\";
            break;
          case 0x2f: // /
            out += "/";
            break;
          case 0x62: // b
            out += "\b";
            break;
          case 0x66: // f
            out += "\f";
            break;
          case 0x6e: // n
            out += "\n";
            break;
          case 0x72: // r
            out += "\r";
            break;
          case 0x74: // t
            out += "\t";
            break;
          case 0x75: {
            // u
            this.pos++;
            const cp = this.parseHex4();
            // Surrogate pair handling (UTF-16 escapes).
            if (cp >= 0xd800 && cp <= 0xdbff) {
              // High surrogate: must be followed by \uDC00..\uDFFF.
              if (this.peek() !== 0x5c) throw malformed("unpaired high surrogate in \\u escape");
              this.pos++;
              if (this.peek() !== 0x75) throw malformed("unpaired high surrogate in \\u escape");
              this.pos++;
              const lo = this.parseHex4();
              if (lo < 0xdc00 || lo > 0xdfff) {
                throw malformed("invalid low surrogate in \\u escape");
              }
              const combined = 0x10000 + (((cp - 0xd800) << 10) | (lo - 0xdc00));
              out += String.fromCodePoint(combined);
            } else if (cp >= 0xdc00 && cp <= 0xdfff) {
              throw malformed("unpaired low surrogate in \\u escape");
            } else {
              out += String.fromCodePoint(cp);
            }
            continue; // parseHex4 already advanced pos past the 4 digits
          }
          default:
            throw malformed("invalid escape sequence");
        }
        this.pos++;
      } else if (c <= 0x1f) {
        throw malformed("control character in string must be escaped");
      } else {
        // Copy one UTF-8 code point verbatim. Determine its byte length, decode-check it (fatal), and
        // append the decoded text.
        const len = utf8Len(c);
        const end = this.pos + len;
        if (end > this.buf.length) throw malformed("truncated UTF-8 sequence in string");
        try {
          out += ASCII_DECODER.decode(this.buf.subarray(this.pos, end));
        } catch {
          throw malformed("invalid UTF-8 in string");
        }
        this.pos = end;
      }
    }
  }

  // parseHex4 reads exactly four hex digits as a code-unit (the cursor is just past `\u`).
  private parseHex4(): number {
    if (this.pos + 4 > this.buf.length) throw malformed("truncated \\u escape");
    let v = 0;
    for (let i = 0; i < 4; i++) {
      const d = this.buf[this.pos + i]!;
      let nib: number;
      if (d >= 0x30 && d <= 0x39)
        nib = d - 0x30; // 0-9
      else if (d >= 0x61 && d <= 0x66)
        nib = d - 0x61 + 10; // a-f
      else if (d >= 0x41 && d <= 0x46)
        nib = d - 0x41 + 10; // A-F
      else throw malformed("invalid hex digit in \\u escape");
      v = (v << 4) | nib;
    }
    this.pos += 4;
    return v;
  }

  // parseNumber parses a JSON number token (RFC 8259 grammar) into an exact Decimal. No leading zeros
  // (`01` is malformed), a `.` requires fractional digits, `e`/`E` an exponent. The value is built via
  // the shared decimal-from-parts path so a jsonb number reads identically to a numeric literal (`1e2`
  // → `100`, `1.50` keeps scale 2). An out-of-cap magnitude → 22003.
  private parseNumber(): JsonNode {
    const start = this.pos;
    let neg = false;
    if (this.peek() === 0x2d /* - */) {
      this.pos++;
      neg = true;
    }
    // Integer part: `0` alone, or a nonzero digit followed by more digits.
    const c = this.peek();
    if (c === 0x30 /* 0 */) {
      this.pos++;
    } else if (c >= 0x31 && c <= 0x39) {
      while (this.isDigit(this.peek())) this.pos++;
    } else {
      throw malformed("invalid number");
    }
    const intEnd = this.pos;
    const intPart = this.asciiSlice(start + (neg ? 1 : 0), intEnd);

    // Fractional part.
    let frac = "";
    if (this.peek() === 0x2e /* . */) {
      this.pos++;
      const fs = this.pos;
      while (this.isDigit(this.peek())) this.pos++;
      if (this.pos === fs) throw malformed("expected digits after decimal point");
      frac = this.asciiSlice(fs, this.pos);
    }

    // Exponent.
    let exp: number | null = null;
    const ec = this.peek();
    if (ec === 0x65 /* e */ || ec === 0x45 /* E */) {
      this.pos++;
      let esign = 1;
      const sc = this.peek();
      if (sc === 0x2d /* - */) {
        this.pos++;
        esign = -1;
      } else if (sc === 0x2b /* + */) {
        this.pos++;
      }
      const es = this.pos;
      let mag = 0;
      while (this.isDigit(this.peek())) {
        const d = this.buf[this.pos]! - 0x30;
        // Clamp to the decimal exponent limit while scanning (decimal.ts EXP_LIMIT); an exponent this
        // large already drives the value past the caps → 22003.
        mag = Math.min(mag * 10 + d, EXP_LIMIT);
        this.pos++;
      }
      if (this.pos === es) throw malformed("expected digits in exponent");
      exp = esign * mag;
    }

    const [digits, scale] = decimalFromParts(intPart, frac, exp);
    const d = Decimal.fromDigitsScale(neg, digits, scale).checkCap();
    return { kind: "number", dec: d };
  }

  private isDigit(c: number): boolean {
    return c >= 0x30 && c <= 0x39;
  }

  // asciiSlice returns the ASCII text of buf[from, to) (a number token is pure ASCII).
  private asciiSlice(from: number, to: number): string {
    let s = "";
    for (let i = from; i < to; i++) s += String.fromCharCode(this.buf[i]!);
    return s;
  }
}

// makeObject builds a canonical `jsonb` object node from (key, value) members — last-wins dedup then
// the canonical key sort (json.md §2.3). The constructor for `jsonb_build_object` (B1) and the future
// `jsonb_object`, reusing the parser's canonicalizer so a built object is byte-identical to a parsed
// one.
export function makeObject(members: JsonMember[]): JsonNode {
  return { kind: "object", members: canonicalizeObject(members) };
}

// canonicalizeObject canonicalizes object members (spec/design/json.md §2.3): drop duplicate keys
// keeping the LAST occurrence (PG jsonb last-wins), then sort the survivors length-then-bytewise.
// Done before sorting so the stored object has unique keys in canonical order — a pure function of
// input.
function canonicalizeObject(members: JsonMember[]): JsonMember[] {
  // Last-wins dedup, preserving the value of the last occurrence (re-sort follows so first-
  // appearance order is irrelevant).
  const out: JsonMember[] = [];
  for (const m of members) {
    let found = false;
    for (let i = 0; i < out.length; i++) {
      if (out[i]!.key === m.key) {
        out[i]!.value = m.value;
        found = true;
        break;
      }
    }
    if (!found) out.push(m);
  }
  // Insertion sort by canonical key order (small objects; a stable, dependency-free sort that is
  // byte-identical across cores).
  for (let i = 1; i < out.length; i++) {
    for (let j = i; j > 0 && keyCmp(out[j]!.key, out[j - 1]!.key) < 0; j--) {
      const tmp = out[j]!;
      out[j] = out[j - 1]!;
      out[j - 1] = tmp;
    }
  }
  return out;
}

// ---------------------------------------------------------------------------------------------
// Output (`jsonbOut` — the canonical PG render). `json_out` is the stored verbatim text.
// ---------------------------------------------------------------------------------------------

// jsonbOut renders a jsonb node to the canonical PG text (spec/design/json.md §6.2): one space after
// each `:` and `,`, keys in canonical order, numbers via the Decimal renderer (scale preserved),
// strings JSON-escaped, `true`/`false`/`null` lowercase.
export function jsonbOut(node: JsonNode): string {
  const parts: string[] = [];
  writeNode(node, parts);
  return parts.join("");
}

function writeNode(node: JsonNode, out: string[]): void {
  switch (node.kind) {
    case "null":
      out.push("null");
      break;
    case "bool":
      out.push(node.value ? "true" : "false");
      break;
    case "number":
      out.push(node.dec.render());
      break;
    case "string":
      writeJsonString(node.value, out);
      break;
    case "array":
      out.push("[");
      for (let i = 0; i < node.elements.length; i++) {
        if (i > 0) out.push(", ");
        writeNode(node.elements[i]!, out);
      }
      out.push("]");
      break;
    case "object":
      out.push("{");
      for (let i = 0; i < node.members.length; i++) {
        if (i > 0) out.push(", ");
        writeJsonString(node.members[i]!.key, out);
        out.push(": ");
        writeNode(node.members[i]!.value, out);
      }
      out.push("}");
      break;
  }
}

// jsonCompactOut renders a node tree to COMPACT JSON text — no space after `:` or `,` — the form
// PG's `json` processing functions (`json_strip_nulls`, `to_json`, the json builders) emit (a `json`
// value's output style, distinct from `jsonb`'s spaced canonical form). Members render in their node
// order (the caller controls canonicalization; a `json`-on-demand parse keeps input order).
export function jsonCompactOut(node: JsonNode): string {
  const parts: string[] = [];
  writeCompact(node, parts);
  return parts.join("");
}

function writeCompact(node: JsonNode, out: string[]): void {
  switch (node.kind) {
    case "null":
      out.push("null");
      break;
    case "bool":
      out.push(node.value ? "true" : "false");
      break;
    case "number":
      out.push(node.dec.render());
      break;
    case "string":
      writeJsonString(node.value, out);
      break;
    case "array":
      out.push("[");
      for (let i = 0; i < node.elements.length; i++) {
        if (i > 0) out.push(",");
        writeCompact(node.elements[i]!, out);
      }
      out.push("]");
      break;
    case "object":
      out.push("{");
      for (let i = 0; i < node.members.length; i++) {
        if (i > 0) out.push(",");
        writeJsonString(node.members[i]!.key, out);
        out.push(":");
        writeCompact(node.members[i]!.value, out);
      }
      out.push("}");
      break;
  }
}

// writeJsonString JSON-escapes a string the way PG escape_json does: quote, escape `"` and `\`, the
// short escapes for `\b \f \n \r \t`, other control chars (< 0x20) as `\u00XX` (lowercase hex); `/`
// is NOT escaped and non-ASCII is emitted as raw UTF-8. Iterates by code point (the escape decision
// is per-character) while sorting/comparison stays bytewise.
export function writeJsonString(s: string, out: string[]): void {
  out.push('"');
  for (const ch of s) {
    switch (ch) {
      case '"':
        out.push('\\"');
        break;
      case "\\":
        out.push("\\\\");
        break;
      case "\b":
        out.push("\\b");
        break;
      case "\f":
        out.push("\\f");
        break;
      case "\n":
        out.push("\\n");
        break;
      case "\r":
        out.push("\\r");
        break;
      case "\t":
        out.push("\\t");
        break;
      default: {
        const code = ch.codePointAt(0)!;
        if (code < 0x20) {
          out.push("\\u" + code.toString(16).padStart(4, "0"));
        } else {
          out.push(ch);
        }
      }
    }
  }
  out.push('"');
}

// ---------------------------------------------------------------------------------------------
// Accessor operators (`-> ->> #> #>>`, spec/design/json-sql-functions.md §1) — jsonb kernels over
// the canonical node tree. (The `json` overloads, which preserve the verbatim sub-text, are a
// deferred follow-on — json.md §4.)
// ---------------------------------------------------------------------------------------------

// getField is `jsonb -> text`: an object field by key. null (→ SQL NULL) if the node is not an
// object or the key is absent. A duplicate-key object cannot occur (jsonb is canonical, unique keys).
export function getField(node: JsonNode, key: string): JsonNode | null {
  if (node.kind !== "object") return null;
  for (const m of node.members) {
    if (m.key === key) return m.value;
  }
  return null;
}

// getIndex is `jsonb -> int`: an array element by index (a negative index counts from the end). null
// (→ SQL NULL) if the node is not an array or the index is out of range. `idx` is a bigint (the TS
// core's integer representation — CLAUDE.md §2).
export function getIndex(node: JsonNode, idx: bigint): JsonNode | null {
  if (node.kind !== "array") return null;
  const len = BigInt(node.elements.length);
  const i = idx < 0n ? len + idx : idx;
  if (i >= 0n && i < len) return node.elements[Number(i)]!;
  return null;
}

// getPath is `jsonb #> text[]`: navigate a path of text steps. At each step an object uses the step
// as a key; an array parses the step as an integer index (a non-integer or out-of-range step → null).
// An empty path returns the whole node (PG). null (→ SQL NULL) if any step fails.
export function getPath(node: JsonNode, path: string[]): JsonNode | null {
  let cur: JsonNode = node;
  for (const step of path) {
    if (cur.kind === "object") {
      const next = getField(cur, step);
      if (next === null) return null;
      cur = next;
    } else if (cur.kind === "array") {
      const idx = parseIntStep(step);
      if (idx === null) return null;
      const next = getIndex(cur, idx);
      if (next === null) return null;
      cur = next;
    } else {
      return null;
    }
  }
  return cur;
}

// parseIntStep parses a #>-path step as a base-10 integer index (the Rust `step.trim().parse()` /
// Go `strconv.ParseInt(TrimSpace(...))` analogue): leading/trailing ASCII whitespace is trimmed, an
// optional leading sign is allowed, and the remainder must be all digits. null on a non-integer.
function parseIntStep(step: string): bigint | null {
  const t = step.trim();
  if (!/^[+-]?[0-9]+$/.test(t)) return null;
  try {
    return BigInt(t);
  } catch {
    return null;
  }
}

// nodeToText is the `->>` / `#>>` text rendering of an accessed node: a STRING node yields its raw
// content (unescaped); a JSON `null` node yields SQL NULL (null); every other node yields its
// canonical jsonb_out text.
export function nodeToText(node: JsonNode): string | null {
  if (node.kind === "null") return null;
  if (node.kind === "string") return node.value;
  return jsonbOut(node);
}

// ---------------------------------------------------------------------------------------------
// Containment / existence operators (`@> <@ ? ?| ?&`, spec/design/json-sql-functions.md §1, J5).
// ---------------------------------------------------------------------------------------------

// nodesEqual is jsonb value equality — two canonical nodes are equal iff they compare equal under
// the total btree order (jsonNodeCmp == 0; the structural form IS the value, §5).
function nodesEqual(a: JsonNode, b: JsonNode): boolean {
  return jsonNodeCmp(a, b) === 0;
}

// isContainer reports whether a node is a container (object or array) vs a scalar
// (null/bool/number/string).
function isContainer(n: JsonNode): boolean {
  return n.kind === "object" || n.kind === "array";
}

// contains is `a @> b` — does the jsonb document `a` deeply contain `b` (PG `jsonb_contains`)? The
// rules, pinned against the postgres:18 oracle:
//   - object @> object: every member of `b` has a matching key in `a` whose value contains it.
//   - array @> array: every element of `b` is "contained in" `a` — a SCALAR element must EQUAL a
//     direct element of `a` (no recursion into `a`'s sub-containers); an OBJECT/ARRAY element must
//     be contained in some same-kind direct element of `a`.
//   - array @> scalar: the scalar is a direct element of the array (by value equality).
//   - scalar @> scalar: value equality.
//   - any other top-level pairing (object vs array, scalar vs array/object, …) is false.
export function contains(a: JsonNode, b: JsonNode): boolean {
  if (a.kind === "object" && b.kind === "object") {
    return b.members.every((mb) => {
      const va = getField(a, mb.key);
      return va !== null && contains(va, mb.value);
    });
  }
  if (a.kind === "array" && b.kind === "array") {
    return b.elements.every((e) => elementInArray(a.elements, e));
  }
  // array @> a scalar: the scalar is a direct element of the array.
  if (a.kind === "array" && !isContainer(b)) {
    return a.elements.some((x) => nodesEqual(x, b));
  }
  // scalar @> scalar: value equality (a container `a` against a scalar `b` already fell through;
  // two scalars compare by structural equality).
  if (!isContainer(a) && !isContainer(b)) {
    return nodesEqual(a, b);
  }
  return false;
}

// elementInArray reports whether `e` (an element of the right array) is "contained in" the left
// array `arr`: a scalar element must EQUAL a direct element of `arr`; an object/array element must
// be contained in some same-kind direct element of `arr`.
function elementInArray(arr: JsonNode[], e: JsonNode): boolean {
  if (e.kind === "object") {
    return arr.some((x) => x.kind === "object" && contains(x, e));
  }
  if (e.kind === "array") {
    return arr.some((x) => x.kind === "array" && contains(x, e));
  }
  // scalar
  return arr.some((x) => nodesEqual(x, e));
}

// hasKey is `jsonb ? text` — does the document have this top-level key? An object: the key is
// present; an array: the key is a string element; a string scalar: it equals the key; otherwise
// false (PG semantics, oracle-pinned).
export function hasKey(node: JsonNode, key: string): boolean {
  switch (node.kind) {
    case "object":
      return node.members.some((m) => m.key === key);
    case "array":
      return node.elements.some((e) => e.kind === "string" && e.value === key);
    case "string":
      return node.value === key;
    default:
      return false;
  }
}

// ---------------------------------------------------------------------------------------------
// Mutation operators (`|| - #-`, spec/design/json-sql-functions.md §1, J6).
// ---------------------------------------------------------------------------------------------

// cannotDelete builds the 22023 (invalid_parameter_value) error for an illegal delete target.
function cannotDelete(msg: string): EngineError {
  return engineError("invalid_parameter_value", msg);
}

// concat is `a || b` — concatenate / shallow-merge (PG): two objects merge with the RIGHT side
// winning on a key conflict (result re-canonicalized); otherwise each operand is treated as an array
// (an array stays, a non-array becomes a one-element array) and the two are concatenated.
export function concat(a: JsonNode, b: JsonNode): JsonNode {
  if (a.kind === "object" && b.kind === "object") {
    const out: JsonMember[] = a.members.map((m) => ({ key: m.key, value: m.value }));
    for (const m of b.members) {
      let found = false;
      for (let i = 0; i < out.length; i++) {
        if (out[i]!.key === m.key) {
          out[i]!.value = m.value;
          found = true;
          break;
        }
      }
      if (!found) out.push({ key: m.key, value: m.value });
    }
    // Insertion sort by canonical key order (small objects; byte-identical across cores).
    for (let i = 1; i < out.length; i++) {
      for (let j = i; j > 0 && keyCmp(out[j]!.key, out[j - 1]!.key) < 0; j--) {
        const tmp = out[j]!;
        out[j] = out[j - 1]!;
        out[j - 1] = tmp;
      }
    }
    return { kind: "object", members: out };
  }
  const elems = toArrayElems(a);
  elems.push(...toArrayElems(b));
  return { kind: "array", elements: elems };
}

// toArrayElems treats a node as an array for `||`: an array contributes its elements, any other node
// becomes a single one-element list.
function toArrayElems(n: JsonNode): JsonNode[] {
  if (n.kind === "array") return n.elements.slice();
  return [n];
}

// deleteKey is `jsonb - text` — delete a key from an object, or delete every matching string element
// from an array; a scalar is `22023` ("cannot delete from scalar").
export function deleteKey(node: JsonNode, key: string): JsonNode {
  switch (node.kind) {
    case "object":
      return { kind: "object", members: node.members.filter((m) => m.key !== key) };
    case "array":
      return {
        kind: "array",
        elements: node.elements.filter((e) => !(e.kind === "string" && e.value === key)),
      };
    default:
      throw cannotDelete("cannot delete from scalar");
  }
}

// deleteIndex is `jsonb - int` — delete the array element at an index (negative from the end; out of
// range is a no-op). An object is `22023` ("cannot delete from object using integer index"); a scalar
// is `22023` ("cannot delete from scalar").
export function deleteIndex(node: JsonNode, idx: bigint): JsonNode {
  switch (node.kind) {
    case "array": {
      const len = BigInt(node.elements.length);
      const i = idx < 0n ? len + idx : idx;
      if (i < 0n || i >= len) return node;
      const out = node.elements.slice();
      out.splice(Number(i), 1);
      return { kind: "array", elements: out };
    }
    case "object":
      throw cannotDelete("cannot delete from object using integer index");
    default:
      throw cannotDelete("cannot delete from scalar");
  }
}

// deleteKeys is `jsonb - text[]` — delete each key in turn (the `- text` rule applied per element).
export function deleteKeys(node: JsonNode, keys: string[]): JsonNode {
  let cur = node;
  for (const k of keys) cur = deleteKey(cur, k);
  return cur;
}

// deletePath is `jsonb #- text[]` — delete the element at a path. An empty path is a no-op (even on a
// scalar); otherwise navigate to the parent and delete the last step (a key from an object, an index
// from an array, negative from the end, out of range a no-op; a missing intermediate step a no-op). A
// non-empty path that reaches a scalar is `22023` ("cannot delete path in scalar").
export function deletePath(node: JsonNode, path: string[]): JsonNode {
  if (path.length === 0) return node;
  const step = path[0]!;
  const rest = path.slice(1);
  switch (node.kind) {
    case "object": {
      const out: JsonMember[] = node.members.map((m) => ({ key: m.key, value: m.value }));
      const pos = out.findIndex((m) => m.key === step);
      if (pos >= 0) {
        if (rest.length === 0) {
          out.splice(pos, 1);
        } else {
          out[pos]!.value = deletePath(out[pos]!.value, rest);
        }
      }
      return { kind: "object", members: out };
    }
    case "array": {
      // A non-integer (or out-of-i64-range) step misses (no-op), matching Rust/Go ParseInt<i64>.
      // Parse the trimmed step as a base-10 integer bounded to the signed-64-bit range.
      const trimmed = step.trim();
      if (!/^[+-]?\d+$/.test(trimmed)) return node;
      const idx = BigInt(trimmed);
      if (idx < -(2n ** 63n) || idx > 2n ** 63n - 1n) return node;
      const len = BigInt(node.elements.length);
      const i = idx < 0n ? len + idx : idx;
      if (i < 0n || i >= len) return node; // out of range, no-op
      const out = node.elements.slice();
      if (rest.length === 0) {
        out.splice(Number(i), 1);
      } else {
        out[Number(i)] = deletePath(out[Number(i)]!, rest);
      }
      return { kind: "array", elements: out };
    }
    default:
      throw cannotDelete("cannot delete path in scalar");
  }
}

// PathSetMode selects whether a path mutation REPLACES at the final step (`jsonb_set`) or INSERTS a
// new element (`jsonb_insert`). For Insert, the flag is `insert_after` (place after the array index,
// not before); for Set, the flag is `create_if_missing` (add a missing final key / out-of-range
// index).
export type PathSetMode = "set" | "insert";

// setPath is `jsonb_set(target, path, value[, create_if_missing])` (json-sql-functions.md §2): set
// the value at `path` (a text[] of object keys / array indices). A non-final missing key/index is a
// no-op (the target is returned unchanged); at the final step an existing element is REPLACED, a
// missing one is added only when `create`. A scalar at any step → `22023`; a non-integer step into an
// array → `22P02`. Negative array indices count from the end; an out-of-range create appends (≥len)
// or prepends (<0).
export function setPath(
  node: JsonNode,
  path: string[],
  value: JsonNode,
  create: boolean,
): JsonNode {
  return setInsertPath(node, path, value, create, "set", 0);
}

// insertPath is `jsonb_insert(target, path, value[, insert_after])` (json-sql-functions.md §2): like
// setPath but the final step INSERTS rather than replaces — an existing object key → `22023` ("cannot
// replace existing key"); an array index inserts before the index (or after, when `insert_after`).
export function insertPath(
  node: JsonNode,
  path: string[],
  value: JsonNode,
  insertAfter: boolean,
): JsonNode {
  return setInsertPath(node, path, value, insertAfter, "insert", 0);
}

// setInsertPath is the shared recursion for setPath/insertPath (mirrors deletePath's structure).
// `flag` is create_if_missing (Set) | insert_after (Insert); `pos` is the 0-based path position, for
// the 22P02 message.
function setInsertPath(
  node: JsonNode,
  path: string[],
  value: JsonNode,
  flag: boolean,
  mode: PathSetMode,
  pos: number,
): JsonNode {
  if (path.length === 0) return node; // an empty path returns the target unchanged (PG)
  const step = path[0]!;
  const rest = path.slice(1);
  const isFinal = rest.length === 0;
  switch (node.kind) {
    case "object": {
      const out: JsonMember[] = node.members.map((m) => ({ key: m.key, value: m.value }));
      const found = out.findIndex((m) => m.key === step);
      if (isFinal) {
        if (found >= 0) {
          if (mode === "insert") throw cannotDelete("cannot replace existing key");
          out[found]!.value = value;
        } else if (mode === "insert" || flag) {
          // A missing final key: Set adds it only with create; Insert always adds it.
          out.push({ key: step, value });
        }
      } else if (found >= 0) {
        out[found]!.value = setInsertPath(out[found]!.value, rest, value, flag, mode, pos + 1);
      }
      // (a missing non-final key is a no-op). Re-canonicalize: a replaced value keeps the canonical
      // order; an added key is sorted into place.
      return { kind: "object", members: canonicalizeObject(out) };
    }
    case "array": {
      // The step must parse as a base-10 i64 (trimmed); a non-integer OR an out-of-i64-range value →
      // 22P02 (1-based position), matching Rust `step.trim().parse::<i64>()` (which errors on both).
      const trimmed = step.trim();
      const bad = (): never => {
        throw malformed(`path element at position ${pos + 1} is not an integer: "${step}"`);
      };
      if (!/^[+-]?\d+$/.test(trimmed)) bad();
      const idx64 = BigInt(trimmed);
      if (idx64 < -(2n ** 63n) || idx64 > 2n ** 63n - 1n) bad();
      const idx = Number(idx64);
      const len = node.elements.length;
      const out = node.elements.slice();
      if (isFinal) {
        if (mode === "insert") {
          // Insertion index: normalize a negative index from the end, clamp to [0,len], then
          // `insert_after` shifts one past.
          let i = idx < 0 ? len + idx : idx;
          if (i < 0) i = 0;
          if (flag) i += 1;
          if (i > len) i = len;
          out.splice(i, 0, value);
        } else {
          const i = idx < 0 ? len + idx : idx;
          if (i >= 0 && i < len) {
            out[i] = value;
          } else if (flag) {
            // out of range + create: append (≥len) or prepend (<0).
            if (idx < 0) out.unshift(value);
            else out.push(value);
          }
        }
      } else {
        const i = idx < 0 ? len + idx : idx;
        if (i >= 0 && i < len) {
          out[i] = setInsertPath(out[i]!, rest, value, flag, mode, pos + 1);
        }
      }
      return { kind: "array", elements: out };
    }
    default:
      throw cannotDelete("cannot set path in scalar");
  }
}

// ---------------------------------------------------------------------------------------------
// Processing / introspection functions (B1, spec/design/json-sql-functions.md §2).
// ---------------------------------------------------------------------------------------------

// typeofName is `json[b]_typeof` — the JSON type name of a node (PG): `object`/`array`/`string`/
// `number`/`boolean`/`null`.
export function typeofName(node: JsonNode): string {
  switch (node.kind) {
    case "null":
      return "null";
    case "bool":
      return "boolean";
    case "number":
      return "number";
    case "string":
      return "string";
    case "array":
      return "array";
    case "object":
      return "object";
  }
}

// arrayLength is `json[b]_array_length` — the element count of an array node; a non-array is `22023`.
export function arrayLength(node: JsonNode): bigint {
  if (node.kind !== "array") {
    throw engineError("invalid_parameter_value", "cannot get array length of a scalar");
  }
  return BigInt(node.elements.length);
}

// stripNulls is `json[b]_strip_nulls` — recursively remove object members whose value is JSON `null`
// (array nulls are kept, PG). Objects re-canonicalize (the surviving members stay in canonical order;
// the input is already canonical for jsonb, and for json the on-demand parse order is kept).
export function stripNulls(node: JsonNode): JsonNode {
  switch (node.kind) {
    case "object": {
      const members: JsonMember[] = [];
      for (const m of node.members) {
        if (m.value.kind === "null") continue;
        members.push({ key: m.key, value: stripNulls(m.value) });
      }
      return { kind: "object", members };
    }
    case "array":
      return { kind: "array", elements: node.elements.map(stripNulls) };
    default:
      return node;
  }
}

// pretty is `jsonb_pretty` — an indented multi-line render (PG: 4-space indent, one space after `:`).
// A container ALWAYS multi-lines (even an empty one: `{` newline, then the close at the container's
// own indent → `{\n}` / `{\n    }`); scalars render inline.
export function pretty(node: JsonNode): string {
  const parts: string[] = [];
  writePretty(node, 0, parts);
  return parts.join("");
}

function writePretty(node: JsonNode, indent: number, out: string[]): void {
  switch (node.kind) {
    case "object":
      out.push("{");
      for (let i = 0; i < node.members.length; i++) {
        if (i > 0) out.push(",");
        out.push("\n");
        pushIndent(indent + 1, out);
        writeJsonString(node.members[i]!.key, out);
        out.push(": ");
        writePretty(node.members[i]!.value, indent + 1, out);
      }
      out.push("\n");
      pushIndent(indent, out);
      out.push("}");
      break;
    case "array":
      out.push("[");
      for (let i = 0; i < node.elements.length; i++) {
        if (i > 0) out.push(",");
        out.push("\n");
        pushIndent(indent + 1, out);
        writePretty(node.elements[i]!, indent + 1, out);
      }
      out.push("\n");
      pushIndent(indent, out);
      out.push("]");
      break;
    default:
      writeNode(node, out);
      break;
  }
}

function pushIndent(level: number, out: string[]): void {
  for (let i = 0; i < level; i++) out.push("    ");
}
