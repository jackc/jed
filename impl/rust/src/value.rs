//! Runtime values and three-valued comparison (CLAUDE.md §4).
//!
//! All step-1 scalar types are signed integers that fit in i64, so a non-null
//! value is represented as an `i64` regardless of its declared column type. The
//! declared type governs range checks (overflow) and key-encoding width, not the
//! in-memory integer representation.

/// A runtime value: SQL NULL, or an integer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Value {
    Null,
    Int(i64),
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
    /// Render for conformance output: integers as shortest decimal, NULL as the
    /// literal `NULL` (spec/design/conformance.md §1).
    pub fn render(self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Int(n) => n.to_string(),
        }
    }

    /// Three-valued equality. NULL compared with anything (including NULL) is
    /// UNKNOWN — equality is not reflexive across NULL (CLAUDE.md §4). Integers
    /// compare by value; since all integer types promote losslessly into i64,
    /// cross-type comparison is just i64 equality (spec/types/compare.toml).
    pub fn eq3(self, other: Value) -> ThreeValued {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => bool3(a == b),
            _ => ThreeValued::Unknown,
        }
    }

    /// Three-valued ordering predicate `self < other`.
    pub fn lt3(self, other: Value) -> ThreeValued {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => bool3(a < b),
            _ => ThreeValued::Unknown,
        }
    }

    /// Three-valued ordering predicate `self > other`.
    pub fn gt3(self, other: Value) -> ThreeValued {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => bool3(a > b),
            _ => ThreeValued::Unknown,
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
