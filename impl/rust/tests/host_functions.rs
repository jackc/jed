//! Host-defined scalar functions (spec/design/extensibility.md §4.2 / §5.1, delivery step 3).
//! The registry/resolve/eval injection seam is a HOST-API surface the conformance corpus cannot
//! express (it registers no host code), so it is tested per core (CLAUDE.md §10 — host-API is one
//! of the sanctioned unit-test categories). These assertions must mirror the Go/TS host-function
//! tests one-for-one.

use std::sync::Arc;

use jed::value::Value;
use jed::{
    CreateOptions, Database, ExtensionRegistry, HostFunction, Outcome, ScalarType, Session,
    SessionOptions, Volatility,
};

/// `host_add(i64, i64) -> i64` — integer sum (strict: never sees NULL).
fn add_i64() -> HostFunction {
    HostFunction::new(
        "host_add",
        vec![ScalarType::Int64, ScalarType::Int64],
        ScalarType::Int64,
        Box::new(|args: &[Value]| -> jed::Result<Value> {
            let (Value::Int(a), Value::Int(b)) = (&args[0], &args[1]) else {
                unreachable!("strict + resolved i64 args")
            };
            Ok(Value::Int(a + b))
        }),
    )
    .volatility(Volatility::Immutable)
    .cross_core(true)
}

/// `host_add(text, text) -> text` — concatenation, a same-name overload on a different signature.
fn add_text() -> HostFunction {
    HostFunction::new(
        "host_add",
        vec![ScalarType::Text, ScalarType::Text],
        ScalarType::Text,
        Box::new(|args: &[Value]| -> jed::Result<Value> {
            let (Value::Text(a), Value::Text(b)) = (&args[0], &args[1]) else {
                unreachable!("strict + resolved text args")
            };
            Ok(Value::Text(format!("{a}{b}")))
        }),
    )
}

fn registry(funcs: Vec<HostFunction>) -> Arc<ExtensionRegistry> {
    let mut reg = ExtensionRegistry::new();
    for f in funcs {
        reg.register_function(f)
            .unwrap_or_else(|e| panic!("register: {}", e.message));
    }
    Arc::new(reg)
}

fn db_with_ext(extensions: Arc<ExtensionRegistry>, stmts: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions {
        extensions,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    for s in stmts {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn query(db: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match db
        .query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
    {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn one(db: &mut Session, sql: &str) -> Value {
    let rows = query(db, sql);
    assert_eq!(rows.len(), 1, "{sql:?} expected exactly one row");
    rows.into_iter().next().unwrap().into_iter().next().unwrap()
}

#[test]
fn host_scalar_function_over_literals() {
    let mut db = db_with_ext(registry(vec![add_i64()]), &[]);
    assert_eq!(one(&mut db, "SELECT host_add(2, 3)"), Value::Int(5));
    assert_eq!(
        one(&mut db, "SELECT host_add(host_add(1, 1), 40)"),
        Value::Int(42)
    );
}

#[test]
fn host_scalar_function_over_columns() {
    let mut db = db_with_ext(
        registry(vec![add_i64()]),
        &[
            "CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)",
            "INSERT INTO t VALUES (1, 10, 20), (2, 100, 1)",
        ],
    );
    let mut rows = query(&mut db, "SELECT host_add(a, b) FROM t ORDER BY id");
    let got: Vec<Value> = rows
        .drain(..)
        .map(|r| r.into_iter().next().unwrap())
        .collect();
    assert_eq!(got, vec![Value::Int(30), Value::Int(101)]);
}

#[test]
fn host_function_is_strict_on_typed_null() {
    // A NULL-valued argument of a KNOWN type short-circuits to NULL before the kernel runs (§4.2);
    // the kernel (which unreachable!s on a non-Int arg) is never called.
    let mut db = db_with_ext(
        registry(vec![add_i64()]),
        &[
            "CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)",
            "INSERT INTO t VALUES (1, NULL, 20)",
        ],
    );
    assert_eq!(one(&mut db, "SELECT host_add(a, b) FROM t"), Value::Null);
}

#[test]
fn bare_null_literal_finds_no_overload() {
    // A bare untyped NULL matches no concrete scalar signature — 42883, exactly as a built-in
    // (`abs(NULL)`) behaves (resolve_agg.rs arg_family). Strictness is an eval-time property of a
    // TYPED null, not a resolution one.
    let mut db = db_with_ext(registry(vec![add_i64()]), &[]);
    assert_eq!(
        db.query_outcome("SELECT host_add(NULL, 3)", &[])
            .unwrap_err()
            .code(),
        "42883"
    );
}

#[test]
fn overload_by_signature() {
    let mut db = db_with_ext(registry(vec![add_i64(), add_text()]), &[]);
    assert_eq!(one(&mut db, "SELECT host_add(2, 3)"), Value::Int(5));
    assert_eq!(
        one(&mut db, "SELECT host_add('foo', 'bar')"),
        Value::Text("foobar".into())
    );
}

#[test]
fn builtin_wins_over_host_same_signature() {
    // Registering a host `abs(i64)` is accepted but never reached — the built-in `abs` shadows it
    // (§4.2). If the host kernel (returning a sentinel 999) ran, abs(-5) would be 999.
    let host_abs = HostFunction::new(
        "abs",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(|_: &[Value]| -> jed::Result<Value> { Ok(Value::Int(999)) }),
    );
    let mut db = db_with_ext(registry(vec![host_abs]), &[]);
    assert_eq!(one(&mut db, "SELECT abs(-5)"), Value::Int(5));
}

#[test]
fn duplicate_signature_rejected() {
    let mut reg = ExtensionRegistry::new();
    reg.register_function(add_i64()).unwrap();
    // Same (name, arg_types) — rejected 42723 (signature-level, §4.2).
    let err = reg.register_function(add_i64()).unwrap_err();
    assert_eq!(err.code(), "42723");
    // A different signature on the same name is fine (overloading).
    reg.register_function(add_text()).unwrap();
}

#[test]
fn negative_cost_rejected() {
    let mut reg = ExtensionRegistry::new();
    let bad = HostFunction::new(
        "host_neg",
        vec![],
        ScalarType::Int64,
        Box::new(|_: &[Value]| -> jed::Result<Value> { Ok(Value::Int(0)) }),
    )
    .cost(-1);
    assert_eq!(reg.register_function(bad).unwrap_err().code(), "22023");
}

#[test]
fn declared_cost_is_charged_per_call() {
    // Two 0-arg functions identical but for their declared static weight; the query-cost difference
    // is exactly the weight difference (cost.md §6 design (a), charged once per call).
    fn const0(name: &str, cost: i64) -> HostFunction {
        HostFunction::new(
            name,
            vec![],
            ScalarType::Int64,
            Box::new(|_: &[Value]| -> jed::Result<Value> { Ok(Value::Int(0)) }),
        )
        .cost(cost)
    }
    let mut db = db_with_ext(
        registry(vec![const0("host_c0", 0), const0("host_c1000", 1000)]),
        &[],
    );
    let c0 = db.query_outcome("SELECT host_c0()", &[]).unwrap().cost();
    let c1000 = db.query_outcome("SELECT host_c1000()", &[]).unwrap().cost();
    assert_eq!(c1000 - c0, 1000);
}

#[test]
fn declared_cost_gates_max_cost_ceiling() {
    // A declared weight above the ceiling aborts 54P01 before the kernel runs (guard after charge).
    let heavy = HostFunction::new(
        "host_heavy",
        vec![],
        ScalarType::Int64,
        Box::new(|_: &[Value]| -> jed::Result<Value> { Ok(Value::Int(0)) }),
    )
    .cost(1_000_000);
    let mut db = Database::create(CreateOptions {
        extensions: registry(vec![heavy]),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.set_max_cost(1000);
    assert_eq!(
        db.query_outcome("SELECT host_heavy()", &[])
            .unwrap_err()
            .code(),
        "54P01"
    );
}

#[test]
fn wrong_result_type_is_rejected() {
    // A kernel that violates its declared RETURNS i64 (returns text) is caught (22000) rather than
    // leaking a wrong-typed value into jed's strict type system (CLAUDE.md §13).
    let liar = HostFunction::new(
        "host_liar",
        vec![],
        ScalarType::Int64,
        Box::new(|_: &[Value]| -> jed::Result<Value> { Ok(Value::Text("oops".into())) }),
    );
    let mut db = db_with_ext(registry(vec![liar]), &[]);
    assert_eq!(
        db.query_outcome("SELECT host_liar()", &[])
            .unwrap_err()
            .code(),
        "22000"
    );
}

#[test]
fn unknown_function_still_undefined() {
    let mut db = db_with_ext(registry(vec![add_i64()]), &[]);
    assert_eq!(
        db.query_outcome("SELECT host_missing(1)", &[])
            .unwrap_err()
            .code(),
        "42883"
    );
}

#[test]
fn explain_renders_host_function_name() {
    let mut db = db_with_ext(
        registry(vec![add_i64()]),
        &["CREATE TABLE t (id i32 PRIMARY KEY, a i64, b i64)"],
    );
    let rows = query(&mut db, "EXPLAIN (VERBOSE) SELECT host_add(a, b) FROM t");
    // VERBOSE renders the projection in a `output=[…]` detail column (not the first, which is the
    // node id), so scan every text cell.
    let text: String = rows
        .iter()
        .flat_map(|r| r.iter())
        .filter_map(|v| match v {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("host_add("),
        "EXPLAIN VERBOSE should render the host function name; got:\n{text}"
    );
}

#[test]
fn no_extensions_is_unaffected() {
    // The built-in-only path is untouched: an empty registry resolves nothing new, and a call to a
    // would-be host name is 42883.
    let mut db = db_with_ext(registry(vec![]), &[]);
    assert_eq!(one(&mut db, "SELECT abs(-7)"), Value::Int(7));
    assert_eq!(
        db.query_outcome("SELECT host_add(1, 2)", &[])
            .unwrap_err()
            .code(),
        "42883"
    );
}
