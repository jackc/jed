// row_to_json(record [, pretty]) (mirrors impl/rust executor/row_json.rs): a port of PostgreSQL's
// composite_to_json / array_to_json_internal / datum_to_json_internal (json.c) — the composite →
// json object render (spec/design/json-sql-functions.md §2.2).

import type { ResolvedType } from "./executor.ts";
import { engineError } from "./errors.ts";
import { jsonbOut, writeJsonString } from "./json.ts";
import { renderDate } from "./date.ts";
import { renderTimestampXsd } from "./timestamp.ts";
import { type ZoneRef, renderTimestamptzXsdIn } from "./timezone.ts";
import { type Value, renderIn } from "./value.ts";

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

// datumToJson writes one value's JSON image (PostgreSQL's datum_to_json_internal, never a key). ty is
// the value's static type, read only for the field names of a composite (directly or as an array
// element).
function datumToJson(v: Value, ty: ResolvedType, zone: ZoneRef, out: string[]): void {
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
      arrayToJson(v, ty.elem, zone, out);
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
    case "f32":
    case "f64":
      throw engineError(
        "feature_not_supported",
        "row_to_json of a float value is not supported yet",
      );
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

// arrayToJson writes an array's JSON image (PostgreSQL's array_to_json_internal without line feeds):
// one bracket level per dimension, the lower bounds dropped; an empty array is [].
function arrayToJson(a: ArrayValue, elem: ResolvedType, zone: ZoneRef, out: string[]): void {
  if (a.elements.length === 0) {
    out.push("[]");
    return;
  }
  const next = { i: 0 };
  arrayDimToJson(a, 0, elem, zone, next, out);
}

// arrayDimToJson writes one dimension of arrayToJson: the innermost dimension renders dims[dim]
// elements from next.i, an outer one recurses once per sub-array.
function arrayDimToJson(
  a: ArrayValue,
  dim: number,
  elem: ResolvedType,
  zone: ZoneRef,
  next: { i: number },
  out: string[],
): void {
  out.push("[");
  for (let i = 0; i < a.dims[dim]; i++) {
    if (i > 0) out.push(",");
    if (dim + 1 === a.dims.length) {
      datumToJson(a.elements[next.i], elem, zone, out);
      next.i++;
    } else {
      arrayDimToJson(a, dim + 1, elem, zone, next, out);
    }
  }
  out.push("]");
}
