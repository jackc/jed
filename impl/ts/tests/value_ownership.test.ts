import assert from "node:assert/strict";
import { test } from "node:test";
import { runSync } from "../src/execution.ts";
import { arrayValue, byteaValue, type Value } from "../src/value.ts";
import { memDb } from "./mem_db.ts";

test("native byte buffers are isolated on bind and result boundaries", () => {
  const db = memDb();
  db.run("CREATE TABLE buffers (id bigint PRIMARY KEY, bytes bytea UNIQUE)");
  const input = new Uint8Array([1, 2]);
  db.prepare("INSERT INTO buffers VALUES (1, $1)").run(input);
  const version = db.txid;
  input[0] = 9;
  const row = db.get("SELECT bytes FROM buffers WHERE id = 1")!;
  assert.deepStrictEqual(row.bytes, new Uint8Array([1, 2]));
  (row.bytes as Uint8Array)[0] = 8;
  assert.deepStrictEqual(
    db.get("SELECT bytes FROM buffers WHERE id = 1")!.bytes,
    new Uint8Array([1, 2]),
  );
  assert.deepStrictEqual(db.get("SELECT id FROM buffers WHERE bytes = decode('0102', 'hex')"), {
    id: 1n,
  });
  assert.equal(db.txid, version);
});

test("raw nested values and both cursor drivers isolate mutable payloads", () => {
  const db = memDb();
  db.run("CREATE TABLE nested_buffers (id bigint PRIMARY KEY, bytes bytea[])");
  const bytes = new Uint8Array([3, 4]);
  const input = arrayValue([byteaValue(bytes)]);
  db.executePrepared(db.prepareStatement("INSERT INTO nested_buffers VALUES (1, $1)"), [input]);
  bytes[0] = 9;
  if (input.kind === "array") input.elements.length = 0;
  for (const asyncDriver of [false, true]) {
    const rows = db.query("SELECT bytes FROM nested_buffers");
    try {
      const next = asyncDriver ? runSync(rows.nextSteps()) : rows[Symbol.iterator]().next();
      assert.equal(next.done, false);
      const value = next.value![0]!;
      assert.equal(value.kind, "array");
      if (value.kind !== "array") throw new Error("expected array");
      const bytea = value.elements[0]!;
      assert.equal(bytea.kind, "bytea");
      if (bytea.kind !== "bytea") throw new Error("expected bytea");
      assert.deepStrictEqual(bytea.bytes, new Uint8Array([3, 4]));
      bytea.bytes[0] = 8;
      value.elements.length = 0;
      value.dims[0] = 0;
    } finally {
      rows.close();
    }
  }
  assert.equal(db.get("SELECT get_byte(bytes[1], 0) AS first FROM nested_buffers")!.first, 3n);
});

test("deferred cursors snapshot bind values before their first pull", () => {
  const db = memDb();
  const bytes = new Uint8Array([5, 6]);
  const params: Value[] = [byteaValue(bytes)];
  const rows = db.query("SELECT $1::bytea UNION ALL SELECT $1::bytea", params);
  bytes[0] = 9;
  params.length = 0;
  try {
    for (const row of rows) {
      assert.deepStrictEqual(row[0], byteaValue(new Uint8Array([5, 6])));
      if (row[0]!.kind === "bytea") row[0]!.bytes[0] = 8;
    }
  } finally {
    rows.close();
  }
});
