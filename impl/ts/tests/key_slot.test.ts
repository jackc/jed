// The unified key-slot skipper over every ordered slot in spec/encoding/ordered.toml (indexes.md
// §5.1/§11, encoding.md §2.16): for each scalar group's four orders, the skip recognizes the order's
// NULL tag and steps over the (DESC: inverted) body exactly, with trailing bytes after the slot — the
// storage-key suffix a real entry carries. Byte-level skipping is out of the corpus's reach
// (CLAUDE.md §10). Mirrors impl/go/key_slot_test.go and the Rust key_slot_ordered_vector_tests.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { KeyOrder } from "../src/encoding.ts";
import { type KeySlot, keySlotForScalar, keySlotLen, keySlotWithOrder } from "../src/executor.ts";
import { scalarTypeFromName } from "../src/types.ts";
import { specPath } from "./tomlmini.ts";

const ORDERS: Record<string, KeyOrder> = {
  asc_nulls_last: { desc: false, nullsFirst: false },
  asc_nulls_first: { desc: false, nullsFirst: true },
  desc_nulls_first: { desc: true, nullsFirst: true },
  desc_nulls_last: { desc: true, nullsFirst: false },
};

test("keySlotLen skips every ordered vector", () => {
  const text = readFileSync(specPath("encoding/ordered.toml"), "utf8");
  const field =
    /(asc_nulls_last|asc_nulls_first|desc_nulls_first|desc_nulls_last) = "([0-9a-f]*)"/g;
  let slot: KeySlot | null = null;
  let checked = 0;
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line.startsWith("type = ")) {
      const name = line.slice(7).replace(/"/g, "");
      if (name.startsWith("collated text")) slot = { kind: "collated" };
      else if (name.includes("range") || name.includes("[]") || name.includes("(")) slot = null;
      else {
        const ty = scalarTypeFromName(name);
        assert.ok(ty !== undefined, `unknown type ${name}`);
        slot = keySlotForScalar(ty, false);
      }
      continue;
    }
    if (slot === null || !line.startsWith("{")) continue;
    for (const m of line.matchAll(field)) {
      const body = Uint8Array.from(m[2]!.match(/../g) ?? [], (h) => Number.parseInt(h, 16));
      const key = new Uint8Array(body.length + 4);
      key.set(body);
      key.set([0x00, 0x01, 0xfe, 0xff], body.length);
      assert.equal(
        keySlotLen(keySlotWithOrder(slot, ORDERS[m[1]!]!), key, 0),
        body.length,
        `${line} ${m[1]}`,
      );
      checked++;
    }
  }
  assert.ok(checked >= 200, `only ${checked} slots checked`);
});
