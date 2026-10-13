// The JSON image of a value (mirrors impl/rust executor/row_json.rs): a port of PostgreSQL's json.c
// datum_to_json_internal / array_to_json_internal / composite_to_json and jsonb.c
// datum_to_jsonb_internal / array_to_jsonb_internal / composite_to_jsonb. to_json, to_jsonb,
// array_to_json, row_to_json, the json[b] builders, and the json[b] aggregates all convert their
// arguments here (spec/design/json-sql-functions.md §2.2).

import type { ResolvedType } from "./executor.ts";
import { Decimal } from "./decimal.ts";
import { engineError } from "./errors.ts";
import {
  type JsonMember,
  type JsonNode,
  jsonbIn,
  jsonbOut,
  makeObject,
  writeJsonString,
} from "./json.ts";
import { renderDate } from "./date.ts";
import { renderTimestampXsd } from "./timestamp.ts";
import { type ZoneRef, renderTimestamptzXsdIn } from "./timezone.ts";
import { type Value, floatShortest, renderFloatPg, renderIn } from "./value.ts";

type CompositeType = Extract<ResolvedType, { kind: "composite" }>;
type ArrayValue = Extract<Value, { kind: "array" }>;

// compositeToJson renders the composite value fields, of the composite type ty, as a json object
// onto out: {"name":value,…} in declaration order. pretty separates the members with ",\n " instead
// of "," (PostgreSQL's use_line_feeds, honored at the top level only). A timestamptz at any depth
// renders in zone.
export function compositeToJson(
  fields: Value[],
  ty: CompositeType,
  pretty: boolean,
  zone: ZoneRef,
  out: string[],
): void {
  const sep = pretty ? ",\n " : ",";
  out.push("{");
  ty.fields.forEach((f, i) => {
    if (i > 0) out.push(sep);
    writeJsonString(f.name, out);
    out.push(":");
    datumToJson(fields[i], f.type, zone, out);
  });
  out.push("}");
}

// datumToJson writes one value's json image (PostgreSQL's datum_to_json_internal, never a key). ty is
// the value's static type, read only for the field names of a composite (directly or as an array
// element).
export function datumToJson(v: Value, ty: ResolvedType, zone: ZoneRef, out: string[]): void {
  switch (v.kind) {
    case "null":
      out.push("null");
      return;
    case "bool":
      out.push(v.value ? "true" : "false");
      return;
    // A number is its exact text (jed's decimal is always finite, so never a quoted NaN).
    case "int":
      out.push(v.int.toString());
      return;
    case "decimal":
      out.push(v.dec.render());
      return;
    case "text":
      writeJsonString(v.text, out);
      return;
    // json embeds its text verbatim; jsonb its canonical (spaced) output.
    case "json":
      out.push(v.text);
      return;
    case "jsonb":
      out.push(jsonbOut(v.node));
      return;
    case "composite":
      if (ty.kind !== "composite") throw new Error("a composite value has a composite static type");
      compositeToJson(v.fields, ty, false, zone, out);
      return;
    case "array":
      if (ty.kind !== "array") throw new Error("an array value has an array static type");
      arrayToJson(v, ty.elem, false, zone, out);
      return;
    // The datetimes take PostgreSQL's JsonEncodeDateTime (XSD) form.
    case "date":
      writeJsonString(renderDate(v.days), out);
      return;
    case "timestamp":
      writeJsonString(renderTimestampXsd(v.micros), out);
      return;
    case "timestamptz":
      writeJsonString(renderTimestamptzXsdIn(zone, v.micros), out);
      return;
    // PostgreSQL's float8out/float4out text: a number, or the JSON string of NaN/±Infinity.
    case "f32":
    case "f64": {
      const text = renderFloatPg(v.value, v.kind === "f32");
      if (Number.isFinite(v.value)) out.push(text);
      else writeJsonString(text, out);
      return;
    }
    // Every other type is the JSON string of its text output (a range's timestamptz bounds in the
    // session zone).
    case "interval":
    case "uuid":
    case "bytea":
    case "range":
    case "jsonpath":
      writeJsonString(renderIn(v, zone), out);
      return;
    case "unfetched":
      throw new Error("BUG: unfetched large value escaped the storage layer");
  }
}

// arrayToJson writes an array's json image (PostgreSQL's array_to_json_internal): one bracket level
// per dimension, the lower bounds dropped; an empty array is []. pretty separates the OUTERMOST
// dimension's items with ",\n " (inner dimensions and the elements never break lines).
export function arrayToJson(
  a: ArrayValue,
  elem: ResolvedType,
  pretty: boolean,
  zone: ZoneRef,
  out: string[],
): void {
  if (a.elements.length === 0) {
    out.push("[]");
    return;
  }
  const next = { i: 0 };
  arrayDimToJson(a, 0, elem, pretty, zone, next, out);
}

// arrayDimToJson writes one dimension of arrayToJson: the innermost dimension renders dims[dim]
// elements from next.i, an outer one recurses once per sub-array (never with line feeds).
function arrayDimToJson(
  a: ArrayValue,
  dim: number,
  elem: ResolvedType,
  pretty: boolean,
  zone: ZoneRef,
  next: { i: number },
  out: string[],
): void {
  const sep = pretty ? ",\n " : ",";
  out.push("[");
  for (let i = 0; i < a.dims[dim]; i++) {
    if (i > 0) out.push(sep);
    if (dim + 1 === a.dims.length) {
      datumToJson(a.elements[next.i], elem, zone, out);
      next.i++;
    } else {
      arrayDimToJson(a, dim + 1, elem, false, zone, next, out);
    }
  }
  out.push("]");
}

// datumToJsonb is one value's jsonb image (PostgreSQL's datum_to_jsonb_internal, never a key): as
// datumToJson, except that a json value is parsed (duplicate keys collapse, last wins), a composite is
// an object in canonical key order, and the result is a node, not text.
export function datumToJsonb(v: Value, ty: ResolvedType, zone: ZoneRef): JsonNode {
  switch (v.kind) {
    case "null":
      return { kind: "null" };
    case "bool":
      return { kind: "bool", value: v.value };
    case "int":
      return { kind: "number", dec: Decimal.fromBigInt(v.int) };
    case "decimal":
      return { kind: "number", dec: v.dec };
    case "text":
      return { kind: "string", value: v.text };
    case "json":
      return jsonbIn(v.text);
    case "jsonb":
      return v.node;
    case "composite": {
      if (ty.kind !== "composite") throw new Error("a composite value has a composite static type");
      const members: JsonMember[] = ty.fields.map((f, i) => ({
        key: f.name,
        value: datumToJsonb(v.fields[i], f.type, zone),
      }));
      return makeObject(members);
    }
    case "array": {
      if (ty.kind !== "array") throw new Error("an array value has an array static type");
      if (v.elements.length === 0) return { kind: "array", elements: [] };
      const next = { i: 0 };
      return arrayDimToJsonb(v, 0, ty.elem, zone, next);
    }
    case "date":
      return { kind: "string", value: renderDate(v.days) };
    case "timestamp":
      return { kind: "string", value: renderTimestampXsd(v.micros) };
    case "timestamptz":
      return { kind: "string", value: renderTimestamptzXsdIn(zone, v.micros) };
    // The number numeric_in reads from the float8out/float4out text; NaN/±Infinity are strings.
    case "f32":
    case "f64":
      if (Number.isFinite(v.value)) {
        return { kind: "number", dec: floatJsonNumber(v.value, v.kind === "f32") };
      }
      return { kind: "string", value: renderFloatPg(v.value, v.kind === "f32") };
    case "interval":
    case "uuid":
    case "bytea":
    case "range":
    case "jsonpath":
      return { kind: "string", value: renderIn(v, zone) };
    case "unfetched":
      throw new Error("BUG: unfetched large value escaped the storage layer");
  }
}

// arrayDimToJsonb is one dimension of an array's jsonb image (PostgreSQL's array_dim_to_jsonb).
function arrayDimToJsonb(
  a: ArrayValue,
  dim: number,
  elem: ResolvedType,
  zone: ZoneRef,
  next: { i: number },
): JsonNode {
  const elements: JsonNode[] = [];
  for (let i = 0; i < a.dims[dim]; i++) {
    if (dim + 1 === a.dims.length) {
      elements.push(datumToJsonb(a.elements[next.i], elem, zone));
      next.i++;
    } else {
      elements.push(arrayDimToJsonb(a, dim + 1, elem, zone, next));
    }
  }
  return { kind: "array", elements };
}

// jsonKeyText is the text of an object KEY (PostgreSQL's datum_to_json[b]_internal with key_scalar):
// a key is always a JSON string. A number or boolean is its text, a datetime its ISO 8601 form, any
// other scalar its text output. An array, composite, json, or jsonb key is 22023. The caller rejects a
// NULL key first. jed integers are bigint, rendered via toString (never a float path).
export function jsonKeyText(v: Value, zone: ZoneRef): string {
  switch (v.kind) {
    case "array":
    case "composite":
    case "json":
    case "jsonb":
      throw engineError(
        "invalid_parameter_value",
        "key value must be scalar, not array, composite, or json",
      );
    case "bool":
      return v.value ? "true" : "false";
    case "int":
      return v.int.toString();
    case "decimal":
      return v.dec.render();
    case "text":
      return v.text;
    case "date":
      return renderDate(v.days);
    case "timestamp":
      return renderTimestampXsd(v.micros);
    case "timestamptz":
      return renderTimestamptzXsdIn(zone, v.micros);
    case "f32":
    case "f64":
      return renderFloatPg(v.value, v.kind === "f32");
    case "interval":
    case "uuid":
    case "bytea":
    case "range":
    case "jsonpath":
      return renderIn(v, zone);
    case "null":
      throw new Error("jsonKeyText: the caller rejects a NULL key");
    case "unfetched":
      throw new Error("BUG: unfetched large value escaped the storage layer");
  }
}

// jsonAggBreaksLine reports whether json_agg separates a value of type ty from the previous element
// with an extra "\n " — PostgreSQL does so for the array and composite categories.
export function jsonAggBreaksLine(ty: ResolvedType): boolean {
  return ty.kind === "array" || ty.kind === "composite";
}

// floatJsonNumber is the jsonb number of a finite float: PostgreSQL's numeric_in over the
// renderFloatPg text, whose display scale is the count of digits after the point in fixed notation.
// -0 is 0 (a numeric has no negative zero).
function floatJsonNumber(n: number, is32: boolean): Decimal {
  if (n === 0) return Decimal.fromDigitsScale(false, "0", 0);
  const { neg, digits, exp } = floatShortest(n, is32);
  const shift = exp - digits.length + 1;
  return shift >= 0
    ? Decimal.fromDigitsScale(neg, digits + "0".repeat(shift), 0)
    : Decimal.fromDigitsScale(neg, digits, -shift);
}
