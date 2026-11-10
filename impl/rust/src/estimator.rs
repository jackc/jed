//! Deterministic Path-B estimator arithmetic shared by planner candidate estimates and fixtures.

use crate::estimator_constants::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Selectivity {
    All,
    Zero,
    Unique,
    Fraction(EstimatorFraction),
    Not(Box<Selectivity>),
    And(Box<Selectivity>, Box<Selectivity>),
    Or(Box<Selectivity>, Box<Selectivity>),
}

impl Selectivity {
    pub(crate) fn fraction(fraction: EstimatorFraction) -> Self {
        Self::Fraction(fraction)
    }

    pub(crate) fn and(self, rhs: Self) -> Self {
        Self::And(Box::new(self), Box::new(rhs))
    }

    pub(crate) fn or(self, rhs: Self) -> Self {
        Self::Or(Box::new(self), Box::new(rhs))
    }

    pub(crate) fn not(self) -> Self {
        Self::Not(Box::new(self))
    }
}

/// Fold disjunct estimates with the OR rule as a balanced tree (the halves split at `len / 2`), so
/// an array of many elements cannot nest the program deeply. The OR rule's capped sum is
/// associative, so the tree shape never changes an estimate. No parts is `Zero`.
pub(crate) fn or_selectivities(mut parts: Vec<Selectivity>) -> Selectivity {
    match parts.len() {
        0 => return Selectivity::Zero,
        1 => return parts.pop().expect("one part"),
        _ => {}
    }
    let right = parts.split_off(parts.len() / 2);
    or_selectivities(parts).or(or_selectivities(right))
}

pub(crate) fn sat_add(a: i64, b: i64) -> i64 {
    a.saturating_add(b)
}

pub(crate) fn sat_mul(a: i64, b: i64) -> i64 {
    a.saturating_mul(b)
}

/// `ceil(n * numerator / denominator)` without a wider intermediate.
pub(crate) fn scale_ceil(n: i64, fraction: EstimatorFraction) -> i64 {
    debug_assert!(n >= 0 && fraction.numerator >= 0 && fraction.denominator > 0);
    if n == 0 || fraction.numerator == 0 {
        return 0;
    }
    let quotient = n / fraction.denominator;
    let remainder = n % fraction.denominator;
    let whole = sat_mul(quotient, fraction.numerator);
    let tail_product = sat_mul(remainder, fraction.numerator);
    let tail =
        tail_product / fraction.denominator + i64::from(tail_product % fraction.denominator != 0);
    sat_add(whole, tail)
}

pub(crate) fn estimate_rows(selectivity: &Selectivity, input_rows: i64) -> i64 {
    let n = input_rows.clamp(0, MAX_ESTIMATE);
    match selectivity {
        Selectivity::All => n,
        Selectivity::Zero => 0,
        Selectivity::Unique => n.min(1),
        Selectivity::Fraction(fraction) => scale_ceil(n, *fraction),
        Selectivity::Not(child) => n - estimate_rows(child, n),
        Selectivity::And(lhs, rhs) => {
            let left_rows = estimate_rows(lhs, n);
            estimate_rows(rhs, left_rows)
        }
        Selectivity::Or(lhs, rhs) => sat_add(estimate_rows(lhs, n), estimate_rows(rhs, n)).min(n),
    }
}

pub(crate) fn selectivity_class(class: &str) -> Selectivity {
    match class {
        "equality" => Selectivity::fraction(SELECTIVITY_EQUALITY),
        "inequality" => Selectivity::fraction(SELECTIVITY_INEQUALITY),
        "paired_range" => Selectivity::fraction(SELECTIVITY_PAIRED_RANGE),
        "null_test" => Selectivity::fraction(SELECTIVITY_NULL_TEST),
        "match" => Selectivity::fraction(SELECTIVITY_MATCH),
        "matching" => Selectivity::fraction(SELECTIVITY_MATCHING),
        "boolean" => Selectivity::fraction(SELECTIVITY_BOOLEAN),
        _ => Selectivity::fraction(SELECTIVITY_OPAQUE),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CandidateEstimate {
    pub(crate) rows: i64,
    pub(crate) units: [i64; ESTIMATOR_UNIT_COUNT],
    pub(crate) cost: i64,
}

/// P5's cumulative estimate for one rendered plan node. `logical_rows` carries the unbounded
/// logical population alongside the rows delivered by the selected access path, preventing a
/// residual predicate used as a bound from being selectivity-folded twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlanEstimate {
    pub(crate) rows: i64,
    pub(crate) logical_rows: i64,
    pub(crate) units: [i64; ESTIMATOR_UNIT_COUNT],
}

impl PlanEstimate {
    pub(crate) fn empty(rows: i64) -> Self {
        let rows = rows.clamp(0, MAX_ESTIMATE);
        Self {
            rows,
            logical_rows: rows,
            units: [0; ESTIMATOR_UNIT_COUNT],
        }
    }

    pub(crate) fn cost(&self) -> i64 {
        self.units
            .iter()
            .zip(ESTIMATOR_UNIT_WEIGHTS)
            .fold(0, |total, (count, weight)| {
                sat_add(total, sat_mul(*count, weight))
            })
    }

    pub(crate) fn add(&self, rhs: &Self) -> Self {
        let mut out = self.clone();
        for (value, addend) in out.units.iter_mut().zip(rhs.units) {
            *value = sat_add(*value, addend);
        }
        out
    }

    pub(crate) fn repeated(&self, count: i64) -> Self {
        let count = count.clamp(0, MAX_ESTIMATE);
        Self {
            rows: sat_mul(self.rows, count),
            logical_rows: sat_mul(self.logical_rows, count),
            units: self.units.map(|value| sat_mul(value, count)),
        }
    }

    pub(crate) fn add_unit(&mut self, unit: usize, count: i64) {
        self.units[unit] = sat_add(self.units[unit], count.clamp(0, MAX_ESTIMATE));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EstimatedPlan {
    pub(crate) root: PlanEstimate,
    /// Pre-order, exactly matching the hand-written EXPLAIN renderer.
    pub(crate) nodes: Vec<PlanEstimate>,
}

impl EstimatedPlan {
    pub(crate) fn leaf(estimate: PlanEstimate) -> Self {
        Self {
            root: estimate.clone(),
            nodes: vec![estimate],
        }
    }

    pub(crate) fn parent(root: PlanEstimate, children: &[&Self]) -> Self {
        let mut nodes = vec![root.clone()];
        for child in children {
            nodes.extend(child.nodes.iter().cloned());
        }
        Self { root, nodes }
    }

    pub(crate) fn wrap(
        child: Self,
        rows: i64,
        logical_rows: i64,
        local: [i64; ESTIMATOR_UNIT_COUNT],
    ) -> Self {
        let mut root = child.root.clone();
        root.rows = rows.clamp(0, MAX_ESTIMATE);
        root.logical_rows = logical_rows.clamp(0, MAX_ESTIMATE);
        for (value, addend) in root.units.iter_mut().zip(local) {
            *value = sat_add(*value, addend);
        }
        let mut nodes = vec![root.clone()];
        nodes.extend(child.nodes);
        Self { root, nodes }
    }

    pub(crate) fn add_root_unit(&mut self, unit: usize, count: i64) {
        self.root.add_unit(unit, count);
        self.nodes[0] = self.root.clone();
    }
}

/// Canonical access-path rank from estimator.toml. Keep declaration order byte-identical to the
/// shared fact: P4 compares estimated cost first, then this rank. P3 uses it only to sort inventory.
/// `ACCESS_PATH_ORDER[kind as usize]` is the kind's shared name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ScanCandidateKind {
    Pk,
    Btree,
    Gist,
    Gin,
    PkInterval,
    IndexInterval,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CandidateInputs {
    pub(crate) kind: ScanCandidateKind,
    pub(crate) scan_rows: i64,
    pub(crate) output_rows: i64,
    pub(crate) access_pages: i64,
    pub(crate) table_height: i64,
    pub(crate) filter_nodes: i64,
    pub(crate) access_work: i64,
    pub(crate) produces_rows: bool,
}

/// The textual tie key the shared fixtures pin: `<access-path rank>:<index name>`. The planner
/// itself orders candidates by `ScanCandidateIdentity`, which compares the same two facts.
#[cfg(test)]
fn candidate_tie_key(kind: ScanCandidateKind, index_name: &str) -> String {
    format!("{}:{}", kind as usize, index_name)
}

pub(crate) fn estimate_candidate(input: CandidateInputs) -> CandidateEstimate {
    let scan_rows = input.scan_rows.clamp(0, MAX_ESTIMATE);
    let output_rows = input.output_rows.clamp(0, MAX_ESTIMATE);
    let mut units = [0; ESTIMATOR_UNIT_COUNT];
    units[UNIT_STORAGE_ROW_READ] = scan_rows;
    units[UNIT_PAGE_READ] = input.access_pages.clamp(0, MAX_ESTIMATE);
    if matches!(
        input.kind,
        ScanCandidateKind::Btree
            | ScanCandidateKind::Gist
            | ScanCandidateKind::Gin
            | ScanCandidateKind::IndexInterval
    ) {
        units[UNIT_PAGE_READ] = sat_add(
            units[UNIT_PAGE_READ],
            sat_mul(scan_rows, input.table_height.clamp(0, MAX_ESTIMATE)),
        );
    }
    units[UNIT_OPERATOR_EVAL] = sat_mul(scan_rows, input.filter_nodes.clamp(0, MAX_ESTIMATE));
    if input.produces_rows {
        units[UNIT_ROW_PRODUCED] = output_rows;
    }
    if input.kind == ScanCandidateKind::Gin {
        units[UNIT_GIN_ENTRY] = input.access_work.clamp(0, MAX_ESTIMATE);
    }
    if input.kind == ScanCandidateKind::Gist {
        units[UNIT_GIST_DESCENT] = input.access_work.clamp(0, MAX_ESTIMATE);
    }
    let cost = units
        .iter()
        .zip(ESTIMATOR_UNIT_WEIGHTS)
        .fold(0, |total, (count, weight)| {
            sat_add(total, sat_mul(*count, weight))
        });
    CandidateEstimate {
        rows: output_rows,
        units,
        cost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [ScanCandidateKind; 7] = [
        ScanCandidateKind::Pk,
        ScanCandidateKind::Btree,
        ScanCandidateKind::Gist,
        ScanCandidateKind::Gin,
        ScanCandidateKind::PkInterval,
        ScanCandidateKind::IndexInterval,
        ScanCandidateKind::Full,
    ];

    /// The kind a shared fixture names. `KINDS` is in `ACCESS_PATH_ORDER` order, which
    /// `access_kind_order_matches_shared_fact` pins.
    fn kind_named(name: &str) -> ScanCandidateKind {
        let rank = ACCESS_PATH_ORDER
            .iter()
            .position(|kind| *kind == name)
            .unwrap_or_else(|| panic!("unknown access path {name}"));
        KINDS[rank]
    }

    #[test]
    fn access_kind_order_matches_shared_fact() {
        assert_eq!(KINDS.len(), ACCESS_PATH_ORDER.len());
        for (rank, kind) in KINDS.iter().enumerate() {
            assert_eq!(*kind as usize, rank, "{kind:?}");
        }
        let names = [
            "pk",
            "btree",
            "gist",
            "gin",
            "pk_interval",
            "index_interval",
            "full",
        ];
        assert_eq!(ACCESS_PATH_ORDER, names);
    }

    fn named_selectivity(token: &str) -> Selectivity {
        match token {
            "all" => Selectivity::All,
            "zero" => Selectivity::Zero,
            "unique" => Selectivity::Unique,
            "equality" => Selectivity::fraction(SELECTIVITY_EQUALITY),
            "inequality" => Selectivity::fraction(SELECTIVITY_INEQUALITY),
            "paired_range" => Selectivity::fraction(SELECTIVITY_PAIRED_RANGE),
            "null_test" => Selectivity::fraction(SELECTIVITY_NULL_TEST),
            "match" => Selectivity::fraction(SELECTIVITY_MATCH),
            "matching" => Selectivity::fraction(SELECTIVITY_MATCHING),
            "boolean" => Selectivity::fraction(SELECTIVITY_BOOLEAN),
            "opaque" => Selectivity::fraction(SELECTIVITY_OPAQUE),
            _ => panic!("unknown selectivity token {token}"),
        }
    }

    fn postfix(tokens: &[toml::Value]) -> Selectivity {
        let mut stack = Vec::new();
        for value in tokens {
            let token = value.as_str().expect("token string");
            match token {
                "not" => {
                    let child = stack.pop().expect("not operand");
                    stack.push(Selectivity::not(child));
                }
                "and" | "or" => {
                    let rhs = stack.pop().expect("right operand");
                    let lhs = stack.pop().expect("left operand");
                    stack.push(if token == "and" {
                        lhs.and(rhs)
                    } else {
                        lhs.or(rhs)
                    });
                }
                _ => stack.push(named_selectivity(token)),
            }
        }
        assert_eq!(stack.len(), 1);
        stack.pop().unwrap()
    }

    #[test]
    fn shared_estimator_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/cost/estimator_vectors.toml"
        );
        let source = std::fs::read_to_string(path).unwrap();
        let root: toml::Value = toml::from_str(&source).unwrap();
        for row in root["arithmetic"].as_array().unwrap() {
            let a = row["a"].as_integer().unwrap();
            let b = row["b"].as_integer().unwrap();
            let actual = match row["op"].as_str().unwrap() {
                "sat_add" => sat_add(a, b),
                "sat_mul" => sat_mul(a, b),
                "scale_ceil" => scale_ceil(
                    a,
                    EstimatorFraction {
                        numerator: b,
                        denominator: row["c"].as_integer().unwrap(),
                    },
                ),
                op => panic!("unknown arithmetic op {op}"),
            };
            assert_eq!(
                actual,
                row["expected"].as_integer().unwrap(),
                "{}",
                row["id"]
            );
        }
        for row in root["predicate"].as_array().unwrap() {
            let expr = postfix(row["tokens"].as_array().unwrap());
            assert_eq!(
                estimate_rows(&expr, row["n"].as_integer().unwrap()),
                row["expected"].as_integer().unwrap(),
                "{}",
                row["id"]
            );
        }
        for row in root["candidate"].as_array().unwrap() {
            let kind = kind_named(row["kind"].as_str().unwrap());
            let estimate = estimate_candidate(CandidateInputs {
                kind,
                scan_rows: row["scan_rows"].as_integer().unwrap(),
                output_rows: row["output_rows"].as_integer().unwrap(),
                access_pages: row["access_pages"].as_integer().unwrap(),
                table_height: row["table_height"].as_integer().unwrap(),
                filter_nodes: row["filter_nodes"].as_integer().unwrap(),
                access_work: row["access_work"].as_integer().unwrap(),
                produces_rows: row["produces_rows"].as_bool().unwrap(),
            });
            assert_eq!(
                estimate.rows,
                row["est_rows"].as_integer().unwrap(),
                "{}",
                row["id"]
            );
            assert_eq!(
                estimate.cost,
                row["est_cost"].as_integer().unwrap(),
                "{}",
                row["id"]
            );
            assert_eq!(
                candidate_tie_key(kind, row["index_name"].as_str().unwrap()),
                row["tie_key"].as_str().unwrap(),
                "{}",
                row["id"]
            );
            let expected_units = row["units"].as_table().unwrap();
            for (i, id) in ESTIMATOR_UNIT_IDS.iter().enumerate() {
                let expected = expected_units
                    .get(*id)
                    .and_then(toml::Value::as_integer)
                    .unwrap_or(0);
                assert_eq!(estimate.units[i], expected, "{} unit {id}", row["id"]);
            }
        }
    }
}
