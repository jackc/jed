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
import { utf8Length } from "./memsize.ts";

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

// MAX_JSON_DEPTH is the maximum nesting depth of a `json`/`jsonb` document (spec/design/json.md
// §6.4): the largest number of containers (arrays/objects) on any root-to-leaf path — a scalar is
// depth 0, `[]` depth 1. Every producer (the text parser, the constructors, the on-disk decoder)
// enforces it, so every recursive walk over a node (compare, render, codec, containment) is bounded
// by it — a deeply nested untrusted document would otherwise overflow V8's call stack as an uncoded
// RangeError. A fixed, cross-core constant sized for the weakest core's native stack; it also
// bounds jsonpath program nesting.
export const MAX_JSON_DEPTH = 256;

// tooDeep is `54001` statement_too_complex — a document (or constructor result) deeper than
// MAX_JSON_DEPTH (spec/design/json.md §6.4).
export function tooDeep(): EngineError {
  return engineError(
    "statement_too_complex",
    `json nesting depth exceeds the maximum of ${MAX_JSON_DEPTH}`,
  );
}

// checkDepth enforces MAX_JSON_DEPTH on a constructed `jsonb` node — the constructor gate (json.md
// §6.4): an operation that nests existing values inside a new container checks its result. The walk
// stops one level past the limit, so it never recurses deeper than MAX_JSON_DEPTH + 1.
export function checkDepth(node: JsonNode): void {
  if (exceedsDepth(node, MAX_JSON_DEPTH)) throw tooDeep();
}

// exceedsDepth reports whether `node` nests more than `budget` containers deep.
function exceedsDepth(node: JsonNode, budget: number): boolean {
  if (node.kind === "array") {
    return budget === 0 || node.elements.some((e) => exceedsDepth(e, budget - 1));
  }
  if (node.kind === "object") {
    return budget === 0 || node.members.some((m) => exceedsDepth(m.value, budget - 1));
  }
  return false;
}

// checkTextDepth enforces MAX_JSON_DEPTH on a constructed textual `json` value (json.md §6.4) — a
// NON-recursive bracket scan of well-formed JSON text (brackets inside strings are skipped). Scans
// UTF-16 code units: every structural character is ASCII, and a surrogate never equals one.
export function checkTextDepth(text: string): void {
  let depth = 0;
  let inString = false;
  let escaped = false;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (inString) {
      if (escaped) escaped = false;
      else if (c === 0x5c /* \ */) escaped = true;
      else if (c === 0x22 /* " */) inString = false;
      continue;
    }
    if (c === 0x22 /* " */) {
      inString = true;
    } else if (c === 0x5b /* [ */ || c === 0x7b /* { */) {
      depth++;
      if (depth > MAX_JSON_DEPTH) throw tooDeep();
    } else if (c === 0x5d /* ] */ || c === 0x7d /* } */) {
      if (depth > 0) depth--;
    }
  }
}

function malformed(detail: string): Error {
  return engineError(
    "invalid_text_representation",
    "invalid input syntax for type json: " + detail,
  );
}

// escapeSyntaxError is PostgreSQL's 22P02 `invalid input syntax for type {typeName}` carrying one of
// its json_errdetail / jsonpath-scanner sentences as the Detail (error-fields.md §9.2) — the
// `\u`-escape family, whose text jed matches byte-for-byte.
export function escapeSyntaxError(typeName: string, detail: string): EngineError {
  return engineError(
    "invalid_text_representation",
    "invalid input syntax for type " + typeName,
  ).withDetail(detail);
}

// escapeFormatError is a `\u` escape not followed by four hex digits (PG
// JSON_UNICODE_ESCAPE_FORMAT).
function escapeFormatError(): EngineError {
  return escapeSyntaxError("json", '"\\u" must be followed by four hexadecimal digits.');
}

// HIGH_SURROGATE_DETAIL is the Detail of a high surrogate followed by another high surrogate (PG
// JSON_UNICODE_HIGH_SURROGATE, and the jsonpath scanner's same sentence).
export const HIGH_SURROGATE_DETAIL = "Unicode high surrogate must not follow a high surrogate.";

// LOW_SURROGATE_DETAIL is the Detail of a low surrogate without a preceding high one, or a high
// surrogate not followed by a low one (PG JSON_UNICODE_LOW_SURROGATE, and the jsonpath scanner's).
export const LOW_SURROGATE_DETAIL = "Unicode low surrogate must follow a high surrogate.";

// unicodeZeroError is 22P05 for a decoded `\u0000` (PG JSON_UNICODE_CODE_POINT_ZERO; the jsonpath
// scanner raises the same): text cannot hold the NUL character (json.md §6.2).
export function unicodeZeroError(): EngineError {
  return engineError("untranslatable_character", "unsupported Unicode escape sequence").withDetail(
    "\\u0000 cannot be converted to text.",
  );
}

// jsonbIn parses + canonicalizes JSON text into a jsonb node tree (`jsonb_in` — spec/design/json.md
// §6.2): numbers → Decimal, object keys deduped last-wins then sorted length-then-bytewise.
// Malformed input → 22P02; a `\u0000` escape → 22P05.
export function jsonbIn(input: string): JsonNode {
  return new JsonParser(UTF8.encode(input), true, true).parseDocument();
}

// validateJson validates JSON text well-formedness (`json_in` — spec/design/json.md §4); the caller
// stores the original bytes verbatim. Malformed input → 22P02. Like PostgreSQL's json_in this only
// lexes: a `\u` escape must have four hex digits, but `\u0000` and an unpaired surrogate are accepted
// (they fail later, in a function that de-escapes the string — json.md §4.2).
export function validateJson(input: string): void {
  new JsonParser(UTF8.encode(input), false, false).parseDocument();
}

// parsePreservingJson parses JSON text into a node tree WITHOUT canonicalizing (object key order +
// duplicates preserved) — the on-demand structural parse a `json` operator needs
// (spec/design/json.md §4). Strings are de-escaped, so a `\u0000` (22P05) or an unpaired surrogate
// (22P02) anywhere in the document raises (PG's need_escapes parse, json.md §4.2).
export function parsePreservingJson(input: string): JsonNode {
  return new JsonParser(UTF8.encode(input), false, true).parseDocument();
}

// parseShapeJson is the node SHAPE of JSON text, for a consumer that never reads string contents
// (json_typeof, json_array_length, IS JSON without UNIQUE KEYS — PG lexes these without
// need_escapes): every string (member names included) is left EMPTY, and `\u` escapes are only
// lexed, never decoded or checked (json.md §4.2).
export function parseShapeJson(input: string): JsonNode {
  return new JsonParser(UTF8.encode(input), false, false).parseDocument();
}

// parsePreservingJsonUnlessRoot is the node tree of a `json` argument to a de-escaping function that
// raises a kind error on a root container of the wrong kind (wrongRoot is "[" or "{"). PostgreSQL
// raises that error at the root's opening bracket, before lexing anything inside it, so such a root
// is returned as its parseShapeJson (the caller raises the kind error and reads no string); any other
// root is parsePreservingJson'd, its escapes checked first (json.md §4.2).
export function parsePreservingJsonUnlessRoot(input: string, wrongRoot: "[" | "{"): JsonNode {
  const start = skipJsonWs(input, 0);
  if (input[start] === wrongRoot) return parseShapeJson(input);
  return parsePreservingJson(input);
}

// checkJsonEscapes checks the escapes of every string token of well-formed JSON text the way a
// de-escaping parse would (PG lexes a `json` argument with need_escapes for every function that reads
// string contents): the first `\u0000` (22P05) or unpaired surrogate (22P02) in document order raises
// (json.md §4.2).
export function checkJsonEscapes(text: string): void {
  // Only a `\u` escape can fail the check, so a document without one skips the scan.
  if (!text.includes("\\u")) return;
  new JsonParser(UTF8.encode(text), false, true).checkStringEscapes();
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
  // needEscapes: PostgreSQL's need_escapes (json.md §4.2) — when true, strings are de-escaped
  // (surrogate pairs combined, and `\u0000` (22P05) or an unpaired surrogate (22P02) rejected); when
  // false (json_in, parseShapeJson), escapes are only lexed and strings are left empty.
  private needEscapes: boolean;
  // depth: the current container nesting depth (spec/design/json.md §6.4) — entering the
  // MAX_JSON_DEPTH + 1-th `[`/`{` aborts 54001 before parsing anything inside it.
  private depth = 0;

  constructor(buf: Uint8Array, canonicalize: boolean, needEscapes: boolean) {
    this.buf = buf;
    this.canonicalize = canonicalize;
    this.needEscapes = needEscapes;
  }

  // checkStringEscapes de-escapes every string token of well-formed JSON (checkJsonEscapes).
  checkStringEscapes(): void {
    while (this.pos < this.buf.length) {
      if (this.buf[this.pos] === 0x22 /* " */) this.parseString();
      else this.pos++;
    }
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

  // descend enters one container level, enforcing MAX_JSON_DEPTH (json.md §6.4).
  private descend(): void {
    this.depth++;
    if (this.depth > MAX_JSON_DEPTH) throw tooDeep();
  }

  private parseValue(): JsonNode {
    const c = this.peek();
    if (c === -1) throw malformed("unexpected end of input");
    if (c === 0x7b /* { */) {
      this.descend();
      const node = this.parseObject();
      this.depth--;
      return node;
    }
    if (c === 0x5b /* [ */) {
      this.descend();
      const node = this.parseArray();
      this.depth--;
      return node;
    }
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

  // decodeStringToken decodes a buffer holding exactly one string token (verbatim navigation).
  decodeStringToken(): string {
    return this.parseString();
  }

  // parseString parses a JSON string token (the leading `"` is at this.pos) — a port of PostgreSQL's
  // json_lex_string (src/common/jsonapi.c). RFC 8259: `\" \\ \/ \b \f \n \r \t` and `\uXXXX`;
  // unescaped control characters (< 0x20) are rejected. With needEscapes the content is decoded: a
  // high surrogate must be followed directly by a `\u` low surrogate (the pair combines), anything
  // else after it — and a low surrogate without one — is 22P02 with PG's Detail, and a decoded
  // `\u0000` is 22P05. Without it the content is not built (an empty string is returned).
  private parseString(): string {
    this.pos++; // consume opening '"'
    let out = "";
    // hiSurrogate: a pending high surrogate awaiting its low half (PG's hi_surrogate); -1 = none.
    let hiSurrogate = -1;
    for (;;) {
      const c = this.peek();
      if (c === -1) throw malformed("unterminated string");
      if (c === 0x22 /* " */) {
        if (hiSurrogate !== -1) throw escapeSyntaxError("json", LOW_SURROGATE_DETAIL);
        this.pos++;
        return out;
      }
      if (c === 0x5c /* \ */) {
        this.pos++;
        const e = this.peek();
        if (e === -1) throw malformed("unterminated escape");
        if (e === 0x75 /* u */) {
          this.pos++;
          let ch = this.parseHex4();
          if (!this.needEscapes) continue;
          if (ch >= 0xd800 && ch <= 0xdbff) {
            if (hiSurrogate !== -1) throw escapeSyntaxError("json", HIGH_SURROGATE_DETAIL);
            hiSurrogate = ch;
            continue;
          } else if (ch >= 0xdc00 && ch <= 0xdfff) {
            if (hiSurrogate === -1) throw escapeSyntaxError("json", LOW_SURROGATE_DETAIL);
            ch = 0x10000 + (((hiSurrogate - 0xd800) << 10) | (ch - 0xdc00));
            hiSurrogate = -1;
          }
          if (hiSurrogate !== -1) throw escapeSyntaxError("json", LOW_SURROGATE_DETAIL);
          if (ch === 0) throw unicodeZeroError();
          out += String.fromCodePoint(ch);
          continue; // parseHex4 already advanced pos past the 4 digits
        }
        if (hiSurrogate !== -1) throw escapeSyntaxError("json", LOW_SURROGATE_DETAIL);
        let decoded: string;
        switch (e) {
          case 0x22: // "
            decoded = '"';
            break;
          case 0x5c: // backslash
            decoded = "\\";
            break;
          case 0x2f: // /
            decoded = "/";
            break;
          case 0x62: // b
            decoded = "\b";
            break;
          case 0x66: // f
            decoded = "\f";
            break;
          case 0x6e: // n
            decoded = "\n";
            break;
          case 0x72: // r
            decoded = "\r";
            break;
          case 0x74: // t
            decoded = "\t";
            break;
          default:
            throw malformed("invalid escape sequence");
        }
        if (this.needEscapes) out += decoded;
        this.pos++;
      } else if (hiSurrogate !== -1) {
        throw escapeSyntaxError("json", LOW_SURROGATE_DETAIL);
      } else if (c <= 0x1f) {
        throw malformed("control character in string must be escaped");
      } else {
        // Copy one UTF-8 code point verbatim. Determine its byte length, decode-check it (fatal), and
        // append the decoded text.
        const len = utf8Len(c);
        const end = this.pos + len;
        if (end > this.buf.length) throw malformed("truncated UTF-8 sequence in string");
        let text: string;
        try {
          text = ASCII_DECODER.decode(this.buf.subarray(this.pos, end));
        } catch {
          throw malformed("invalid UTF-8 in string");
        }
        if (this.needEscapes) out += text;
        this.pos = end;
      }
    }
  }

  // parseHex4 reads exactly four hex digits as a code-unit (the cursor is just past `\u`). A non-hex
  // digit is PG's JSON_UNICODE_ESCAPE_FORMAT (22P02 with its Detail); running out of input first is a
  // truncated token.
  private parseHex4(): number {
    let v = 0;
    for (let i = 0; i < 4; i++) {
      if (this.pos + i >= this.buf.length) throw malformed("truncated \\u escape");
      const d = this.buf[this.pos + i]!;
      let nib: number;
      if (d >= 0x30 && d <= 0x39)
        nib = d - 0x30; // 0-9
      else if (d >= 0x61 && d <= 0x66)
        nib = d - 0x61 + 10; // a-f
      else if (d >= 0x41 && d <= 0x46)
        nib = d - 0x41 + 10; // A-F
      else throw escapeFormatError();
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

// hasDuplicateKeys detects a duplicate object key anywhere in the tree — for `IS JSON WITH UNIQUE
// KEYS` (json-sql-functions.md §5). The node must be parsed with `parsePreservingJson` (which keeps
// duplicate keys); this walks every object (and recurses into member values and array elements).
export function hasDuplicateKeys(node: JsonNode): boolean {
  switch (node.kind) {
    case "object": {
      const members = node.members;
      for (let i = 0; i < members.length; i++) {
        for (let j = i + 1; j < members.length; j++) {
          if (members[i]!.key === members[j]!.key) return true;
        }
      }
      return members.some((m) => hasDuplicateKeys(m.value));
    }
    case "array":
      return node.elements.some(hasDuplicateKeys);
    default:
      return false;
  }
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
// the canonical node tree. (The `json` overloads, which return verbatim sub-text, are the
// "Verbatim `json` navigation" section below — json.md §4.)
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
      const idx = pathStepIndex(step);
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

// pathStepIndex parses a `#>` / `#>>` path step as an array index the way PostgreSQL does
// (`strtoint(step, 10)` in get_path_all / get_jsonb_path_all): optional leading C-locale whitespace,
// an optional sign, at least one digit, nothing trailing, and the value within i32. Anything else is
// null — a step that never matches an array element (PG's INT_MIN sentinel; '1 ' and an out-of-i32
// step miss).
export function pathStepIndex(step: string): bigint | null {
  const m = /^[ \t\n\v\f\r]*([+-]?[0-9]+)$/.exec(step);
  if (m === null) return null;
  const v = BigInt(m[1]!);
  if (v < -2147483648n || v > 2147483647n) return null;
  return v;
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
// Verbatim `json` navigation (spec/design/json.md §4, json-sql-functions.md §1/§3). A `json` value is
// its validated input text; its accessors and SRFs return SUB-TEXT spans of that text exactly as
// written (whitespace, key order, duplicate keys kept), mirroring PostgreSQL's streaming get_worker /
// each_worker / elements_worker (jsonfuncs.c). The text is already well-formed (json_in validated
// it), so the scanners below only skip, never re-validate. Positions are UTF-16 code-unit offsets;
// every structural character is ASCII, so they slice the JS string exactly.
// ---------------------------------------------------------------------------------------------

// VerbatimStep is one step of a verbatim-json access path (PG's tpath[i] / ipath[i] pair): `key`
// matches an object member by its de-escaped name; `index` an array element (null never matches).
export type VerbatimStep = { key: string | null; index: bigint | null };

function isJsonWs(c: number): boolean {
  return c === 0x20 || c === 0x09 || c === 0x0a || c === 0x0d;
}

export function skipJsonWs(t: string, pos: number): number {
  while (pos < t.length && isJsonWs(t.charCodeAt(pos))) pos++;
  return pos;
}

// skipJsonString returns the end (exclusive) of the string token whose opening `"` is at pos.
function skipJsonString(t: string, pos: number): number {
  pos++;
  while (pos < t.length) {
    const c = t.charCodeAt(pos);
    if (c === 0x22 /* " */) return pos + 1;
    pos += c === 0x5c /* \ */ ? 2 : 1;
  }
  return pos;
}

// skipJsonValue returns the end (exclusive) of the well-formed value starting at pos (no leading
// whitespace).
function skipJsonValue(t: string, pos: number): number {
  const c = t.charCodeAt(pos);
  if (c === 0x22 /* " */) return skipJsonString(t, pos);
  if (c === 0x7b /* { */ || c === 0x5b /* [ */) {
    let depth = 0;
    let p = pos;
    while (p < t.length) {
      const d = t.charCodeAt(p);
      if (d === 0x22) {
        p = skipJsonString(t, p);
        continue;
      }
      if (d === 0x7b || d === 0x5b) depth++;
      else if (d === 0x7d /* } */ || d === 0x5d /* ] */) {
        depth--;
        if (depth === 0) return p + 1;
      }
      p++;
    }
    return p;
  }
  // A number / true / false / null: runs to the next delimiter.
  let p = pos;
  while (p < t.length) {
    const d = t.charCodeAt(p);
    if (isJsonWs(d) || d === 0x2c /* , */ || d === 0x5d || d === 0x7d) break;
    p++;
  }
  return p;
}

// decodeJsonString decodes the string token at pos (its opening `"`) to its content, reusing the
// parser's escape decoding. Returns [content, end].
function decodeJsonString(t: string, pos: number): [string, number] {
  const end = skipJsonString(t, pos);
  const s = new JsonParser(UTF8.encode(t.slice(pos, end)), false, true).decodeStringToken();
  return [s, end];
}

// forEachJsonMember visits the members of the object whose `{` is at pos: f(key, valueStart) must
// return the value's end. Returns the end of the object.
function forEachJsonMember(
  t: string,
  pos: number,
  f: (key: string, start: number) => number,
): number {
  let p = skipJsonWs(t, pos + 1);
  if (t.charCodeAt(p) === 0x7d /* } */) return p + 1;
  for (;;) {
    const [key, kend] = decodeJsonString(t, p);
    p = skipJsonWs(t, kend) + 1; // past ':'
    p = skipJsonWs(t, p);
    p = skipJsonWs(t, f(key, p));
    if (t.charCodeAt(p) === 0x2c /* , */) p = skipJsonWs(t, p + 1);
    else return p + 1; // past '}'
  }
}

// forEachJsonElement visits the elements of the array whose `[` is at pos: f(ordinal, valueStart)
// must return the element's end. Returns the end of the array.
function forEachJsonElement(
  t: string,
  pos: number,
  f: (ord: bigint, start: number) => number,
): number {
  let p = skipJsonWs(t, pos + 1);
  if (t.charCodeAt(p) === 0x5d /* ] */) return p + 1;
  let i = 0n;
  for (;;) {
    p = skipJsonWs(t, f(i, p));
    i++;
    if (t.charCodeAt(p) === 0x2c /* , */) p = skipJsonWs(t, p + 1);
    else return p + 1; // past ']'
  }
}

// verbatimGet is the verbatim `json` access `-> ->> #> #>>` with a NON-empty path (PG get_worker):
// the sub-text of the LAST value, in document order, whose path matches steps — an object step
// matches EVERY member of that name (duplicate keys included, so a later duplicate overrides an
// earlier match), an array step the element at its index. A negative index is resolved PG's way: on
// entering ANY array at that depth (matched or not), if the index is negative and in range it is
// rebased onto that array's length, and the rebased index then carries to every later array at the
// same depth (an observable PG quirk, pinned by the oracle). null → SQL NULL.
export function verbatimGet(text: string, steps: VerbatimStep[]): string | null {
  const n = steps.length;
  // The per-depth array index, rebased in place on a negative subscript.
  const index = steps.map((s) => s.index);
  let result: [number, number] | null = null;
  // walk walks the value at pos (nested in depth containers); pathOk = the path matched down to
  // here. Returns the value's end.
  const walk = (pos: number, depth: number, pathOk: boolean): number => {
    if (depth >= n) return skipJsonValue(text, pos);
    const c = text.charCodeAt(pos);
    if (c === 0x7b /* { */) {
      return forEachJsonMember(text, pos, (key, vstart) => {
        const matched = pathOk && steps[depth]!.key === key;
        const vend = walk(vstart, depth + 1, matched && depth + 1 < n);
        if (matched && depth + 1 === n) result = [vstart, vend];
        return vend;
      });
    }
    if (c === 0x5b /* [ */) {
      const i = index[depth]!;
      if (i !== null && i < 0n) {
        let count = 0n;
        forEachJsonElement(text, pos, (_, s) => {
          count++;
          return skipJsonValue(text, s);
        });
        if (-i <= count) index[depth] = i + count;
      }
      return forEachJsonElement(text, pos, (ord, vstart) => {
        const matched = pathOk && index[depth] === ord;
        const vend = walk(vstart, depth + 1, matched && depth + 1 < n);
        if (matched && depth + 1 === n) result = [vstart, vend];
        return vend;
      });
    }
    return skipJsonValue(text, pos);
  };
  walk(skipJsonWs(text, 0), 0, true);
  const r = result as [number, number] | null;
  return r === null ? null : text.slice(r[0], r[1]);
}

// verbatimWhole is the verbatim `json #> '{}'` / `#>> '{}'` (an EMPTY path) span — the whole
// document as PG's get_worker returns it: an object/array without its surrounding whitespace, but a
// SCALAR from the very start of the text (keeping any leading whitespace) to the end of its token.
export function verbatimWhole(text: string): string {
  const start = skipJsonWs(text, 0);
  const end = skipJsonValue(text, start);
  const c = text.charCodeAt(start);
  return c === 0x7b || c === 0x5b ? text.slice(start, end) : text.slice(0, end);
}

// verbatimToText is the `->>` / `#>>` / `_text` render of a verbatim sub-text: a JSON string → its
// de-escaped content, a JSON null → SQL NULL (null), anything else → the sub-text verbatim.
export function verbatimToText(sub: string): string | null {
  const start = skipJsonWs(sub, 0);
  const c = sub.charCodeAt(start);
  if (c === 0x22 /* " */) return decodeJsonString(sub, start)[0];
  if (c === 0x6e /* n */) return null;
  return sub;
}

// verbatimArrayElements is json_array_elements[_text](json) (PG elements_worker): the verbatim
// sub-text of each top-level array element, in order. A non-array → 22023 with PG's message. The
// _text form (deEscape) lexes with need_escapes, so a `\u0000` / unpaired surrogate raises — but an
// object root's error comes first (PG raises it at the `{`), while a scalar root is lexed before its
// own error (json.md §4.2).
export function verbatimArrayElements(text: string, fn: string, deEscape: boolean): string[] {
  const start = skipJsonWs(text, 0);
  const c = text.charCodeAt(start);
  if (c === 0x7b /* { */)
    throw engineError("invalid_parameter_value", `cannot call ${fn} on a non-array`);
  if (deEscape) checkJsonEscapes(text);
  if (c !== 0x5b /* [ */)
    throw engineError("invalid_parameter_value", `cannot call ${fn} on a scalar`);
  const out: string[] = [];
  forEachJsonElement(text, start, (_, s) => {
    const e = skipJsonValue(text, s);
    out.push(text.slice(s, e));
    return e;
  });
  return out;
}

// verbatimObjectMembers is json_each[_text](json) (PG each_worker): each top-level object member as
// its de-escaped key and verbatim value sub-text, in INPUT order with duplicate keys kept. A
// non-object → 22023 with PG's message. Both forms lex with need_escapes, so a `\u0000` / unpaired
// surrogate raises — after an array root's error (PG raises it at the `[`), before a scalar root's
// (json.md §4.2).
export function verbatimObjectMembers(text: string): { key: string; value: string }[] {
  const start = skipJsonWs(text, 0);
  const c = text.charCodeAt(start);
  if (c === 0x5b /* [ */)
    throw engineError("invalid_parameter_value", "cannot deconstruct an array as an object");
  checkJsonEscapes(text);
  if (c !== 0x7b /* { */)
    throw engineError("invalid_parameter_value", "cannot deconstruct a scalar");
  const out: { key: string; value: string }[] = [];
  forEachJsonMember(text, start, (key, s) => {
    const e = skipJsonValue(text, s);
    out.push({ key, value: text.slice(s, e) });
    return e;
  });
  return out;
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

// rootCount is the element count of a container root (`JB_ROOT_COUNT`); a scalar counts as one.
function rootCount(node: JsonNode): number {
  if (node.kind === "object") return node.members.length;
  if (node.kind === "array") return node.elements.length;
  return 1;
}

// deleteKeys is `jsonb - text[]` (PG `jsonb_delete_array`) — delete every key (object) / matching
// string element (array) named by a non-NULL element of `keys`; a NULL element is skipped. A scalar
// root is `22023` ("cannot delete from scalar") even for empty `keys`; an empty container is returned
// unchanged.
export function deleteKeys(node: JsonNode, keys: (string | null)[]): JsonNode {
  if (!isContainer(node)) throw cannotDelete("cannot delete from scalar");
  let cur = node;
  for (const k of keys) {
    if (k !== null) cur = deleteKey(cur, k);
  }
  return cur;
}

// badNullValueTreatment is jsonb_set_lax's 22023 for a NULL or unrecognized null_value_treatment
// (PostgreSQL's exact text).
export function badNullValueTreatment(): EngineError {
  return engineError(
    "invalid_parameter_value",
    'null_value_treatment must be "delete_key", "return_target", "use_json_null", or "raise_exception"',
  );
}

// nullValueRaised is jsonb_set_lax's 22004 for a SQL NULL value under null_value_treatment =>
// 'raise_exception', with PostgreSQL's Detail and Hint (error-fields.md §9.2, §11.4).
export function nullValueRaised(): EngineError {
  return engineError("null_value_not_allowed", "JSON value must not be null")
    .withDetail('Exception was raised because null_value_treatment is "raise_exception".')
    .withHint(
      "To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed.",
    );
}

// PathOp is the walk a path mutation performs — PostgreSQL's setPath op_type bits, restricted to the
// SQL functions (the subscript-assignment FILL_GAPS / CONSISTENT_POSITION bits never arise here):
// replace = jsonb_set without create_if_missing, create = with it, insertBefore / insertAfter =
// jsonb_insert, delete = `#-` / jsonb_set_lax 'delete_key'.
type PathOp = "replace" | "create" | "insertBefore" | "insertAfter" | "delete";

// addsMissing is JB_PATH_CREATE_OR_INSERT: whether a missing final key / index is added.
function addsMissing(op: PathOp): boolean {
  return op === "create" || op === "insertBefore" || op === "insertAfter";
}

// deletePath is `jsonb #- text[]` (PG `jsonb_delete_path`) — delete the element at a path. A scalar
// root is `22023` ("cannot delete path in scalar"), even for an empty path; an empty container or an
// empty path returns the target unchanged; otherwise the setPathWalk rules apply (a NULL path element
// reached → `22004`, a non-integer step into an array → `22P02`, a missing / out-of-range / scalar
// step → no-op).
export function deletePath(node: JsonNode, path: (string | null)[]): JsonNode {
  if (!isContainer(node)) throw cannotDelete("cannot delete path in scalar");
  if (rootCount(node) === 0 || path.length === 0) return node;
  return setPathWalk(node, path, 0, { kind: "null" }, "delete");
}

// PathSetMode selects whether a path mutation REPLACES at the final step (`jsonb_set`) or INSERTS a
// new element (`jsonb_insert`). For Insert, the flag is `insert_after` (place after the array index,
// not before); for Set, the flag is `create_if_missing` (add a missing final key / out-of-range
// index).
export type PathSetMode = "set" | "insert";

// setPath is `jsonb_set(target, path, value[, create_if_missing])` (json-sql-functions.md §2, PG
// `jsonb_set`): set the value at `path` (a text[] of object keys / array indices). A scalar root is
// `22023` ("cannot set path in scalar"), even for an empty path; an empty container without `create`,
// or an empty path, returns the target unchanged. At the final step an existing element is REPLACED
// and a missing one is added only when `create` (an out-of-range index appends; one before the start
// prepends). See setPathWalk for the per-step rules.
export function setPath(
  node: JsonNode,
  path: (string | null)[],
  value: JsonNode,
  create: boolean,
): JsonNode {
  if (!isContainer(node)) throw cannotDelete("cannot set path in scalar");
  if ((rootCount(node) === 0 && !create) || path.length === 0) return node;
  return setPathWalk(node, path, 0, value, create ? "create" : "replace");
}

// insertPath is `jsonb_insert(target, path, value[, insert_after])` (json-sql-functions.md §2, PG
// `jsonb_insert`): like setPath but the final step INSERTS — an existing object key is `22023`
// ("cannot replace existing key"); an array index inserts before it (or after, when `insert_after`).
// A scalar root is `22023` ("cannot set path in scalar"); an empty path returns the target unchanged
// (an empty container does NOT short-circuit).
export function insertPath(
  node: JsonNode,
  path: (string | null)[],
  value: JsonNode,
  insertAfter: boolean,
): JsonNode {
  if (!isContainer(node)) throw cannotDelete("cannot set path in scalar");
  if (path.length === 0) return node;
  return setPathWalk(node, path, 0, value, insertAfter ? "insertAfter" : "insertBefore");
}

// pathStepInt32 is PostgreSQL's `strtoint(step, &end, 10)` acceptance for an array path step:
// optional leading C whitespace, an optional sign, then one or more ASCII digits to the end, within
// int32. null for anything else (no digits, trailing characters, or out of int32 range).
function pathStepInt32(step: string): number | null {
  const m = /^[ \t\n\v\f\r]*([+-]?)([0-9]+)$/.exec(step);
  if (m === null) return null;
  let mag = 0;
  for (const c of m[2]!) {
    mag = mag * 10 + (c.charCodeAt(0) - 48);
    if (mag > 2 ** 31) return null; // beyond every int32 magnitude
  }
  const v = m[1] === "-" ? -mag : mag;
  if (v < -(2 ** 31) || v > 2 ** 31 - 1) return null;
  return v === 0 ? 0 : v; // normalize -0
}

// setPathWalk is the recursive step of a path mutation (PG setPath / setPathObject / setPathArray), at
// path position `level` (< path.length). A NULL step reached is `22004` ("path element at position N
// is null", 1-based) — checked before the node is inspected, so a NULL step under a scalar still
// raises, while one past a missing key / out-of-range index is never reached. A scalar node below the
// root is returned unchanged. Object: the key matching the step is replaced / deleted at the final
// step (an insert op on an existing key is `22023` "cannot replace existing key") or descended into; a
// missing final key is added by the adding ops. Array: the step must be an int32 (else `22P02` "path
// element at position N is not an integer"); a negative index counts from the end, one before the
// start becomes a prepend for the adding ops, one past the end clamps to the length (an append); a
// non-final missing index is a no-op.
function setPathWalk(
  node: JsonNode,
  path: (string | null)[],
  level: number,
  value: JsonNode,
  op: PathOp,
): JsonNode {
  const step = path[level]!;
  if (step === null) {
    throw engineError("null_value_not_allowed", `path element at position ${level + 1} is null`);
  }
  const isFinal = level + 1 === path.length;
  switch (node.kind) {
    case "object": {
      let out: JsonMember[] = [];
      let done = false;
      for (const mem of node.members) {
        if (!done && mem.key === step) {
          done = true;
          if (isFinal) {
            if (op === "insertBefore" || op === "insertAfter") {
              throw cannotDelete("cannot replace existing key").withHint(
                "Try using the function jsonb_set to replace key value.",
              );
            }
            if (op !== "delete") out.push({ key: mem.key, value });
          } else {
            out.push({ key: mem.key, value: setPathWalk(mem.value, path, level + 1, value, op) });
          }
        } else {
          out.push(mem);
        }
      }
      if (!done && isFinal && addsMissing(op)) {
        // A missing final key is added, sorted into canonical place.
        out.push({ key: step, value });
        out = canonicalizeObject(out);
      }
      return { kind: "object", members: out };
    }
    case "array": {
      const parsed = pathStepInt32(step);
      if (parsed === null) {
        throw engineError(
          "invalid_text_representation",
          `path element at position ${level + 1} is not an integer: "${step}"`,
        );
      }
      const nelems = node.elements.length;
      // before is PG's INT_MIN sentinel: a negative index before the start (matches nothing; an
      // adding op prepends).
      let idx = parsed;
      let before = false;
      if (parsed < 0) {
        if (-parsed > nelems) before = true;
        else idx = nelems + parsed;
      }
      if (!before && idx > nelems) idx = nelems;
      const out: JsonNode[] = [];
      let done = false;
      if ((before || nelems === 0) && isFinal && addsMissing(op)) {
        out.push(value);
        done = true;
      }
      for (let i = 0; i < nelems; i++) {
        const el = node.elements[i]!;
        if (!before && idx === i) {
          done = true;
          if (isFinal) {
            if (op === "insertBefore" || op === "create") out.push(value);
            if (op === "insertBefore" || op === "insertAfter") out.push(el);
            if (op === "insertAfter" || op === "replace") out.push(value);
          } else {
            out.push(setPathWalk(el, path, level + 1, value, op));
          }
        } else {
          out.push(el);
        }
      }
      if (!done && isFinal && addsMissing(op)) out.push(value);
      return { kind: "array", elements: out };
    }
    default:
      // A scalar below the root: the path cannot continue, so the value stays (PG).
      return node;
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

// prettySize is the exact UTF-8 byte length of pretty's rendering, computed without building it: the
// jsonb_pretty amplifier sizes its output before allocating (spec/design/cost.md §8.1). Mirrors
// writePretty: brackets, a newline plus four spaces per level before each member and the close,
// commas, and ": " plus the escaped key per object member. Each scalar and key is rendered on its own
// to measure it; that scratch is bounded by the input.
export function prettySize(node: JsonNode): bigint {
  return BigInt(prettySizeAt(node, 0));
}

function renderedLength(write: (out: string[]) => void): number {
  const out: string[] = [];
  write(out);
  let n = 0;
  for (const part of out) n += utf8Length(part);
  return n;
}

function prettySizeAt(node: JsonNode, indent: number): number {
  // The open bracket, then the close on its own line at this container's indent.
  const shell = 1 + 1 + 4 * indent + 1;
  // Before each member: a comma after the first, then a newline and the member's indent.
  const lead = (i: number): number => (i > 0 ? 1 : 0) + 1 + 4 * (indent + 1);
  switch (node.kind) {
    case "object": {
      let size = shell;
      node.members.forEach((m, i) => {
        size +=
          lead(i) +
          renderedLength((out) => writeJsonString(m.key, out)) +
          2 +
          prettySizeAt(m.value, indent + 1);
      });
      return size;
    }
    case "array": {
      let size = shell;
      node.elements.forEach((e, i) => {
        size += lead(i) + prettySizeAt(e, indent + 1);
      });
      return size;
    }
    default:
      return renderedLength((out) => writeNode(node, out));
  }
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
