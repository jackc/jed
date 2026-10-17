//! Error `Hint` text (spec/design/error-fields.md §11): PostgreSQL's misspelled-column suggestion
//! (`errorMissingColumn` in `parse_relation.c`) and its table-alias suggestion. Pure functions over
//! the names in scope; the [`Scope`](super::Scope) gathers the candidates in the §11.1 walk order.

// PostgreSQL 18's fixed hint sentences (spec/design/error-fields.md §11.7–§11.8), byte-for-byte.

/// `42883` — no function matches a call's name and argument types (`parse_func.c`).
pub(crate) const FUNCTION_NO_MATCH: &str = "No function matches the given name and argument types. You might need to add explicit type casts.";
/// `42725` — several functions match and none is best (`parse_func.c`).
pub(crate) const FUNCTION_AMBIGUOUS: &str =
    "Could not choose a best candidate function. You might need to add explicit type casts.";
/// `42883` — no binary operator matches its operand types (`parse_oper.c` `op_error`).
pub(crate) const OPERATOR_NO_MATCH: &str = "No operator matches the given name and argument types. You might need to add explicit type casts.";
/// `42883` — no prefix operator matches its operand type (`op_error`, one argument).
pub(crate) const OPERATOR_NO_MATCH_PREFIX: &str = "No operator matches the given name and argument type. You might need to add an explicit type cast.";
/// `42725` — several operators match and none is best (`op_error`).
pub(crate) const OPERATOR_AMBIGUOUS: &str =
    "Could not choose a best candidate operator. You might need to add explicit type casts.";
/// `42883` — a sort key's type has no ordering operator (`get_sort_group_operators`).
pub(crate) const NO_ORDERING_OPERATOR: &str =
    "Use an explicit ordering operator or modify the query.";
/// `42804` — a value of the wrong type for its column or default (`transformAssignedExpr`,
/// `cookDefault`).
pub(crate) const ASSIGNMENT_MISMATCH: &str = "You will need to rewrite or cast the expression.";
/// `42804` — a recursive CTE column whose recursive term changes its type (`parse_cte.c`).
pub(crate) const RECURSIVE_TERM_TYPE: &str =
    "Cast the output of the non-recursive term to the correct type.";
/// `42P21` — two different implicit collations meet where a collation is required at plan time
/// (`select_common_collation`, `assign_collations_walker`, `parse_collate.c`).
pub(crate) const COLLATION_IMPLICIT_MISMATCH: &str =
    "You can choose the collation by applying the COLLATE clause to one or both expressions.";
/// `42P21` — a recursive CTE's recursive term changes a column's collation (`parse_cte.c`).
pub(crate) const RECURSIVE_TERM_COLLATION: &str =
    "Use the COLLATE clause to set the collation of the non-recursive term.";
/// `42P22` — no collation could be determined for a value that needs one (`varstr_cmp`,
/// `intorel_startup`, …).
pub(crate) const INDETERMINATE_COLLATION: &str =
    "Use the COLLATE clause to set the collation explicitly.";
/// `2BP01` — a `DROP` (or `ALTER TABLE … DROP`) that other objects depend on, where the statement
/// accepts `CASCADE` (`reportDependentObjects`, `dependency.c`).
pub(crate) const DROP_CASCADE: &str = "Use DROP ... CASCADE to drop the dependent objects too.";

/// `2BP01` — dropping an index a constraint owns (`findDependentObjects`, `dependency.c`).
pub(crate) fn drop_owner_instead(constraint: &str, table: &str) -> String {
    format!("You can drop constraint {constraint} on table {table} instead.")
}

/// `42703` — the column is in a FROM item to the left of a non-LATERAL subquery (`errorMissingColumn`).
pub(crate) const LATERAL_COLUMN: &str =
    "To reference that column, you must mark this subquery with LATERAL.";
/// `42P01` — the table is a FROM item to the left of a non-LATERAL subquery (`errorMissingRTE`).
pub(crate) const LATERAL_TABLE: &str =
    "To reference that table, you must mark this subquery with LATERAL.";
/// `42703` — the column's relation is visible only by its qualifier (`errorMissingColumn`).
pub(crate) const QUALIFY_COLUMN: &str =
    "To reference that column, you must use a table-qualified name.";
/// `42703` — several unreachable relations have the column (`errorMissingColumn`).
pub(crate) const TRY_QUALIFIED: &str = "Try using a table-qualified name.";

/// PostgreSQL's `MAX_FUZZY_DISTANCE`; the search starts one above it.
const MAX_FUZZY_DISTANCE: usize = 3;

/// The Levenshtein distance from `a` to `b` over characters, every edit costing 1. PostgreSQL's
/// bounded `varstr_levenshtein_less_equal` may stop early above its bound, but a distance above the
/// bound is never used, so the full distance gives the same outcome (§11.1).
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The fuzzy-match state of §11.1 step 3, fed candidates in walk order.
pub(crate) struct FuzzyColumnMatch<'n> {
    /// The written column name `c`.
    target: &'n str,
    /// The written qualifier `q`, or `None` for a bare reference.
    qualifier: Option<&'n str>,
    distance: usize,
    first: Option<(String, String)>,
    second: Option<(String, String)>,
    exact: bool,
}

impl<'n> FuzzyColumnMatch<'n> {
    pub(crate) fn new(qualifier: Option<&'n str>, target: &'n str) -> Self {
        FuzzyColumnMatch {
            target,
            qualifier,
            distance: MAX_FUZZY_DISTANCE + 1,
            first: None,
            second: None,
            exact: false,
        }
    }

    /// The relation penalty for a relation labeled `label` (§11.1 step 2).
    pub(crate) fn penalty(&self, label: &str) -> usize {
        self.qualifier.map_or(0, |q| levenshtein(q, label))
    }

    /// Offer one candidate column of the relation labeled `label`, whose penalty is `penalty`.
    pub(crate) fn offer(&mut self, label: &str, penalty: usize, column: &str) {
        if penalty > self.distance {
            return;
        }
        let d = levenshtein(column, self.target);
        if penalty == 0 && d == 0 {
            self.exact = true;
        }
        // `len` in bytes, as PostgreSQL's `strlen`.
        if d > self.target.len() / 2 {
            return;
        }
        let d = d + penalty;
        let candidate = (label.to_string(), column.to_string());
        if d < self.distance {
            self.distance = d;
            self.first = Some(candidate);
            self.second = None;
        } else if d == self.distance {
            if self.second.is_some() {
                // Three at one distance are too many; keep the distance so only a closer match counts.
                self.first = None;
                self.second = None;
            } else if self.first.is_some() {
                self.second = Some(candidate);
            }
        }
    }

    /// The hint text (§11.1 step 5), or `None` — no match, or an exact match (step 4).
    pub(crate) fn hint(self) -> Option<String> {
        if self.exact {
            return None;
        }
        match (self.first, self.second) {
            (Some((l1, c1)), None) => Some(format!(
                "Perhaps you meant to reference the column \"{l1}.{c1}\"."
            )),
            (Some((l1, c1)), Some((l2, c2))) => Some(format!(
                "Perhaps you meant to reference the column \"{l1}.{c1}\" or the column \"{l2}.{c2}\"."
            )),
            _ => None,
        }
    }
}

/// The §11.2 table-alias hint for an aliased relation labeled `label`.
pub(crate) fn alias_hint(label: &str) -> String {
    format!("Perhaps you meant to reference the table alias \"{label}\".")
}

/// `42703` Detail: one relation the reference cannot see has the column (`errorMissingColumn`,
/// spec/design/error-fields.md §11.8).
pub(crate) fn unreachable_column_detail(column: &str, label: &str) -> String {
    format!(
        "There is a column named \"{column}\" in table \"{label}\", but it cannot be referenced from this part of the query."
    )
}

/// `42703` Detail: several relations the reference cannot see have the column (`errorMissingColumn`).
pub(crate) fn unreachable_columns_detail(column: &str) -> String {
    format!(
        "There are columns named \"{column}\", but they are in tables that cannot be referenced from this part of the query."
    )
}

/// `42P01` Detail: the qualifier names a relation the reference cannot see (`errorMissingRTE`).
pub(crate) fn unreachable_table_detail(label: &str) -> String {
    format!(
        "There is an entry for table \"{label}\", but it cannot be referenced from this part of the query."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("é", "e"), 1);
    }
}
