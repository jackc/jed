//! The logical query-memory size schedule (spec/design/memory.md §3).
//!
//! Representation-independent byte counts the query-memory account reserves and releases. They are
//! a pure function of a value's logical content — never of Rust's object layout, a leaf's physical
//! state, or whether a column was decoded — so a `54P05` abort is cross-core identical. The
//! constants are codegen'd from spec/cost/schedule.toml `[memory]`; the shared vectors in
//! spec/cost/memory_sizes.toml pin the measurement (tests/memory_sizes.rs).

use crate::costs::{
    MEMORY_ARRAY_DIM, MEMORY_DECIMAL_GROUP, MEMORY_ENTRY, MEMORY_ROW, MEMORY_VALUE,
};
use crate::json::JsonNode;
use crate::value::Value;

/// `value_bytes(v) = VALUE + payload(v)`.
pub(crate) fn value_bytes(v: &Value) -> i64 {
    match v {
        Value::Text(s) | Value::Json(s) | Value::JsonPath(s) => MEMORY_VALUE + s.len() as i64,
        Value::Bytea(b) => MEMORY_VALUE + b.len() as i64,
        Value::Decimal(d) => MEMORY_VALUE + MEMORY_DECIMAL_GROUP * d.codec_group_count() as i64,
        Value::Array(a) => {
            MEMORY_VALUE
                + MEMORY_ARRAY_DIM * a.dims.len() as i64
                + a.elements.iter().map(value_bytes).sum::<i64>()
        }
        Value::Composite(fields) => MEMORY_VALUE + fields.iter().map(value_bytes).sum::<i64>(),
        Value::Range(r) => {
            MEMORY_VALUE
                + r.lower.as_deref().map_or(0, value_bytes)
                + r.upper.as_deref().map_or(0, value_bytes)
        }
        Value::Jsonb(node) => jsonb_bytes(node),
        // NULL, fixed-width scalars, and an unfetched reference (only ever an untouched slot) are a
        // bare node.
        _ => MEMORY_VALUE,
    }
}

/// One `VALUE` per jsonb node plus its string/key bytes and number groups.
fn jsonb_bytes(node: &JsonNode) -> i64 {
    MEMORY_VALUE
        + match node {
            JsonNode::String(s) => s.len() as i64,
            JsonNode::Number(d) => MEMORY_DECIMAL_GROUP * d.codec_group_count() as i64,
            JsonNode::Array(items) => items.iter().map(jsonb_bytes).sum(),
            JsonNode::Object(members) => members
                .iter()
                .map(|(k, v)| k.len() as i64 + jsonb_bytes(v))
                .sum(),
            JsonNode::Null | JsonNode::Bool(_) => 0,
        }
}

/// `row_bytes(row) = ROW + Σ value_bytes` — a projected row, every slot a computed value.
pub(crate) fn row_bytes(row: &[Value]) -> i64 {
    MEMORY_ROW + row.iter().map(value_bytes).sum::<i64>()
}

/// A pre-projection row measured under the plan's touched mask (memory.md §3): an untouched slot
/// (`i < mask.len() && !mask[i]`) is a bare `VALUE` whatever its physical form; touched and appended
/// slots count in full.
pub(crate) fn row_bytes_masked(row: &[Value], mask: &[bool]) -> i64 {
    MEMORY_ROW
        + row
            .iter()
            .enumerate()
            .map(|(i, v)| {
                if mask.get(i) == Some(&false) {
                    MEMORY_VALUE
                } else {
                    value_bytes(v)
                }
            })
            .sum::<i64>()
}

/// `key_bytes(values) = Σ value_bytes` — a key or state tuple with no ROW header (memory.md §6.1).
pub(crate) fn key_bytes(values: &[Value]) -> i64 {
    values.iter().map(value_bytes).sum()
}

/// `entry_bytes(values) = ENTRY + key_bytes` — one hash/dedup/group entry (memory.md §6.1).
pub(crate) fn entry_bytes(values: &[Value]) -> i64 {
    MEMORY_ENTRY + key_bytes(values)
}

/// `entry_bytes` of the key at `indices` of `row`, without copying the key out.
pub(crate) fn entry_bytes_at(row: &[Value], indices: &[usize]) -> i64 {
    MEMORY_ENTRY + indices.iter().map(|&i| value_bytes(&row[i])).sum::<i64>()
}

/// A spill-capable structure's resident measure of one keyed state-map element (memory.md §6.6).
pub(crate) fn map_entry_bytes(key: &[Value], value: &[Value]) -> i64 {
    MEMORY_ENTRY + key_bytes(key) + key_bytes(value)
}

/// A spill-capable hash row table's resident measure of one row (memory.md §6.6).
pub(crate) fn hash_row_bytes(row: &[Value]) -> i64 {
    MEMORY_ENTRY + row_bytes(row)
}
