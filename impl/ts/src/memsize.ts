// The logical query-memory size schedule (spec/design/memory.md §3).
//
// Representation-independent byte counts the query-memory account reserves and releases. They are a
// pure function of a value's logical content — never of the JS object layout, a leaf's physical state,
// or whether a column was decoded — so a 54P05 abort is cross-core identical. The constants are
// codegen'd from spec/cost/schedule.toml [memory]; the shared vectors in spec/cost/memory_sizes.toml
// pin the measurement (tests/memory_sizes.test.ts). Plain numbers: the live account stays far below
// 2^53.

import { MEMORY_ARRAY_DIM, MEMORY_DECIMAL_GROUP, MEMORY_ROW, MEMORY_VALUE } from "./costs.ts";
import type { JsonNode } from "./json.ts";
import type { Value } from "./value.ts";

// utf8Length is the UTF-8 byte length of a JS (UTF-16) string, measured without allocating an encoded
// copy. A surrogate pair is one 4-byte code point; a lone surrogate encodes as U+FFFD (3 bytes), the
// TextEncoder behavior.
export function utf8Length(s: string): number {
  let n = 0;
  const len = s.length;
  for (let i = 0; i < len; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x80) n += 1;
    else if (c < 0x800) n += 2;
    else if (c >= 0xd800 && c <= 0xdbff && i + 1 < len) {
      const d = s.charCodeAt(i + 1);
      if (d >= 0xdc00 && d <= 0xdfff) {
        n += 4;
        i++;
      } else n += 3;
    } else n += 3;
  }
  return n;
}

// valueBytes is value_bytes(v) = VALUE + payload(v). A hole (undefined — never a touched slot) is a
// bare node, like NULL.
export function valueBytes(v: Value | undefined): number {
  if (v === undefined) return MEMORY_VALUE;
  switch (v.kind) {
    case "text":
    case "json":
    case "jsonpath":
      return MEMORY_VALUE + utf8Length(v.text);
    case "bytea":
      return MEMORY_VALUE + v.bytes.length;
    case "decimal":
      // The decimal's limbs ARE the codec's base-10^4 groups (no high zero limb; empty for zero).
      return MEMORY_VALUE + MEMORY_DECIMAL_GROUP * v.dec.limbs.length;
    case "array": {
      let n = MEMORY_VALUE + MEMORY_ARRAY_DIM * v.dims.length;
      for (const e of v.elements) n += valueBytes(e);
      return n;
    }
    case "composite": {
      let n = MEMORY_VALUE;
      for (const f of v.fields) n += valueBytes(f);
      return n;
    }
    case "range": {
      let n = MEMORY_VALUE;
      if (v.lower !== null) n += valueBytes(v.lower);
      if (v.upper !== null) n += valueBytes(v.upper);
      return n;
    }
    case "jsonb":
      return jsonbBytes(v.node);
    default:
      // NULL, fixed-width scalars, and an unfetched reference (only ever an untouched slot) are a
      // bare node.
      return MEMORY_VALUE;
  }
}

// jsonbBytes is one VALUE per jsonb node plus its string/key bytes and number groups.
function jsonbBytes(node: JsonNode): number {
  switch (node.kind) {
    case "string":
      return MEMORY_VALUE + utf8Length(node.value);
    case "number":
      return MEMORY_VALUE + MEMORY_DECIMAL_GROUP * node.dec.limbs.length;
    case "array": {
      let n = MEMORY_VALUE;
      for (const e of node.elements) n += jsonbBytes(e);
      return n;
    }
    case "object": {
      let n = MEMORY_VALUE;
      for (const m of node.members) n += utf8Length(m.key) + jsonbBytes(m.value);
      return n;
    }
    default:
      return MEMORY_VALUE;
  }
}

// rowBytes is row_bytes(row) = ROW + Σ value_bytes — a projected row, every slot a computed value.
export function rowBytes(row: Value[]): number {
  let n = MEMORY_ROW;
  for (let i = 0; i < row.length; i++) n += valueBytes(row[i]);
  return n;
}

// rowBytesMasked measures a pre-projection row under the plan's touched mask (memory.md §3): an
// untouched slot (i < mask.length && !mask[i]) is a bare VALUE whatever its physical form; touched and
// appended slots count in full.
export function rowBytesMasked(row: Value[], mask: boolean[]): number {
  let n = MEMORY_ROW;
  for (let i = 0; i < row.length; i++) {
    n += i < mask.length && !mask[i] ? MEMORY_VALUE : valueBytes(row[i]);
  }
  return n;
}
