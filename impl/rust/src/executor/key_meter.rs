//! Per-row key metering (spec/design/cost.md §3 "sort_key / hash_key"; mirrors impl/go
//! key_meter.go). A sort, a grouping or dedup table, and a window's partitioning charge each row's
//! key values once, by size, where the row enters the operator. The comparisons and probes inside
//! them stay unmetered. Each charge is guarded only when it is nonzero, so a fixed-width or NULL key
//! moves no abort point.

use super::*;

/// Charge one sort-key value of a row the sort receives: `collate` per code point for a collated
/// text or text[] key, else `sort_key` per payload byte.
pub(crate) fn charge_sort_key(m: &mut Meter, v: &Value, coll: Option<&Collation>) -> Result<()> {
    let units = match (coll, v) {
        (Some(_), Value::Text(_) | Value::Array(_)) => COSTS.collate * collated_code_points(v),
        _ => COSTS.sort_key * crate::memsize::payload_bytes(v),
    };
    if units == 0 {
        return Ok(());
    }
    m.charge(units);
    m.guard()
}

/// Charge every sort key of one row a sort receives, in key order.
pub(crate) fn charge_sort_keys(
    m: &mut Meter,
    row: &[Value],
    order: &[crate::spill::SortKey],
) -> Result<()> {
    for (idx, _, _, coll) in order {
        charge_sort_key(m, &row[*idx], coll.as_deref())?;
    }
    Ok(())
}

/// The code points a collated key's UCA sort key is built from: a text's, or a text[]'s non-NULL
/// elements'.
fn collated_code_points(v: &Value) -> i64 {
    match v {
        Value::Text(s) => s.chars().count() as i64,
        Value::Array(a) => a
            .elements
            .iter()
            .map(|e| match e {
                Value::Text(s) => s.chars().count() as i64,
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// Charge the key values of a row a grouping, dedup, or partition table receives: `hash_key` per
/// payload byte of each, in key order.
pub(crate) fn charge_hash_keys<'a>(
    m: &mut Meter,
    keys: impl IntoIterator<Item = &'a Value>,
) -> Result<()> {
    for v in keys {
        charge_hash_key(m, v)?;
    }
    Ok(())
}

pub(crate) fn charge_hash_key(m: &mut Meter, v: &Value) -> Result<()> {
    let units = COSTS.hash_key * crate::memsize::payload_bytes(v);
    if units == 0 {
        return Ok(());
    }
    m.charge(units);
    m.guard()
}
