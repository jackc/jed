//! The JSON image of a value (mirrors impl/go row_json.go): a port of PostgreSQL's json.c
//! `datum_to_json_internal` / `array_to_json_internal` / `composite_to_json` and jsonb.c
//! `datum_to_jsonb_internal` / `array_to_jsonb_internal` / `composite_to_jsonb`. `to_json`,
//! `to_jsonb`, `array_to_json`, `row_to_json`, the json[b] builders, and the json[b] aggregates all
//! convert their arguments here (spec/design/json-sql-functions.md §2.2).

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

/// One value's `json` image onto `out` (PostgreSQL's `datum_to_json_internal`, never a key). `ty` is
/// the value's static type, read only for the field names of a composite (directly or as an array
/// element).
pub(crate) fn datum_to_json(
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
            array_to_json(a, elem, false, zone, out)?;
        }
        // The datetimes take PostgreSQL's JsonEncodeDateTime (XSD) form.
        Value::Date(d) => json::write_json_string(&crate::date::render_date(*d), out),
        Value::Timestamp(m) => {
            json::write_json_string(&crate::timestamp::render_timestamp_xsd(*m), out)
        }
        Value::Timestamptz(m) => {
            json::write_json_string(&crate::timestamp::render_timestamptz_xsd_in(*m, zone)?, out)
        }
        // PostgreSQL's float8out/float4out text: a number, or the JSON string of NaN/±Infinity.
        Value::Float32(_) | Value::Float64(_) => {
            let (f, is32) = crate::value::float_of(v).expect("a float value");
            let text = crate::value::render_float_pg(f, is32);
            if f.is_finite() {
                out.push_str(&text);
            } else {
                json::write_json_string(&text, out);
            }
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

/// An array's `json` image (PostgreSQL's `array_to_json_internal`): one bracket level per dimension,
/// the lower bounds dropped; an empty array is `[]`. `pretty` separates the OUTERMOST dimension's
/// items with `",\n "` (inner dimensions and the elements never break lines).
pub(crate) fn array_to_json(
    a: &ArrayVal,
    elem: &ResolvedType,
    pretty: bool,
    zone: &crate::timezone::ZoneRef,
    out: &mut String,
) -> Result<()> {
    if a.elements.is_empty() {
        out.push_str("[]");
        return Ok(());
    }
    let mut next = 0;
    array_dim_to_json(a, 0, elem, pretty, zone, &mut next, out)
}

/// One dimension of [`array_to_json`]: the innermost dimension renders `dims[dim]` elements from
/// `next`, an outer one recurses once per sub-array (never with line feeds).
fn array_dim_to_json(
    a: &ArrayVal,
    dim: usize,
    elem: &ResolvedType,
    pretty: bool,
    zone: &crate::timezone::ZoneRef,
    next: &mut usize,
    out: &mut String,
) -> Result<()> {
    let sep = if pretty { ",\n " } else { "," };
    out.push('[');
    for i in 0..a.dims[dim] {
        if i > 0 {
            out.push_str(sep);
        }
        if dim + 1 == a.ndim() {
            datum_to_json(&a.elements[*next], elem, zone, out)?;
            *next += 1;
        } else {
            array_dim_to_json(a, dim + 1, elem, false, zone, next, out)?;
        }
    }
    out.push(']');
    Ok(())
}

/// One value's `jsonb` image (PostgreSQL's `datum_to_jsonb_internal`, never a key): as
/// [`datum_to_json`], except that a `json` value is parsed (duplicate keys collapse, last wins), a
/// composite is an object in canonical key order, and the result is a node, not text.
pub(crate) fn datum_to_jsonb(
    v: &Value,
    ty: &ResolvedType,
    zone: &crate::timezone::ZoneRef,
) -> Result<JsonNode> {
    Ok(match v {
        Value::Null => JsonNode::Null,
        Value::Bool(b) => JsonNode::Bool(*b),
        Value::Int(n) => JsonNode::Number(Decimal::from_i64(*n)),
        Value::Decimal(d) => JsonNode::Number(d.clone()),
        Value::Text(s) => JsonNode::String(s.clone()),
        Value::Json(s) => json::jsonb_in(s)?,
        Value::Jsonb(n) => n.clone(),
        Value::Composite(fields) => {
            let ResolvedType::Composite(c) = ty else {
                unreachable!("a composite value has a composite static type")
            };
            let mut members = Vec::with_capacity(fields.len());
            for ((name, field_ty), f) in c.fields.iter().zip(fields) {
                members.push((name.clone(), datum_to_jsonb(f, field_ty, zone)?));
            }
            json::make_object(members)
        }
        Value::Array(a) => {
            let ResolvedType::Array(elem) = ty else {
                unreachable!("an array value has an array static type")
            };
            if a.elements.is_empty() {
                JsonNode::Array(Vec::new())
            } else {
                let mut next = 0;
                array_dim_to_jsonb(a, 0, elem, zone, &mut next)?
            }
        }
        Value::Date(d) => JsonNode::String(crate::date::render_date(*d)),
        Value::Timestamp(m) => JsonNode::String(crate::timestamp::render_timestamp_xsd(*m)),
        Value::Timestamptz(m) => {
            JsonNode::String(crate::timestamp::render_timestamptz_xsd_in(*m, zone)?)
        }
        // The number numeric_in reads from the float8out/float4out text; NaN/±Infinity are strings.
        Value::Float32(_) | Value::Float64(_) => {
            let (f, is32) = crate::value::float_of(v).expect("a float value");
            if f.is_finite() {
                JsonNode::Number(crate::value::float_json_number(f, is32))
            } else {
                JsonNode::String(crate::value::render_float_pg(f, is32))
            }
        }
        Value::Interval(_)
        | Value::Uuid(_)
        | Value::Bytea(_)
        | Value::Range(_)
        | Value::JsonPath(_) => JsonNode::String(v.render_in(zone)?),
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    })
}

/// One dimension of an array's `jsonb` image (PostgreSQL's `array_dim_to_jsonb`).
fn array_dim_to_jsonb(
    a: &ArrayVal,
    dim: usize,
    elem: &ResolvedType,
    zone: &crate::timezone::ZoneRef,
    next: &mut usize,
) -> Result<JsonNode> {
    let mut items = Vec::with_capacity(a.dims[dim]);
    for _ in 0..a.dims[dim] {
        if dim + 1 == a.ndim() {
            items.push(datum_to_jsonb(&a.elements[*next], elem, zone)?);
            *next += 1;
        } else {
            items.push(array_dim_to_jsonb(a, dim + 1, elem, zone, next)?);
        }
    }
    Ok(JsonNode::Array(items))
}

/// The text of an object KEY (PostgreSQL's `datum_to_json[b]_internal` with `key_scalar`): a key is
/// always a JSON string. A number or boolean is its text, a datetime its ISO 8601 form, any other
/// scalar its text output. An array, composite, `json`, or `jsonb` key is `22023`. The caller rejects a
/// NULL key first.
pub(crate) fn json_key_text(v: &Value, zone: &crate::timezone::ZoneRef) -> Result<String> {
    Ok(match v {
        Value::Array(_) | Value::Composite(_) | Value::Json(_) | Value::Jsonb(_) => {
            return Err(EngineError::new(
                SqlState::InvalidParameterValue,
                "key value must be scalar, not array, composite, or json",
            ));
        }
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Int(n) => n.to_string(),
        Value::Decimal(d) => d.render(),
        Value::Text(s) => s.clone(),
        Value::Date(d) => crate::date::render_date(*d),
        Value::Timestamp(m) => crate::timestamp::render_timestamp_xsd(*m),
        Value::Timestamptz(m) => crate::timestamp::render_timestamptz_xsd_in(*m, zone)?,
        Value::Float32(_) | Value::Float64(_) => {
            let (f, is32) = crate::value::float_of(v).expect("a float value");
            crate::value::render_float_pg(f, is32)
        }
        Value::Interval(_)
        | Value::Uuid(_)
        | Value::Bytea(_)
        | Value::Range(_)
        | Value::JsonPath(_) => v.render_in(zone)?,
        Value::Null => unreachable!("the caller rejects a NULL key"),
        Value::Unfetched(_) => panic!("BUG: unfetched large value escaped the storage layer"),
    })
}

/// Whether `json_agg` separates a value of type `ty` from the previous element with an extra
/// `"\n "` — PostgreSQL does so for the array and composite categories.
pub(crate) fn json_agg_breaks_line(ty: &ResolvedType) -> bool {
    matches!(ty, ResolvedType::Array(_) | ResolvedType::Composite(_))
}
