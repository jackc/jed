//! rusqlite-style ergonomic host bindings for the Rust core (spec/design/api.md §11).
//!
//! The raw handle surface (`Database`/`Session`/`Transaction`) speaks `&[Value]` parameters and
//! yields `Vec<Value>` rows — full fidelity, but the caller hand-builds `Value::Int(..)` and
//! pattern-matches every column. This module layers the **rusqlite idiom** on top, *additively*:
//! the raw `execute`/`query` stay exactly as they are (the FFI wraps and the conformance harness
//! depend on them), and these new methods give the rusqlite feel without a single change to the
//! low-level path.
//!
//! Three pieces, mirroring rusqlite's `ToSql` / `FromSql` / `Row`:
//!
//!   - [`ToValue`] converts one native Rust value into a bind [`Value`]; [`Params`] is a *set* of
//!     them, implemented for `()`, tuples up to 12-arity, arrays, slices, and `Vec` — so a caller
//!     writes `db.run("… $1, $2", (1, "ada"))?` instead of `&[Value::Int(1), Value::Text(..)]`. A
//!     raw `&[Value]` is still a `Params` (via `Value: ToValue`), so nothing is lost.
//!   - [`FromValue`] converts one column [`Value`] into a native Rust value, with `Option<T>` the
//!     nullable target (a bare `T` rejects SQL NULL — rusqlite's rule); [`Row`] wraps one result
//!     row and offers `row.get::<T>(idx)` / `row.get_by_name::<T>(name)` / `row.value(idx)`.
//!   - The methods [`Database::run`]/[`query_row`](Database::query_row)/[`query_map`](Database::query_map)/
//!     [`query_rows`](Database::query_rows) (and the same on [`Session`] and [`Transaction`]) tie
//!     them together. `run` returns the affected-row count; the `query_*` family returns typed rows.
//!
//! This is a per-impl surface, NOT the shared conformance corpus (api.md §1): each core spells the
//! ergonomics in its own idiom (Go: `database/sql` `Scan`; TS: better-sqlite3; here: rusqlite), and
//! it is unit-tested per core. The conformance contract is untouched — every method funnels through
//! the same parser + executor the raw path uses.

use std::rc::Rc;

use crate::api::{Rows, Transaction};
use crate::decimal::Decimal;
use crate::error::{EngineError, Result, SqlState};
use crate::executor::Outcome;
use crate::shared::{Database, Session};
use crate::value::Value;

// ───────────────────────────── ToValue (native → Value) ─────────────────────────────

/// Convert a native Rust value into a bind parameter [`Value`] (rusqlite's `ToSql`). Implemented
/// for the integer/float primitives, `bool`, string and byte slices, [`Decimal`], `Option<T>` (the
/// nullable binder), and `Value`/`&Value` themselves (the identity, so a raw `&[Value]` is still a
/// [`Params`]).
pub trait ToValue {
    /// Produce the bind value. Fallible only for the lossy conversions (a `u64`/`usize` past
    /// `i64::MAX` is `22003`); the common cases never fail.
    fn to_value(&self) -> Result<Value>;
}

impl ToValue for Value {
    fn to_value(&self) -> Result<Value> {
        Ok(self.clone())
    }
}

impl ToValue for &Value {
    fn to_value(&self) -> Result<Value> {
        Ok((*self).clone())
    }
}

impl ToValue for bool {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Bool(*self))
    }
}

/// Signed integers all widen losslessly into the engine's uniform `i64` representation.
macro_rules! to_value_signed {
    ($($t:ty),*) => {$(
        impl ToValue for $t {
            fn to_value(&self) -> Result<Value> {
                Ok(Value::Int(i64::from(*self)))
            }
        }
    )*};
}
to_value_signed!(i8, i16, i32, i64);

/// Small unsigned integers widen losslessly; `u64`/`usize` are range-checked against `i64::MAX`.
macro_rules! to_value_unsigned_small {
    ($($t:ty),*) => {$(
        impl ToValue for $t {
            fn to_value(&self) -> Result<Value> {
                Ok(Value::Int(i64::from(*self)))
            }
        }
    )*};
}
to_value_unsigned_small!(u8, u16, u32);

impl ToValue for u64 {
    fn to_value(&self) -> Result<Value> {
        i64::try_from(*self).map(Value::Int).map_err(|_| {
            EngineError::new(
                SqlState::NumericValueOutOfRange,
                "u64 value exceeds i64 range",
            )
        })
    }
}

impl ToValue for usize {
    fn to_value(&self) -> Result<Value> {
        i64::try_from(*self).map(Value::Int).map_err(|_| {
            EngineError::new(
                SqlState::NumericValueOutOfRange,
                "usize value exceeds i64 range",
            )
        })
    }
}

impl ToValue for f32 {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Float32(*self))
    }
}

impl ToValue for f64 {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Float64(*self))
    }
}

impl ToValue for str {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Text(self.to_string()))
    }
}

impl ToValue for &str {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Text((*self).to_string()))
    }
}

impl ToValue for String {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Text(self.clone()))
    }
}

impl ToValue for &String {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Text((*self).clone()))
    }
}

impl ToValue for [u8] {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Bytea(self.to_vec()))
    }
}

impl ToValue for &[u8] {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Bytea(self.to_vec()))
    }
}

impl ToValue for Vec<u8> {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Bytea(self.clone()))
    }
}

impl ToValue for Decimal {
    fn to_value(&self) -> Result<Value> {
        Ok(Value::Decimal(self.clone()))
    }
}

/// `Some(x)` binds `x`; `None` binds SQL NULL — the nullable binder (rusqlite's `Option` impl).
impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Result<Value> {
        match self {
            Some(v) => v.to_value(),
            None => Ok(Value::Null),
        }
    }
}

// ───────────────────────────── Params (a set of binds) ─────────────────────────────

/// A set of bind parameters (rusqlite's `Params`). Implemented for `()` (no parameters), tuples up
/// to 12-arity (`(a, b, …)`, heterogeneous), and the homogeneous containers `[T; N]` / `&[T]` /
/// `Vec<T>` where `T: ToValue` — so `&[Value]` is a `Params` too (via `Value: ToValue`), keeping the
/// raw path reachable through the ergonomic methods.
pub trait Params {
    /// Lower the parameter set into the engine's `Vec<Value>` (the order is `$1, $2, …`).
    fn into_values(self) -> Result<Vec<Value>>;
}

/// `()` is the empty parameter set — the rusqlite spelling of "no binds".
impl Params for () {
    fn into_values(self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }
}

/// Heterogeneous tuple parameter sets, `(A,)` through 12-arity. Each element is a distinct
/// [`ToValue`] type, so `(1_i32, "ada", true)` lowers to `[Int, Text, Bool]` in `$1, $2, $3` order.
macro_rules! params_tuple {
    ($($name:ident $idx:tt),+) => {
        impl<$($name: ToValue),+> Params for ($($name,)+) {
            fn into_values(self) -> Result<Vec<Value>> {
                Ok(vec![$(self.$idx.to_value()?),+])
            }
        }
    };
}
params_tuple!(A 0);
params_tuple!(A 0, B 1);
params_tuple!(A 0, B 1, C 2);
params_tuple!(A 0, B 1, C 2, D 3);
params_tuple!(A 0, B 1, C 2, D 3, E 4);
params_tuple!(A 0, B 1, C 2, D 3, E 4, F 5);
params_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6);
params_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
params_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
params_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
params_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
params_tuple!(A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);

impl<T: ToValue, const N: usize> Params for [T; N] {
    fn into_values(self) -> Result<Vec<Value>> {
        self.iter().map(ToValue::to_value).collect()
    }
}

impl<T: ToValue> Params for &[T] {
    fn into_values(self) -> Result<Vec<Value>> {
        self.iter().map(ToValue::to_value).collect()
    }
}

impl<T: ToValue> Params for Vec<T> {
    fn into_values(self) -> Result<Vec<Value>> {
        self.iter().map(ToValue::to_value).collect()
    }
}

impl<T: ToValue> Params for &Vec<T> {
    fn into_values(self) -> Result<Vec<Value>> {
        self.iter().map(ToValue::to_value).collect()
    }
}

// ───────────────────────────── FromValue (Value → native) ─────────────────────────────

/// Convert one column [`Value`] into a native Rust value (rusqlite's `FromSql`). A bare scalar `T`
/// rejects SQL NULL with `22004`; wrap it in `Option<T>` to accept NULL (`None`). A width-narrowing
/// integer read (`get::<i32>` of a value past `i32::MAX`) is `22003`; a family mismatch is `42804`.
pub trait FromValue: Sized {
    /// Read `v` as `Self`, or fail with the SQLSTATE above.
    fn from_value(v: &Value) -> Result<Self>;
}

fn mismatch(v: &Value, want: &str) -> EngineError {
    EngineError::new(
        SqlState::DatatypeMismatch,
        format!("cannot read {} as {want}", value_kind(v)),
    )
}

/// A short label for a [`Value`]'s kind, for `FromValue` error messages.
fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Int(_) => "integer",
        Value::Bool(_) => "boolean",
        Value::Float32(_) => "f32",
        Value::Float64(_) => "f64",
        Value::Text(_) => "text",
        Value::Decimal(_) => "decimal",
        Value::Bytea(_) => "bytea",
        Value::Uuid(_) => "uuid",
        Value::Timestamp(_) => "timestamp",
        Value::Timestamptz(_) => "timestamptz",
        Value::Date(_) => "date",
        Value::Interval(_) => "interval",
        Value::Composite(_) => "composite",
        Value::Array(_) => "array",
        Value::Range(_) => "range",
        Value::Json(_) => "json",
        Value::Jsonb(_) => "jsonb",
        Value::JsonPath(_) => "jsonpath",
        Value::Unfetched(_) => "unfetched",
    }
}

fn want_int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(n) => Ok(*n),
        Value::Null => Err(EngineError::new(
            SqlState::NullValueNotAllowed,
            "NULL read into a non-Option integer target (use Option<T>)",
        )),
        _ => Err(mismatch(v, "integer")),
    }
}

impl FromValue for i64 {
    fn from_value(v: &Value) -> Result<i64> {
        want_int(v)
    }
}

/// Narrowing integer reads range-check against the target width (`22003` on overflow).
macro_rules! from_value_narrow_int {
    ($($t:ty),*) => {$(
        impl FromValue for $t {
            fn from_value(v: &Value) -> Result<$t> {
                let n = want_int(v)?;
                <$t>::try_from(n).map_err(|_| {
                    EngineError::new(
                        SqlState::NumericValueOutOfRange,
                        format!("integer {n} out of range for {}", stringify!($t)),
                    )
                })
            }
        }
    )*};
}
from_value_narrow_int!(i8, i16, i32, u8, u16, u32, u64, usize);

impl FromValue for bool {
    fn from_value(v: &Value) -> Result<bool> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::Null => Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "NULL read into a non-Option bool target (use Option<bool>)",
            )),
            _ => Err(mismatch(v, "bool")),
        }
    }
}

impl FromValue for String {
    fn from_value(v: &Value) -> Result<String> {
        match v {
            Value::Text(s) => Ok(s.clone()),
            Value::Null => Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "NULL read into a non-Option String target (use Option<String>)",
            )),
            _ => Err(mismatch(v, "String")),
        }
    }
}

impl FromValue for f64 {
    fn from_value(v: &Value) -> Result<f64> {
        match v {
            Value::Float64(f) => Ok(*f),
            Value::Float32(f) => Ok(f64::from(*f)),
            Value::Int(n) => Ok(*n as f64),
            Value::Null => Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "NULL read into a non-Option f64 target (use Option<f64>)",
            )),
            _ => Err(mismatch(v, "f64")),
        }
    }
}

impl FromValue for f32 {
    fn from_value(v: &Value) -> Result<f32> {
        match v {
            Value::Float32(f) => Ok(*f),
            Value::Float64(f) => Ok(*f as f32),
            Value::Null => Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "NULL read into a non-Option f32 target (use Option<f32>)",
            )),
            _ => Err(mismatch(v, "f32")),
        }
    }
}

impl FromValue for Vec<u8> {
    fn from_value(v: &Value) -> Result<Vec<u8>> {
        match v {
            Value::Bytea(b) => Ok(b.clone()),
            Value::Uuid(u) => Ok(u.to_vec()),
            Value::Null => Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "NULL read into a non-Option Vec<u8> target (use Option<Vec<u8>>)",
            )),
            _ => Err(mismatch(v, "Vec<u8>")),
        }
    }
}

impl FromValue for Decimal {
    fn from_value(v: &Value) -> Result<Decimal> {
        match v {
            Value::Decimal(d) => Ok(d.clone()),
            Value::Null => Err(EngineError::new(
                SqlState::NullValueNotAllowed,
                "NULL read into a non-Option Decimal target (use Option<Decimal>)",
            )),
            _ => Err(mismatch(v, "Decimal")),
        }
    }
}

/// The full-fidelity escape hatch: read the column as the raw engine [`Value`] (never fails, NULL
/// included).
impl FromValue for Value {
    fn from_value(v: &Value) -> Result<Value> {
        Ok(v.clone())
    }
}

/// The nullable target: SQL NULL is `None`, anything else `Some(T::from_value(..))`. This is the
/// only way to read a column that may be NULL into a native type.
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: &Value) -> Result<Option<T>> {
        match v {
            Value::Null => Ok(None),
            _ => Ok(Some(T::from_value(v)?)),
        }
    }
}

// ───────────────────────────── Row (one typed result row) ─────────────────────────────

/// One row of a query result, with its column names (rusqlite's `Row`). Built by the `query_*`
/// methods; `get::<T>(idx)` / `get_by_name::<T>(name)` convert a column via [`FromValue`], and
/// `value(idx)` hands back the raw [`Value`]. The column names are shared (`Rc`) across every row of
/// one result, so building a `Vec<Row>` does not clone the header per row.
pub struct Row {
    names: Rc<[String]>,
    values: Vec<Value>,
}

impl Row {
    /// Convert column `idx` (0-based) to `T`. An out-of-range index is `42703`; a type/NULL
    /// mismatch is `42804`/`22004`/`22003` per [`FromValue`].
    pub fn get<T: FromValue>(&self, idx: usize) -> Result<T> {
        T::from_value(self.value(idx)?)
    }

    /// Convert the column named `name` to `T`. An unknown name is `42703`.
    pub fn get_by_name<T: FromValue>(&self, name: &str) -> Result<T> {
        let idx = self.names.iter().position(|c| c == name).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedColumn,
                format!("no column named {name:?}"),
            )
        })?;
        T::from_value(&self.values[idx])
    }

    /// The raw [`Value`] at column `idx` (0-based) — full fidelity. An out-of-range index is `42703`.
    pub fn value(&self, idx: usize) -> Result<&Value> {
        self.values.get(idx).ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedColumn,
                format!(
                    "column index {idx} out of range (row has {} columns)",
                    self.values.len()
                ),
            )
        })
    }

    /// The number of columns in this row.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the row has no columns (a `SELECT` with an empty target list).
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The output column names (shared with every other row of the same result).
    pub fn column_names(&self) -> &[String] {
        &self.names
    }
}

// ───────────────────────────── shared lowering helpers ─────────────────────────────

/// Bind `params`, run `exec`, and return the affected-row count (`0` for DDL / transaction control,
/// which carry no count — matching PostgreSQL / the raw [`Outcome`]).
fn run_with(exec: impl FnOnce(&[Value]) -> Result<Outcome>, params: impl Params) -> Result<u64> {
    let values = params.into_values()?;
    let out = exec(&values)?;
    Ok(match out {
        Outcome::Statement {
            rows_affected: Some(n),
            ..
        } => n.max(0) as u64,
        _ => 0,
    })
}

/// Bind `params`, run `q`, and collect the result into typed [`Row`]s (column names shared `Rc`).
/// Drains the cursor explicitly so a mid-drain streaming error (a `54P01` cost abort, `57014`
/// cancellation, or an arithmetic trap — streaming.md §6) is surfaced via [`Rows::error`] rather than
/// silently truncating the result.
fn rows_with(q: impl FnOnce(&[Value]) -> Result<Rows>, params: impl Params) -> Result<Vec<Row>> {
    let values = params.into_values()?;
    let mut rows = q(&values)?;
    let names: Rc<[String]> = Rc::from(rows.column_names().to_vec());
    let mut out = Vec::new();
    while let Some(values) = rows.next() {
        out.push(Row {
            names: names.clone(),
            values,
        });
    }
    rows.error()?; // surface any error raised mid-drain (streaming.md §6)
    Ok(out)
}

/// Map every typed row through `f`, short-circuiting on the first error.
fn map_rows<T>(rows: Vec<Row>, mut f: impl FnMut(&Row) -> Result<T>) -> Result<Vec<T>> {
    rows.iter().map(|r| f(r)).collect()
}

/// The four ergonomic methods, generated for each handle type (`Database`, `Session`,
/// `Transaction`). Each delegates to that type's existing raw `execute`/`query` — so the
/// conformance contract is untouched — and differs only in which method it calls. Inherent methods
/// cannot be shared by a trait without shadowing the raw `execute`/`query`, so a small macro keeps
/// the three copies identical rather than hand-drifting (CLAUDE.md §5 — data over divergence).
macro_rules! ergonomic_methods {
    ($exec:ident, $query:ident) => {
        /// Run a statement, binding native `params`, and return the affected-row count (`0` for DDL
        /// / transaction control). The ergonomic sibling of the raw `execute` (rusqlite's `execute`).
        pub fn run<P: Params>(&mut self, sql: &str, params: P) -> Result<u64> {
            run_with(|v| self.$exec(sql, v), params)
        }

        /// Run a query, binding native `params`, and return every row as a typed [`Row`]
        /// (call `row.get::<T>(..)`). The materialized analog of rusqlite's `query`.
        pub fn query_rows<P: Params>(&mut self, sql: &str, params: P) -> Result<Vec<Row>> {
            rows_with(|v| self.$query(sql, v), params)
        }

        /// Run a query, binding native `params`, and map each row through `f` (rusqlite's
        /// `query_map`, materialized). The first mapping error short-circuits.
        pub fn query_map<P: Params, T>(
            &mut self,
            sql: &str,
            params: P,
            f: impl FnMut(&Row) -> Result<T>,
        ) -> Result<Vec<T>> {
            map_rows(self.query_rows(sql, params)?, f)
        }

        /// Run a query, binding native `params`, and map its **first** row through `f`, returning
        /// `None` when the query produced no rows (rusqlite's `query_row`, but `Option` rather than
        /// a no-rows error — the idiomatic-Rust "maybe a row"). Extra rows are ignored.
        pub fn query_row<P: Params, T>(
            &mut self,
            sql: &str,
            params: P,
            f: impl FnOnce(&Row) -> Result<T>,
        ) -> Result<Option<T>> {
            match self.query_rows(sql, params)?.into_iter().next() {
                Some(row) => Ok(Some(f(&row)?)),
                None => Ok(None),
            }
        }
    };
}

impl Database {
    ergonomic_methods!(execute, query);
}

impl Session {
    ergonomic_methods!(execute, query);
}

impl Transaction<'_> {
    ergonomic_methods!(execute, query);
}
