use super::*;

/// Deterministic lookup-only hash table. Buckets retain build-row indices in input order; probes
/// compare the full key bytes, so forced hash collisions cannot admit false matches. The executor
/// never iterates the map to emit rows.
pub(crate) struct HashJoinTable {
    entries: HashMap<u64, Vec<HashJoinEntry>>,
    hash: fn(&[u8]) -> u64,
    probe_offset: usize,
}

struct HashJoinEntry {
    key: Vec<u8>,
    row: usize,
}

impl HashJoinTable {
    pub(crate) fn build(
        plan: &HashJoinPlan,
        build_offset: usize,
        probe_offset: usize,
        rows: &[Row],
        meter: &mut Meter,
    ) -> Result<Self> {
        Self::build_with_hash(
            plan,
            build_offset,
            probe_offset,
            rows,
            meter,
            hash_join_fnv1a,
        )
    }

    fn build_with_hash(
        plan: &HashJoinPlan,
        build_offset: usize,
        probe_offset: usize,
        rows: &[Row],
        meter: &mut Meter,
        hash: fn(&[u8]) -> u64,
    ) -> Result<Self> {
        let mut table = Self {
            entries: HashMap::new(),
            hash,
            probe_offset,
        };
        let indices: Vec<usize> = plan
            .keys
            .iter()
            .map(|key| key.right - build_offset)
            .collect();
        let types: Vec<&Type> = plan.keys.iter().map(|key| &key.ty).collect();
        for (row_index, row) in rows.iter().enumerate() {
            let Some(key) = hash_join_row_key(row, &indices, &types, COSTS.hash_build, meter)?
            else {
                continue;
            };
            let hash = (table.hash)(&key);
            table.entries.entry(hash).or_default().push(HashJoinEntry {
                key,
                row: row_index,
            });
        }
        Ok(table)
    }

    pub(crate) fn probe(
        &self,
        plan: &HashJoinPlan,
        row: &Row,
        meter: &mut Meter,
    ) -> Result<Vec<usize>> {
        let indices: Vec<usize> = plan
            .keys
            .iter()
            .map(|key| key.left - self.probe_offset)
            .collect();
        let types: Vec<&Type> = plan.keys.iter().map(|key| &key.ty).collect();
        let Some(key) = hash_join_row_key(row, &indices, &types, COSTS.hash_probe, meter)? else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        if let Some(entries) = self.entries.get(&(self.hash)(&key)) {
            for entry in entries {
                meter.guard()?;
                let work = entry.key.len().min(key.len()).max(1);
                meter.charge(COSTS.hash_probe * i64::try_from(work).unwrap_or(i64::MAX));
                if entry.key == key {
                    rows.push(entry.row);
                }
            }
        }
        Ok(rows)
    }
}

fn hash_join_row_key(
    row: &Row,
    indices: &[usize],
    types: &[&Type],
    unit: i64,
    meter: &mut Meter,
) -> Result<Option<Vec<u8>>> {
    let mut parts = Vec::with_capacity(indices.len());
    let mut present = true;
    for (&index, ty) in indices.iter().zip(types) {
        meter.guard()?;
        if matches!(row[index], Value::Null) {
            meter.charge(unit);
            present = false;
            parts.push(Vec::new());
            continue;
        }
        let part = encode_typed_key(ty, &row[index], None)?;
        meter.charge(unit * i64::try_from(part.len().max(1)).unwrap_or(i64::MAX));
        parts.push(part);
    }
    if !present {
        return Ok(None);
    }
    let mut out = Vec::new();
    for part in parts {
        let len = u32::try_from(part.len()).expect("a row value fits the u32 key length contract");
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&part);
    }
    Ok(Some(out))
}

fn hash_join_fnv1a(key: &[u8]) -> u64 {
    let mut hash = 14_695_981_039_346_656_037_u64;
    for byte in key {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collide(_: &[u8]) -> u64 {
        0
    }

    #[test]
    fn forced_hash_collisions_recheck_full_keys() {
        let plan = HashJoinPlan {
            keys: vec![HashJoinKey {
                left: 0,
                right: 1,
                ty: Type::Scalar(ScalarType::Int32),
            }],
        };
        let rows = vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)],
            vec![Value::Int(2), Value::Int(21)],
        ];
        let mut meter = Meter::new();
        let table =
            HashJoinTable::build_with_hash(&plan, 1, 0, &rows, &mut meter, collide).unwrap();
        let got = table
            .probe(&plan, &vec![Value::Int(2)], &mut Meter::new())
            .unwrap();
        assert_eq!(got, vec![1, 2]);
    }
}
