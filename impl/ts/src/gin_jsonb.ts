// GIN `jsonb_ops` term extraction (spec/design/gin.md §2.1/§4.1) — PostgreSQL's default jsonb GIN
// operator class (`gin_extract_jsonb` / `gin_extract_jsonb_query`), with jed's own byte form for
// each term:
//
//   term = flag ‖ body
//   flag = 0x01 key │ 0x02 null │ 0x03 boolean │ 0x04 number │ 0x05 string value
//          (│ 0x10 when a key / number / string body source was cut to its first 128 bytes)
//   body = null: empty; boolean: 0x00 / 0x01;
//          key / string: text-terminated-escape (encoding.md §2.4) over the (cut) UTF-8 bytes;
//          number: text-terminated-escape over the (cut) decimal-order-preserving key (§2.5).
//
// Hand-written per core (CLAUDE.md §5), checked byte-for-byte against
// spec/encoding/gin_jsonb_terms.toml. The TS hazard: a JS string is UTF-16, so the 128-byte cut is
// taken over the TextEncoder UTF-8 bytes — never over code units — and may split a character (a term
// is bytes, never decoded).

import { encodeTerminated } from "./encoding.ts";
import type { JsonNode } from "./json.ts";

// GIN_JSONB_MAX is the byte cut for a key / string / number body source (gin.md §4.1).
const GIN_JSONB_MAX = 128;

const FLAG_KEY = 0x01;
const FLAG_NULL = 0x02;
const FLAG_BOOL = 0x03;
const FLAG_NUM = 0x04;
const FLAG_STR = 0x05;
const FLAG_CUT = 0x10;

const UTF8 = new TextEncoder();

// framed builds `flag ‖ text-terminated-escape(content)`, cutting content to its first 128 bytes and
// setting the 0x10 flag bit when it is longer.
function framed(flag: number, content: Uint8Array): Uint8Array {
  let src = content;
  let f = flag;
  if (src.length > GIN_JSONB_MAX) {
    f |= FLAG_CUT;
    src = src.subarray(0, GIN_JSONB_MAX);
  }
  const body = encodeTerminated(src);
  const out = new Uint8Array(1 + body.length);
  out[0] = f;
  out.set(body, 1);
  return out;
}

// ginJsonbKeyTerm is the KEY term of a string — an object key, a string array element (or top-level
// string), or a `?` / `?|` / `?&` query key.
export function ginJsonbKeyTerm(key: string): Uint8Array {
  return framed(FLAG_KEY, UTF8.encode(key));
}

// scalarTerm is a scalar's term; asKey marks a string that is an array element (or the top-level
// scalar), which PostgreSQL indexes as a KEY.
function scalarTerm(n: JsonNode, asKey: boolean): Uint8Array {
  switch (n.kind) {
    case "null":
      return Uint8Array.of(FLAG_NULL);
    case "bool":
      return Uint8Array.of(FLAG_BOOL, n.value ? 0x01 : 0x00);
    case "number":
      return framed(FLAG_NUM, n.dec.encodeKey());
    case "string":
      return framed(asKey ? FLAG_KEY : FLAG_STR, UTF8.encode(n.value));
    default:
      throw new Error("scalarTerm: a container has no term");
  }
}

function isContainer(n: JsonNode): boolean {
  return n.kind === "object" || n.kind === "array";
}

function walk(n: JsonNode, out: Uint8Array[]): void {
  if (n.kind === "object") {
    for (const m of n.members) {
      out.push(ginJsonbKeyTerm(m.key));
      if (isContainer(m.value)) walk(m.value, out);
      else out.push(scalarTerm(m.value, false));
    }
  } else if (n.kind === "array") {
    for (const e of n.elements) {
      if (isContainer(e)) walk(e, out);
      else out.push(scalarTerm(e, e.kind === "string"));
    }
  }
}

// cmpBytes is unsigned lexicographic byte order.
function cmpBytes(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    if (a[i]! !== b[i]!) return a[i]! - b[i]!;
  }
  return a.length - b.length;
}

// ginJsonbIndexTerms is a document's index term SET (extract_index_terms, gin.md §2.1): a key term
// for every object key and every string array element (including a top-level string — a raw-scalar
// one-element array), a value term for every other scalar, at every nesting level; containers emit
// nothing of their own. Deduplicated by bytes, ascending byte order. `{}` / `[]` yield none.
export function ginJsonbIndexTerms(doc: JsonNode): Uint8Array[] {
  const raw: Uint8Array[] = [];
  if (isContainer(doc)) walk(doc, raw);
  else raw.push(scalarTerm(doc, doc.kind === "string"));
  raw.sort(cmpBytes);
  const out: Uint8Array[] = [];
  for (const t of raw) {
    if (out.length === 0 || cmpBytes(out[out.length - 1]!, t) !== 0) out.push(t);
  }
  return out;
}
