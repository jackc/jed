//! Runtime values and three-valued comparison (CLAUDE.md §4).
//!
//! An integer value is held as an `i64` regardless of its declared column type (the
//! type governs range checks and key-encoding width, not the representation); a `text`
//! value holds its UTF-8 `String`; a `decimal` value holds an exact `Decimal`
//! (spec/design/decimal.md). Because `Text`/`Decimal` own heap data, `Value` is `Clone`,
//! not `Copy` — the comparison/render helpers borrow (`&self`, `&Value`) rather than
//! consume, and the executor clones a value only when reading it out of a stored row.

use crate::decimal::Decimal;

/// A runtime value: SQL NULL, an integer, a boolean, or a text string.
///
/// A `Bool` value is produced by comparisons and logical connectives, can be
/// projected/rendered, and — now that boolean is storable (spec/design/types.md §9) —
/// is stored in a boolean column. A NULL boolean (unknown) is represented as
/// `Value::Null`, so `{Bool(true), Bool(false), Null}` is the three-valued domain;
/// booleans compare by value, false < true. `Text` is a stored non-integer value; it
/// compares by the `C` collation (UTF-8 byte / code-point order — types.md §11).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Value {
    Null,
    Int(i64),
    Bool(bool),
    Text(String),
    /// An exact base-10 decimal (spec/design/decimal.md). Its `PartialEq`/`Eq`/`Hash` are
    /// **value-canonical** (`1.5 == 1.50`), so DISTINCT/GROUP BY over decimals compare by
    /// value while `render` still preserves the display scale.
    Decimal(Decimal),
}

/// Compare two numeric values by value, promoting an integer operand to decimal when its
/// sibling is decimal (the `integer ↔ decimal` cross-family rule — spec/types/compare.toml).
/// `None` for any non-numeric pair (text, boolean, NULL), which the callers treat as UNKNOWN.
fn numeric_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Decimal(x), Value::Decimal(y)) => Some(x.cmp_value(y)),
        (Value::Int(x), Value::Decimal(y)) => Some(Decimal::from_i64(*x).cmp_value(y)),
        (Value::Decimal(x), Value::Int(y)) => Some(x.cmp_value(&Decimal::from_i64(*y))),
        _ => None,
    }
}

/// The result of a three-valued comparison (CLAUDE.md §4): TRUE / FALSE / UNKNOWN.
/// UNKNOWN arises whenever a NULL participates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreeValued {
    True,
    False,
    Unknown,
}

impl ThreeValued {
    /// A WHERE predicate selects a row only when it evaluates to TRUE; UNKNOWN
    /// (NULL) and FALSE both reject (CLAUDE.md §4).
    pub fn is_true(self) -> bool {
        matches!(self, ThreeValued::True)
    }

    /// Three-valued OR (Kleene logic): TRUE if either is TRUE, else UNKNOWN if
    /// either is UNKNOWN, else FALSE. Used to build `<=` / `>=` from `<`/`>` and
    /// `=` so a NULL operand still yields UNKNOWN rather than a wrong FALSE.
    pub fn or(self, other: ThreeValued) -> ThreeValued {
        match (self, other) {
            (ThreeValued::True, _) | (_, ThreeValued::True) => ThreeValued::True,
            (ThreeValued::Unknown, _) | (_, ThreeValued::Unknown) => ThreeValued::Unknown,
            _ => ThreeValued::False,
        }
    }
}

impl Value {
    /// Render for conformance output: integers as shortest decimal, booleans as the
    /// canonical `true`/`false`, text verbatim (the `T` tag — no quoting), NULL
    /// (including a NULL/unknown boolean) as the literal `NULL` (spec/design/conformance.md
    /// §1; the canonical spelling is a §8 decision).
    pub fn render(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Int(n) => n.to_string(),
            Value::Bool(true) => "true".to_string(),
            Value::Bool(false) => "false".to_string(),
            Value::Text(s) => s.clone(),
            // Decimal renders as its canonical base-10 string, preserving display scale
            // (the `D` tag — spec/design/decimal.md §6).
            Value::Decimal(d) => d.render(),
        }
    }

    /// Whether this value is boolean TRUE. A WHERE expression keeps a row only when it
    /// is TRUE; FALSE and NULL/unknown both reject (CLAUDE.md §4, Kleene).
    pub fn is_true(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    /// Three-valued equality. NULL compared with anything (including NULL) is
    /// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers
    /// compare by value (all integer types promote losslessly into i64); text compares
    /// by the `C` collation — raw UTF-8 bytes, which for UTF-8 equals code-point order
    /// (spec/design/types.md §11); booleans compare by value (false < true). A mixed
    /// cross-family pair never reaches here — the resolver rejects it (42804) — so any
    /// non-matching variant pair is a NULL operand.
    pub fn eq3(&self, other: &Value) -> ThreeValued {
        if let Some(ord) = numeric_cmp(self, other) {
            return bool3(ord == std::cmp::Ordering::Equal);
        }
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => bool3(a.as_bytes() == b.as_bytes()),
            (Value::Bool(a), Value::Bool(b)) => bool3(a == b),
            _ => ThreeValued::Unknown,
        }
    }

    /// Three-valued ordering predicate `self < other` (numerics by value with int↔decimal
    /// promotion; text by `C` collation = UTF-8 byte order; boolean by value, false < true).
    pub fn lt3(&self, other: &Value) -> ThreeValued {
        if let Some(ord) = numeric_cmp(self, other) {
            return bool3(ord == std::cmp::Ordering::Less);
        }
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => bool3(a.as_bytes() < b.as_bytes()),
            (Value::Bool(a), Value::Bool(b)) => bool3(a < b),
            _ => ThreeValued::Unknown,
        }
    }

    /// Three-valued ordering predicate `self > other` (numerics by value with int↔decimal
    /// promotion; text by `C` collation = UTF-8 byte order; boolean by value, false < true).
    pub fn gt3(&self, other: &Value) -> ThreeValued {
        if let Some(ord) = numeric_cmp(self, other) {
            return bool3(ord == std::cmp::Ordering::Greater);
        }
        match (self, other) {
            (Value::Text(a), Value::Text(b)) => bool3(a.as_bytes() > b.as_bytes()),
            (Value::Bool(a), Value::Bool(b)) => bool3(a > b),
            _ => ThreeValued::Unknown,
        }
    }

    /// NULL-safe equality — the `IS NOT DISTINCT FROM` primitive (CLAUDE.md §4,
    /// spec/design/functions.md §3). NULL is a comparable value, not a poison: two NULLs
    /// are "not distinct" (the same), a NULL and a present value are distinct, and two
    /// present values (integer or text) compare by value. The answer is **always**
    /// definite — there is no UNKNOWN here, which is the whole point of the operator.
    /// `IS DISTINCT FROM` is the negation of this. (The resolver guarantees same-family
    /// non-null operands, so they reduce to `eq3`, which is definite when neither is NULL.)
    pub fn not_distinct_from(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Null, _) | (_, Value::Null) => false,
            _ => self.eq3(other) == ThreeValued::True,
        }
    }
}

fn bool3(b: bool) -> ThreeValued {
    if b {
        ThreeValued::True
    } else {
        ThreeValued::False
    }
}

// --- boolean Value <-> ThreeValued bridges, and the Kleene connectives ----------
// A boolean Value carries the three-valued domain directly: TRUE = Bool(true),
// FALSE = Bool(false), UNKNOWN = Null. The comparison primitives (eq3/lt3/gt3) speak
// `ThreeValued`; `from3` lifts their result into a boolean Value, and `to3` projects
// a Value back so the AND/OR/NOT connectives can reuse `ThreeValued::or`.

/// Lift a three-valued result into a boolean Value (UNKNOWN → NULL).
pub fn from3(t: ThreeValued) -> Value {
    match t {
        ThreeValued::True => Value::Bool(true),
        ThreeValued::False => Value::Bool(false),
        ThreeValued::Unknown => Value::Null,
    }
}

/// Project a Value into the three-valued domain. A non-boolean Value (NULL, text, or
/// defensively an Int that the resolver should never route here) is UNKNOWN.
pub fn to3(v: &Value) -> ThreeValued {
    match v {
        Value::Bool(true) => ThreeValued::True,
        Value::Bool(false) => ThreeValued::False,
        _ => ThreeValued::Unknown,
    }
}

/// Kleene AND: FALSE dominates (`false AND unknown = false`); TRUE only when both are
/// TRUE; otherwise UNKNOWN (NULL). This is why AND is not plain NULL-propagation.
pub fn and3(a: &Value, b: &Value) -> Value {
    match (to3(a), to3(b)) {
        (ThreeValued::False, _) | (_, ThreeValued::False) => Value::Bool(false),
        (ThreeValued::True, ThreeValued::True) => Value::Bool(true),
        _ => Value::Null,
    }
}

/// Kleene OR: TRUE dominates (`true OR unknown = true`); built on `ThreeValued::or`.
pub fn or3(a: &Value, b: &Value) -> Value {
    from3(to3(a).or(to3(b)))
}

/// Kleene NOT: genuine propagation — `NOT NULL = NULL`.
pub fn not3(a: &Value) -> Value {
    match to3(a) {
        ThreeValued::True => Value::Bool(false),
        ThreeValued::False => Value::Bool(true),
        ThreeValued::Unknown => Value::Null,
    }
}
