//! `row_to_json(record [, pretty])` (mirrors impl/go row_json.go): a port of PostgreSQL's
//! `composite_to_json` / `array_to_json_internal` / `datum_to_json_internal` (json.c) — the
//! composite → `json` object render (spec/design/json-sql-functions.md §2.2).

use super::*;

/// Render the composite value `fields`, of the composite type `ty`, as a `json` object onto `out`:
/// `{"name":value,…}` in declaration order. `pretty` separates the members with `",\n "` instead of
/// `","` (PostgreSQL's `use_line_feeds`, honored at the top level only). A `timestamptz` at any
/// depth renders in `zone`.
pub(crate) fn composite_to_json(
    fields: &[Value],
    ty: &CompositeRType,
    pretty: bool,
    zone: &crate::timezone::ZoneRef,
    out: &mut String,
) -> Result<()> {
    let sep = if pretty { ",\n " } else { "," };
    out.push('{');
    for (i, ((name, field_ty), v)) in ty.fields.iter().zip(fields).enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        json::write_json_string(name, out);
        out.push(':');
        datum_to_json(v, field_ty, zone, out)?;
    }
    out.push('}');
    Ok(())
}

/// One value's JSON image (PostgreSQL's `datum_to_json_internal`, never a key). `ty` is the value's
/// static type, read only for the field names of a composite (directly or as an array element).
fn datum_to_json(
    v: &Value,
    ty: &ResolvedType,
    zone: &crate::timezone::ZoneRef,
    out: &mut String,
) -> Result<()> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        // A number is its exact text (jed's decimal is always finite, so never a quoted NaN).
        Value::Int(n) => out.push_str(&n.to_string()),
        Value::Decimal(d) => out.push_str(&d.render()),
        Value::Text(s) => json::write_json_string(s, out),
        // json embeds its text verbatim; jsonb its canonical (spaced) output.
        Value::Json(s) => out.push_str(s),
        Value::Jsonb(n) => out.push_str(&json::jsonb_out(n)),
        Value::Composite(fields) => {
            let ResolvedType::Composite(c) = ty else {
                unreachable!("a composite value has a composite static type")
            };
            composite_to_json(fields, c, false, zone, out)?;
        }
        Value::Array(a) => {
            let ResolvedType::Array(elem) = ty else {
                unreachable!("an array value has an array static type")
            };
            array_to_json(a, elem, zone, out)?;
        }
        // The datetimes take PostgreSQL's JsonEncodeDateTime (XSD) form.
        Value::Date(d) => json::write_json_string(&crate::date::render_date(*d), out),
        Value::Timestamp(m) => {
            json::write_json_string(&crate::timestamp::render_timestamp_xsd(*m), out)
        }
        Value::Timestamptz(m) => {
            json::write_json_string(&crate::timestamp::render_timestamptz_xsd_in(*m, zone)?, out)
        }
        Value::Float32(_) | Value::Float64(_) => {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "row_to_json of a float value is not supported yet",
            ));
        }
        // Every other type is the JSON string of its text output (a range's timestamptz bounds in
        // the session zone).
        Value::Interval(_)
        | Value::Uuid(_)
        | Value::Bytea(_)
        | Value::Range(_)
        | Value::JsonPath(_) => json::write_json_string(&v.render_in(zone)?, out),
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    }
    Ok(())
}

/// An array's JSON image (PostgreSQL's `array_to_json_internal` without line feeds): one bracket
/// level per dimension, the lower bounds dropped; an empty array is `[]`.
fn array_to_json(
    a: &ArrayVal,
    elem: &ResolvedType,
    zone: &crate::timezone::ZoneRef,
    out: &mut String,
) -> Result<()> {
    if a.elements.is_empty() {
        out.push_str("[]");
        return Ok(());
    }
    let mut next = 0;
    array_dim_to_json(a, 0, elem, zone, &mut next, out)
}

/// One dimension of [`array_to_json`]: the innermost dimension renders `dims[dim]` elements from
/// `next`, an outer one recurses once per sub-array.
fn array_dim_to_json(
    a: &ArrayVal,
    dim: usize,
    elem: &ResolvedType,
    zone: &crate::timezone::ZoneRef,
    next: &mut usize,
    out: &mut String,
) -> Result<()> {
    out.push('[');
    for i in 0..a.dims[dim] {
        if i > 0 {
            out.push(',');
        }
        if dim + 1 == a.ndim() {
            datum_to_json(&a.elements[*next], elem, zone, out)?;
            *next += 1;
        } else {
            array_dim_to_json(a, dim + 1, elem, zone, next, out)?;
        }
    }
    out.push(']');
    Ok(())
}
