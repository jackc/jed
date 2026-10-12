//! Error `Hint` text (spec/design/error-fields.md §11): PostgreSQL's misspelled-column suggestion
//! (`errorMissingColumn` in `parse_relation.c`) and its table-alias suggestion. Pure functions over
//! the names in scope; the [`Scope`](super::Scope) gathers the candidates in the §11.1 walk order.

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
