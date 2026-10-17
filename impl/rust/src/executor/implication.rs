//! The partial-index predicate-implication prover (spec/design/indexes.md §9.1): does a query's
//! WHERE (a list of filter expressions, each an AND-chain) imply a partial index's predicate? A
//! deterministic, bounded subset of PostgreSQL's `predtest.c`. Sound-if-incomplete: `false` means
//! "not proven", which only costs the index (a correct full scan + residual filter).
//!
//! The filter is in GLOBAL column coordinates; the predicate is table-local and is matched shifted by
//! `offset` (the relation's global column base), exactly as [`rexpr_eq_shifted`] does.

use super::*;
use std::cmp::Ordering;

/// The prover's step budget (indexes.md §9.1): every invocation of one of its three procedures
/// consumes one step, and an exhausted budget answers "not proven".
const IMPLICATION_STEP_BUDGET: usize = 10_000;
/// The largest `x = ANY(array)` premise the prover expands (PostgreSQL's `MAX_SAOP_ARRAY_SIZE`).
const IMPLICATION_MAX_ARRAY: usize = 100;

/// Does the conjunction of `filters` imply the partial-index predicate `pred` (indexes.md §9.1)?
pub(crate) fn filters_imply_predicate(filters: &[&RExpr], pred: &RExpr, offset: usize) -> bool {
    let mut conjuncts = Vec::new();
    for f in filters {
        flatten_and(f, &mut conjuncts);
    }
    let mut prover = Prover {
        offset,
        steps: IMPLICATION_STEP_BUDGET,
    };
    prover.implied_by_conjuncts(&conjuncts, pred)
}

/// The single-filter form of [`filters_imply_predicate`].
pub(crate) fn filter_implies_predicate(filter: &RExpr, pred: &RExpr, offset: usize) -> bool {
    filters_imply_predicate(&[filter], pred, offset)
}

/// Whether `pred` (table-local) is NULL-rejecting (indexes.md §9.3): never TRUE for a row whose every
/// column is NULL, as for a NULL-extended outer-join row. A syntactic, conservative test.
pub(crate) fn predicate_rejects_null(pred: &RExpr) -> bool {
    let bare = |e: &RExpr| matches!(e, RExpr::Column(_));
    match pred {
        RExpr::And(a, b) => predicate_rejects_null(a) || predicate_rejects_null(b),
        RExpr::Or(a, b) => predicate_rejects_null(a) && predicate_rejects_null(b),
        RExpr::Compare { lhs, rhs, .. } => bare(lhs) || bare(rhs),
        RExpr::IsNull {
            operand,
            negated: true,
        } => bare(operand),
        RExpr::Column(_) => true,
        RExpr::Not(inner) => bare(inner),
        RExpr::Quantified {
            all: false, lhs, ..
        } => bare(lhs),
        RExpr::Like { lhs, .. } | RExpr::Regex { lhs, .. } => bare(lhs),
        _ => false,
    }
}

fn flatten_and<'a>(e: &'a RExpr, out: &mut Vec<&'a RExpr>) {
    if let RExpr::And(l, r) = e {
        flatten_and(l, out);
        flatten_and(r, out);
    } else {
        out.push(e);
    }
}

/// A comparison constant the prover can order: one scalar family, compared within its own variant.
enum ProverConst<'a> {
    Int(i64),
    Text(&'a str),
    Decimal(&'a Decimal),
    Date(i32),
    Timestamp(i64),
    Timestamptz(i64),
    Bool(bool),
    Uuid(&'a [u8; 16]),
}

impl<'a> ProverConst<'a> {
    fn from_rexpr(e: &'a RExpr) -> Option<Self> {
        Some(match e {
            RExpr::ConstInt(n) => ProverConst::Int(*n),
            RExpr::ConstText(s) => ProverConst::Text(s),
            RExpr::ConstDecimal(d) => ProverConst::Decimal(d),
            RExpr::ConstDate(d) => ProverConst::Date(*d),
            RExpr::ConstTimestamp(t) => ProverConst::Timestamp(*t),
            RExpr::ConstTimestamptz(t) => ProverConst::Timestamptz(*t),
            RExpr::ConstBool(b) => ProverConst::Bool(*b),
            RExpr::ConstUuid(u) => ProverConst::Uuid(u),
            _ => return None,
        })
    }

    fn from_value(v: &'a Value) -> Option<Self> {
        Some(match v {
            Value::Int(n) => ProverConst::Int(*n),
            Value::Text(s) => ProverConst::Text(s),
            Value::Decimal(d) => ProverConst::Decimal(d),
            Value::Date(d) => ProverConst::Date(*d),
            Value::Timestamp(t) => ProverConst::Timestamp(*t),
            Value::Timestamptz(t) => ProverConst::Timestamptz(*t),
            Value::Bool(b) => ProverConst::Bool(*b),
            Value::Uuid(u) => ProverConst::Uuid(u),
            _ => return None,
        })
    }

    /// The order of two constants of the SAME family; `None` across families (never compared).
    fn cmp(&self, other: &Self) -> Option<Ordering> {
        use ProverConst::*;
        Some(match (self, other) {
            (Int(a), Int(b)) => a.cmp(b),
            (Text(a), Text(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Decimal(a), Decimal(b)) => a.cmp_value(b),
            (Date(a), Date(b)) => a.cmp(b),
            (Timestamp(a), Timestamp(b)) => a.cmp(b),
            (Timestamptz(a), Timestamptz(b)) => a.cmp(b),
            (Bool(a), Bool(b)) => a.cmp(b),
            (Uuid(a), Uuid(b)) => a.cmp(b),
            _ => return None,
        })
    }
}

/// A premise atom `operand op value` known TRUE for every row the filter admits. `ordering` is
/// whether the source comparison carried no collation (a `C`/byte or non-text comparison) — only then
/// may an ordering operator take part in a comparison implication.
struct Premise<'a> {
    operand: &'a RExpr,
    op: CmpOp,
    value: ProverConst<'a>,
    c_order: bool,
}

/// Normalize `w` to `operand op const` when it is a comparison with a non-NULL prover constant on
/// one side (the operator flipped when the constant is on the left).
fn comparison_atom(w: &RExpr) -> Option<(&RExpr, CmpOp, ProverConst<'_>, bool)> {
    let RExpr::Compare {
        op,
        lhs,
        rhs,
        collation,
    } = w
    else {
        return None;
    };
    let c_order = collation.is_none();
    if let Some(c) = ProverConst::from_rexpr(rhs) {
        return Some((lhs, *op, c, c_order));
    }
    if let Some(c) = ProverConst::from_rexpr(lhs) {
        return Some((rhs, flip_cmp(*op), c, c_order));
    }
    None
}

/// Does `x op1 c1` (TRUE) imply `x op2 c2`, given `ord = cmp(c1, c2)`? PostgreSQL's btree
/// implication table (predtest.c `BT_implic_table`), over a total order.
fn comparison_implies(op1: CmpOp, op2: CmpOp, ord: Ordering) -> bool {
    use CmpOp::*;
    use Ordering::*;
    match op1 {
        Eq => match op2 {
            Eq => ord == Equal,
            Ne => ord != Equal,
            Lt => ord == Less,
            Le => ord != Greater,
            Gt => ord == Greater,
            Ge => ord != Less,
        },
        Lt => matches!(op2, Lt | Le | Ne) && ord != Greater,
        Le => match op2 {
            Lt | Ne => ord == Less,
            Le => ord != Greater,
            _ => false,
        },
        Gt => matches!(op2, Gt | Ge | Ne) && ord != Less,
        Ge => match op2 {
            Gt | Ne => ord == Greater,
            Ge => ord != Less,
            _ => false,
        },
        Ne => op2 == Ne && ord == Equal,
    }
}

struct Prover {
    offset: usize,
    steps: usize,
}

impl Prover {
    fn tick(&mut self) -> bool {
        if self.steps == 0 {
            return false;
        }
        self.steps -= 1;
        true
    }

    /// Procedure 1 — the filter's top-level conjunct list `ws` implies `p`.
    fn implied_by_conjuncts(&mut self, ws: &[&RExpr], p: &RExpr) -> bool {
        if !self.tick() {
            return false;
        }
        if let RExpr::And(a, b) = p {
            return self.implied_by_conjuncts(ws, a) && self.implied_by_conjuncts(ws, b);
        }
        for w in ws {
            if self.clause_implies(w, p) {
                return true;
            }
        }
        if let RExpr::Or(a, b) = p {
            return self.implied_by_conjuncts(ws, a) || self.implied_by_conjuncts(ws, b);
        }
        false
    }

    /// Procedure 2 — one filter clause `w` implies `p`.
    fn clause_implies(&mut self, w: &RExpr, p: &RExpr) -> bool {
        if !self.tick() {
            return false;
        }
        if rexpr_eq_shifted(w, p, self.offset) {
            return true;
        }
        if let RExpr::And(a, b) = p {
            return self.clause_implies(w, a) && self.clause_implies(w, b);
        }
        match w {
            RExpr::Or(a, b) => return self.clause_implies(a, p) && self.clause_implies(b, p),
            RExpr::And(a, b) => return self.clause_implies(a, p) || self.clause_implies(b, p),
            _ => {}
        }
        if let Some((operand, elems, c_order)) = any_array_premise(w) {
            // `x = ANY(array)` is TRUE only when x equals some non-NULL element, so it implies `p`
            // iff every non-NULL element's `x = e` does (and at least one element is non-NULL).
            let mut any = false;
            for e in elems {
                let Some(value) = e else { continue };
                any = true;
                let premise = Premise {
                    operand,
                    op: CmpOp::Eq,
                    value,
                    c_order,
                };
                if !self.atom_implies(&premise, p) {
                    return false;
                }
            }
            return any;
        }
        if let RExpr::Or(a, b) = p {
            if self.clause_implies(w, a) || self.clause_implies(w, b) {
                return true;
            }
        }
        match comparison_atom(w) {
            Some((operand, op, value, c_order)) => {
                let premise = Premise {
                    operand,
                    op,
                    value,
                    c_order,
                };
                self.atom_implies(&premise, p)
            }
            None => false,
        }
    }

    /// Procedure 3 — a premise atom implies `p`.
    fn atom_implies(&mut self, premise: &Premise<'_>, p: &RExpr) -> bool {
        if !self.tick() {
            return false;
        }
        match p {
            RExpr::And(a, b) => {
                return self.atom_implies(premise, a) && self.atom_implies(premise, b);
            }
            RExpr::Or(a, b) => {
                return self.atom_implies(premise, a) || self.atom_implies(premise, b);
            }
            // A TRUE comparison with a non-NULL scalar constant has a non-NULL operand.
            RExpr::IsNull {
                operand,
                negated: true,
            } => return rexpr_eq_shifted(premise.operand, operand, self.offset),
            _ => {}
        }
        let Some((operand, op2, c2, c_order2)) = comparison_atom(p) else {
            return false;
        };
        if !rexpr_eq_shifted(premise.operand, operand, self.offset) {
            return false;
        }
        let ordering = |op: CmpOp| !matches!(op, CmpOp::Eq | CmpOp::Ne);
        if (ordering(premise.op) || ordering(op2)) && !(premise.c_order && c_order2) {
            return false;
        }
        match premise.value.cmp(&c2) {
            Some(ord) => comparison_implies(premise.op, op2, ord),
            None => false,
        }
    }
}

/// `x = ANY(array)` over a constant array of at most [`IMPLICATION_MAX_ARRAY`] elements, as its
/// operand and the elements (`None` for a NULL or non-prover element — a NULL never makes the ANY
/// TRUE; any other non-prover element makes the whole premise unusable), plus whether the
/// quantifier carried no collation.
fn any_array_premise(w: &RExpr) -> Option<(&RExpr, Vec<Option<ProverConst<'_>>>, bool)> {
    let RExpr::Quantified {
        op: CmpOp::Eq,
        all: false,
        row_wise: false,
        lhs,
        array,
        collation,
        ..
    } = w
    else {
        return None;
    };
    let mut out = Vec::new();
    match array.as_ref() {
        RExpr::ConstArray(a) => {
            if a.elements.len() > IMPLICATION_MAX_ARRAY {
                return None;
            }
            for v in &a.elements {
                if matches!(v, Value::Null) {
                    out.push(None);
                } else {
                    out.push(Some(ProverConst::from_value(v)?));
                }
            }
        }
        RExpr::Array {
            elems,
            nested: false,
        } => {
            if elems.len() > IMPLICATION_MAX_ARRAY {
                return None;
            }
            for e in elems {
                if matches!(e, RExpr::ConstNull) {
                    out.push(None);
                } else {
                    out.push(Some(ProverConst::from_rexpr(e)?));
                }
            }
        }
        _ => return None,
    }
    Some((lhs, out, collation.is_none()))
}
