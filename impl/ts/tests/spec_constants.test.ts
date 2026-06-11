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
import { MAX_INT_DIGITS, MAX_PRECISION, MAX_SCALE } from "../src/decimal.ts";
import { AGGREGATES, OPERATORS } from "../src/operators.ts";
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

  // boolean is a storable non-integer scalar (storable = true): it resolves to a column
  // ScalarType, canonical-names to "boolean", and its aliases resolve. It has no integer
  // fields (bits/min/max/rank), so those accessors are not exercised here.
  const boolean = rows.find((r) => r.str("id") === "boolean");
  assert.notEqual(boolean, undefined, "boolean type present");
  assert.equal(boolean!.str("family"), "boolean", "boolean family");
  assert.ok(boolean!.bool("storable"), "boolean is storable this slice");
  const boolTy = scalarTypeFromName("boolean");
  assert.notEqual(boolTy, undefined, "boolean resolves to a ScalarType");
  assert.equal(canonicalName(boolTy as ScalarType), "boolean", "boolean canonical name");
  for (const alias of boolean!.strs("aliases")) {
    assert.equal(scalarTypeFromName(alias), boolTy, `alias ${alias} should resolve to boolean`);
  }

  // text: storable; its aliases resolve. decimal: storable, the decimal family; aliases
  // resolve; caps match the decimal module's constants (a cross-core contract, decimal.md §2).
  const text = rows.find((r) => r.str("id") === "text");
  assert.equal(text!.bool("storable"), true, "text storable");
  assert.equal(scalarTypeFromName("text"), "text");

  const decimal = rows.find((r) => r.str("id") === "decimal");
  assert.notEqual(decimal, undefined, "decimal type present");
  assert.equal(decimal!.str("family"), "decimal", "decimal family");
  assert.equal(decimal!.bool("storable"), true, "decimal storable");
  for (const name of ["decimal", "numeric", "dec"]) {
    assert.equal(scalarTypeFromName(name), "decimal", `${name} resolves to decimal`);
  }
  assert.equal(decimal!.big("max_precision"), BigInt(MAX_PRECISION), "max_precision matches module");
  assert.equal(decimal!.big("max_scale"), BigInt(MAX_SCALE), "max_scale matches module");
  assert.equal(decimal!.big("max_int_digits"), BigInt(MAX_INT_DIGITS), "max_int_digits matches module");

  // uuid: storable, the uuid family, fixed-width (the first non-integer with a width_bytes).
  // Its on-disk width (16) is a cross-core contract, so cross-check it against the spec.
  const uuid = rows.find((r) => r.str("id") === "uuid");
  assert.notEqual(uuid, undefined, "uuid type present");
  assert.equal(uuid!.str("family"), "uuid", "uuid family");
  assert.equal(uuid!.bool("storable"), true, "uuid storable");
  assert.equal(scalarTypeFromName("uuid"), "uuid", "uuid resolves");
  assert.equal(canonicalName("uuid"), "uuid", "uuid canonical name");
  assert.equal(widthBytes("uuid"), 16, "uuid is fixed 16 bytes");
});

test("error codes are registered in spec/errors/registry.toml", () => {
  const rows = readTomlTables(specPath("errors/registry.toml"), "error");
  const codes = new Map<string, string>(); // code -> name
  for (const row of rows) codes.set(row.str("code"), row.str("name"));

  const states: SqlState[] = [
    "numeric_value_out_of_range",
    "invalid_datetime_format",
    "datetime_field_overflow",
    "division_by_zero",
    "invalid_parameter_value",
    "invalid_row_count_in_limit_clause",
    "invalid_row_count_in_offset_clause",
    "not_null_violation",
    "unique_violation",
    "check_violation",
    "undefined_parameter",
    "duplicate_object",
    "active_sql_transaction",
    "read_only_sql_transaction",
    "in_failed_sql_transaction",
    "syntax_error",
    "undefined_table",
    "undefined_column",
    "undefined_object",
    "datatype_mismatch",
    "duplicate_table",
    "duplicate_column",
    "invalid_table_definition",
    "indeterminate_datatype",
    "feature_not_supported",
    "io_error",
    "undefined_file",
    "duplicate_file",
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

test("aggregates match spec/functions/catalog.toml", () => {
  // The generated aggregate descriptor table must match the canonical catalog's [[aggregate]]
  // rows field-for-field (codegen middle path, CLAUDE.md §5). Aggregates are overloaded across
  // operand families (one row per (name, arg_families)), like operators.
  const rows = readTomlTables(specPath("functions/catalog.toml"), "aggregate");
  assert.equal(rows.length, AGGREGATES.length, "aggregate count");
  for (const row of rows) {
    const name = row.str("name");
    const fams = row.strs("arg_families");
    const desc = AGGREGATES.find(
      (d) => d.name === name && [...d.argFamilies].join(",") === fams.join(","),
    );
    assert.notEqual(desc, undefined, `generated table missing aggregate ${name} ${fams.join(",")}`);
    const d = desc!;
    assert.equal(row.str("kind"), "aggregate", `${name}: kind`);
    assert.equal(d.surface, row.str("surface"), `${name}: surface`);
    assert.equal(d.arg, row.str("arg"), `${name}: arg`);
    assert.equal(d.result, row.str("result"), `${name}: result`);
    assert.equal(d.null, row.str("null"), `${name}: null`);
    assert.deepEqual([...d.errors], row.strs("errors"), `${name}: errors`);
  }
});

test("cost schedule matches spec/cost/schedule.toml", () => {
  // The generated cost schedule (codegen middle path, CLAUDE.md §5/§13) must match the
  // canonical schedule.toml weight-for-weight. Cost is a cross-core contract (§8): every
  // core reads these weights.
  const rows = readTomlTables(specPath("cost/schedule.toml"), "unit");
  // The weight() switch below forces this cross-check to be updated whenever a unit is added
  // (a new unit with no COSTS field throws), so we don't pin an exact count here.
  const weight = (id: string): bigint => {
    switch (id) {
      case "storage_row_read":
        return COSTS.storageRowRead;
      case "page_read":
        return COSTS.pageRead;
      case "value_compress":
        return COSTS.valueCompress;
      case "value_decompress":
        return COSTS.valueDecompress;
      case "decimal_work":
        return COSTS.decimalWork;
      case "row_produced":
        return COSTS.rowProduced;
      case "operator_eval":
        return COSTS.operatorEval;
      case "aggregate_accumulate":
        return COSTS.aggregateAccumulate;
      default:
        throw new Error(`cost unit ${id} has no COSTS field — update this cross-check`);
    }
  };
  for (const row of rows) {
    const id = row.str("id");
    assert.equal(weight(id), row.big("weight"), `${id}: weight`);
  }
});
