//! Deterministic Path-B estimator arithmetic shared by planner shadow estimates and fixtures.

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

pub(crate) fn sat_add(a: i64, b: i64) -> i64 {
    a.saturating_add(b).min(MAX_ESTIMATE)
}

pub(crate) fn sat_mul(a: i64, b: i64) -> i64 {
    a.saturating_mul(b).min(MAX_ESTIMATE)
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
    pub(crate) tie_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CandidateInputs<'a> {
    pub(crate) kind: &'a str,
    pub(crate) index_name: &'a str,
    pub(crate) scan_rows: i64,
    pub(crate) output_rows: i64,
    pub(crate) access_pages: i64,
    pub(crate) table_height: i64,
    pub(crate) filter_nodes: i64,
    pub(crate) access_work: i64,
    pub(crate) produces_rows: bool,
}

fn access_rank(kind: &str) -> usize {
    ACCESS_PATH_ORDER
        .iter()
        .position(|candidate| *candidate == kind)
        .unwrap_or(ACCESS_PATH_ORDER.len())
}

pub(crate) fn candidate_tie_key(kind: &str, index_name: &str) -> String {
    format!("{}:{}", access_rank(kind), index_name)
}

pub(crate) fn estimate_candidate(input: CandidateInputs<'_>) -> CandidateEstimate {
    let scan_rows = input.scan_rows.clamp(0, MAX_ESTIMATE);
    let output_rows = input.output_rows.clamp(0, MAX_ESTIMATE);
    let mut units = [0; ESTIMATOR_UNIT_COUNT];
    units[UNIT_STORAGE_ROW_READ] = scan_rows;
    units[UNIT_PAGE_READ] = input.access_pages.clamp(0, MAX_ESTIMATE);
    if matches!(input.kind, "btree" | "gist" | "gin" | "index_interval") {
        units[UNIT_PAGE_READ] = sat_add(
            units[UNIT_PAGE_READ],
            sat_mul(scan_rows, input.table_height.clamp(0, MAX_ESTIMATE)),
        );
    }
    units[UNIT_OPERATOR_EVAL] = sat_mul(scan_rows, input.filter_nodes.clamp(0, MAX_ESTIMATE));
    if input.produces_rows {
        units[UNIT_ROW_PRODUCED] = output_rows;
    }
    if input.kind == "gin" {
        units[UNIT_GIN_ENTRY] = input.access_work.clamp(0, MAX_ESTIMATE);
    }
    if input.kind == "gist" {
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
        tie_key: candidate_tie_key(input.kind, input.index_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let estimate = estimate_candidate(CandidateInputs {
                kind: row["kind"].as_str().unwrap(),
                index_name: row["index_name"].as_str().unwrap(),
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
                estimate.tie_key,
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
