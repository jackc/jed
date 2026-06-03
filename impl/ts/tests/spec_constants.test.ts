// Cross-check: the hand-written type and error constants in the TS core must match the
// canonical spec data tables (CLAUDE.md §5). If the spec changes and the core doesn't
// (or vice versa), this fails.

import assert from "node:assert/strict";
import { test } from "node:test";
import { type SqlState, sqlStateCode } from "../src/errors.ts";
import {
  type ScalarType,
  canonicalName,
  isBooleanTypeName,
  maxOf,
  minOf,
  rank,
  scalarTypeFromName,
  widthBytes,
} from "../src/types.ts";
import { OPERATORS } from "../src/operators.ts";
import { COSTS } from "../src/costs.ts";
import { readTomlTables, specPath } from "./tomlmini.ts";

test("scalar types match spec/types/scalars.toml", () => {
  const rows = readTomlTables(specPath("types/scalars.toml"), "type");

  // The storable scalar types are exactly the three integers; each maps to a ScalarType
  // with matching width/range/rank (CLAUDE.md §5 cross-check).
  const integers = rows.filter((r) => r.str("family") === "integer");
  assert.equal(integers.length, 3, "expected 3 storable integer scalar types");
  for (const row of integers) {
    const id = row.str("id");
    const st = scalarTypeFromName(id);
    assert.notEqual(st, undefined, `unknown type id ${id}`);
    const t = st as ScalarType;
    assert.equal(canonicalName(t), id, `${id}: canonical name`);
    assert.ok(row.bool("storable"), `${id}: should be storable`);
    assert.equal(BigInt(widthBytes(t) * 8), row.big("bits"), `${id}: bits`);
    assert.equal(minOf(t), row.big("min"), `${id}: min`);
    assert.equal(maxOf(t), row.big("max"), `${id}: max`);
    assert.equal(rank(t), row.num("rank"), `${id}: rank`);
    for (const alias of row.strs("aliases")) {
      assert.equal(scalarTypeFromName(alias), t, `alias ${alias} should resolve to ${id}`);
    }
  }

  // boolean is the first non-integer scalar: expression-only (storable = false), so it
  // is NOT a column ScalarType, only a recognized non-storable type name.
  const boolean = rows.find((r) => r.str("id") === "boolean");
  assert.notEqual(boolean, undefined, "boolean type present");
  assert.equal(boolean!.str("family"), "boolean", "boolean family");
  assert.equal(boolean!.bool("storable"), false, "boolean is not storable this slice");
  assert.equal(scalarTypeFromName("boolean"), undefined, "boolean is not a column type");
  assert.equal(scalarTypeFromName("bool"), undefined, "bool is not a column type");
  assert.ok(isBooleanTypeName("boolean") && isBooleanTypeName("BOOL"), "boolean name recognized");
});

test("error codes are registered in spec/errors/registry.toml", () => {
  const rows = readTomlTables(specPath("errors/registry.toml"), "error");
  const codes = new Map<string, string>(); // code -> name
  for (const row of rows) codes.set(row.str("code"), row.str("name"));

  const states: SqlState[] = [
    "numeric_value_out_of_range",
    "division_by_zero",
    "invalid_row_count_in_limit_clause",
    "invalid_row_count_in_offset_clause",
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
  // Operators are overloaded across operand families (one row per (name, arg_families) —
  // e.g. `eq` for integer and for text), so match on the full signature, not the name.
  for (const row of rows) {
    const name = row.str("name");
    const fams = row.strs("arg_families");
    const desc = OPERATORS.find(
      (d) => d.name === name && [...d.argFamilies].join(",") === fams.join(","),
    );
    assert.notEqual(desc, undefined, `generated table missing operator ${name} ${fams.join(",")}`);
    const d = desc!;
    assert.equal(d.kind, row.str("kind"), `${name}: kind`);
    assert.equal(d.arity, row.num("arity"), `${name}: arity`);
    assert.equal(d.argResolution, row.str("arg_resolution"), `${name}: arg_resolution`);
    assert.equal(d.result, row.str("result"), `${name}: result`);
    assert.equal(d.null, row.str("null"), `${name}: null`);
    assert.equal(d.precedence, row.has("precedence") ? row.num("precedence") : 0, `${name}: precedence`);
    assert.deepEqual([...d.argFamilies], row.strs("arg_families"), `${name}: argFamilies`);
    assert.deepEqual([...d.errors], row.strs("errors"), `${name}: errors`);
    if (row.has("symbol")) {
      assert.equal(d.symbol, row.str("symbol"), `${name}: symbol`);
    } else {
      assert.equal(d.symbol, undefined, `${name}: symbol absent`);
    }
  }
});

test("cost schedule matches spec/cost/schedule.toml", () => {
  // The generated cost schedule (codegen middle path, CLAUDE.md §5/§13) must match the
  // canonical schedule.toml weight-for-weight. Cost is a cross-core contract (§8): every
  // core reads these weights.
  const rows = readTomlTables(specPath("cost/schedule.toml"), "unit");
  assert.equal(rows.length, 3, "the three phase-1 cost units");
  // Every unit id maps to a field on COSTS; a new unit forces this cross-check to be
  // updated (so a core cannot silently ignore a unit the schedule adds).
  const weight = (id: string): bigint => {
    switch (id) {
      case "storage_row_read":
        return COSTS.storageRowRead;
      case "row_produced":
        return COSTS.rowProduced;
      case "operator_eval":
        return COSTS.operatorEval;
      default:
        throw new Error(`cost unit ${id} has no COSTS field — update this cross-check`);
    }
  };
  for (const row of rows) {
    const id = row.str("id");
    assert.equal(weight(id), row.big("weight"), `${id}: weight`);
  }
});
