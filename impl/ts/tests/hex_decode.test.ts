// Host-only UTF-16/error and mutable-buffer invariants. SQL values, accepted syntax and cost
// are covered by the shared string_functions corpus; that corpus cannot contain lone surrogates.
import assert from "node:assert/strict";
import { test } from "node:test";
import { decodeText } from "../src/executor.ts";
import { EngineError, queryOutcome } from "../src/tooling.ts";
import { decodeHexPairs, decodeHexString } from "../src/value.ts";
import { memDb } from "./mem_db.ts";

test("hex string decoding preserves the byte kernel for arbitrary host UTF-16", () => {
  const encoder = new TextEncoder();
  const characters = [
    ...Array.from({ length: 128 }, (_, i) => String.fromCharCode(i)),
    "\u0080",
    "\u00ff",
    "\u0100",
    "\u07ff",
    "\u0800",
    "\u2028",
    "\ufeff",
    "\ufffd",
    "😀",
    "\ud800",
    "\udfff",
    "\ud800\ud800",
    "\udfff\ud800",
  ];
  for (const character of characters) {
    for (const [before, after] of [
      ["", ""],
      ["0", ""],
      ["00 ", "00"],
      ["z", "0"],
      ["0 ", ""],
    ]) {
      const input = before + character + after;
      const expected = decodeHexPairs(encoder.encode(input));
      assert.deepEqual(decodeHexString(input), expected, JSON.stringify(input));
      if ("error" in expected) {
        assert.throws(
          () => decodeText(input, "hex"),
          (error: unknown) =>
            error instanceof EngineError &&
            error.code() === "22023" &&
            error.message === `22023: ${expected.error}`,
          JSON.stringify(input),
        );
      } else {
        assert.deepEqual(decodeText(input, "hex"), expected.bytes, JSON.stringify(input));
      }
    }
  }
});

test("hex decoding returns independently owned bytes for every result row", () => {
  const db = memDb();
  const session = db.session();
  try {
    const result = queryOutcome(
      session,
      "SELECT decode('0001feff', 'hex') FROM generate_series(1, 3)",
    );
    assert.equal(result.kind, "query");
    if (result.kind !== "query") throw new Error("expected rows");
    const values = result.rows.map((row) => {
      const value = row[0]!;
      if (value.kind !== "bytea") throw new Error("expected bytes");
      return value.bytes;
    });
    assert.notEqual(values[0]!.buffer, values[1]!.buffer);
    assert.notEqual(values[1]!.buffer, values[2]!.buffer);
    values[0]![0] = 42;
    assert.deepEqual(values[1], new Uint8Array([0, 1, 254, 255]));
    assert.deepEqual(values[2], new Uint8Array([0, 1, 254, 255]));
  } finally {
    session.close();
    db.close();
  }
});
