// Cross-check: the hand-written type and error constants in the TS core must match the
// canonical spec data tables (CLAUDE.md §5). If the spec changes and the core doesn't
// (or vice versa), this fails.

import assert from "node:assert/strict";
import { test } from "node:test";
import { type SqlState, sqlStateCode } from "../src/errors.ts";
import {
  type ScalarType,
  canonicalName,
  maxOf,
  minOf,
  rank,
  scalarTypeFromName,
  widthBytes,
} from "../src/types.ts";
import { OPERATORS } from "../src/operators.ts";
import { readTomlTables, specPath } from "./tomlmini.ts";

test("scalar types match spec/types/scalars.toml", () => {
  const rows = readTomlTables(specPath("types/scalars.toml"), "type");
  assert.equal(rows.length, 3, "expected 3 scalar types");
  for (const row of rows) {
    const id = row.str("id");
    const st = scalarTypeFromName(id);
    assert.notEqual(st, undefined, `unknown type id ${id}`);
    const t = st as ScalarType;
    assert.equal(canonicalName(t), id, `${id}: canonical name`);
    assert.equal(BigInt(widthBytes(t) * 8), row.big("bits"), `${id}: bits`);
    assert.equal(minOf(t), row.big("min"), `${id}: min`);
    assert.equal(maxOf(t), row.big("max"), `${id}: max`);
    assert.equal(rank(t), row.num("rank"), `${id}: rank`);
    for (const alias of row.strs("aliases")) {
      assert.equal(scalarTypeFromName(alias), t, `alias ${alias} should resolve to ${id}`);
    }
  }
});

test("error codes are registered in spec/errors/registry.toml", () => {
  const rows = readTomlTables(specPath("errors/registry.toml"), "error");
  const codes = new Map<string, string>(); // code -> name
  for (const row of rows) codes.set(row.str("code"), row.str("name"));

  const states: SqlState[] = [
    "numeric_value_out_of_range",
    "not_null_violation",
    "unique_violation",
    "syntax_error",
    "undefined_table",
    "undefined_column",
    "undefined_object",
    "datatype_mismatch",
    "duplicate_table",
    "duplicate_column",
    "invalid_table_definition",
    "feature_not_supported",
    "data_corrupted",
  ];
  for (const st of states) {
    assert.ok(codes.has(sqlStateCode(st)), `code ${sqlStateCode(st)} missing from registry`);
    // The union member is the registry's snake_case name; cross-check that too.
    assert.equal(codes.get(sqlStateCode(st)), st, `name for ${sqlStateCode(st)}`);
  }
  assert.equal(codes.get("22003"), "numeric_value_out_of_range");
  assert.equal(sqlStateCode("numeric_value_out_of_range"), "22003");
});

test("operators match spec/functions/catalog.toml", () => {
  // The generated operator descriptor table (codegen middle path, CLAUDE.md §5) must
  // match the canonical catalog field-for-field.
  const rows = readTomlTables(specPath("functions/catalog.toml"), "operator");
  assert.equal(rows.length, OPERATORS.length, "operator count");
  const byName = new Map(OPERATORS.map((d) => [d.name, d]));
  for (const row of rows) {
    const name = row.str("name");
    const desc = byName.get(name);
    assert.notEqual(desc, undefined, `generated table missing operator ${name}`);
    const d = desc!;
    assert.equal(d.kind, row.str("kind"), `${name}: kind`);
    assert.equal(d.arity, row.num("arity"), `${name}: arity`);
    assert.equal(d.argResolution, row.str("arg_resolution"), `${name}: arg_resolution`);
    assert.equal(d.result, row.str("result"), `${name}: result`);
    assert.equal(d.null, row.str("null"), `${name}: null`);
    assert.deepEqual([...d.argFamilies], row.strs("arg_families"), `${name}: argFamilies`);
    assert.deepEqual([...d.errors], row.strs("errors"), `${name}: errors`);
    if (row.str("kind") === "comparison") {
      assert.equal(d.symbol, row.str("symbol"), `${name}: symbol`);
    } else {
      assert.equal(d.symbol, undefined, `${name}: symbol absent`);
    }
  }
});
