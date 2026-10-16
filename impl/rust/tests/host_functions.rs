//! Host-defined scalar functions (spec/design/extensibility.md §4.2 / §5.1, delivery step 3).
//! The registry/resolve/eval injection seam is a HOST-API surface the conformance corpus cannot
//! express (it registers no host code), so it is tested per core (CLAUDE.md §10 — host-API is one
//! of the sanctioned unit-test categories). These assertions must mirror the Go/TS host-function
//! tests one-for-one.

use std::sync::Arc;

use jed::value::Value;
use jed::{
    CreateOptions, Database, Environment, HostFunction, OpenOptions, Outcome, ScalarType, Session,
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

/// An environment registering `funcs`.
fn registry(funcs: Vec<HostFunction>) -> Environment {
    let mut env = Environment::builder();
    for f in funcs {
        env = env
            .register_function(f)
            .unwrap_or_else(|e| panic!("register: {}", e.message));
    }
    env.build()
}

fn db_with_ext(environment: Environment, stmts: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions {
        environment,
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
fn host_text_overload_wins_over_literal_adaptation() {
    // The built-in sha256 takes only bytea, so sha256('abc') reads the literal as bytea input — but
    // only after the call failed to resolve with the literal as text. A host sha256(text) matches
    // that first pass, so it is reached instead (functions.md §9.1).
    let host_sha = HostFunction::new(
        "sha256",
        vec![ScalarType::Text],
        ScalarType::Text,
        Box::new(|args: &[Value]| -> jed::Result<Value> {
            let Value::Text(a) = &args[0] else {
                unreachable!("text argument")
            };
            Ok(Value::Text(format!("host:{a}")))
        }),
    );
    let mut db = db_with_ext(registry(vec![host_sha]), &[]);
    assert_eq!(
        one(&mut db, "SELECT sha256('abc')"),
        Value::Text("host:abc".into())
    );
    assert_eq!(
        one(&mut db, "SELECT length(sha256(bytea 'abc'))"),
        Value::Int(32)
    );
}

#[test]
fn duplicate_signature_rejected() {
    let reg = Environment::builder().register_function(add_i64()).unwrap();
    // Same (name, arg_types) — rejected 42723 (signature-level, §4.2).
    let err = reg.clone().register_function(add_i64()).unwrap_err();
    assert_eq!(err.code(), "42723");
    // A different signature on the same name is fine (overloading).
    reg.register_function(add_text()).unwrap();
}

#[test]
fn negative_cost_rejected() {
    let reg = Environment::builder();
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
        environment: registry(vec![heavy]),
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

// ---------------------------------------------------------------------------------------------
// Delivery step 4 (extensibility.md §8.1 / §14): host scalar functions in PERSISTED INDEXES.
// An `immutable` host function carrying a `component_id` + `semantic_version` may back an
// expression / partial index; the file records the resolved dependency (format_version 31) and
// re-checks it on reopen. A missing / different-component / bumped-version function makes the index
// unusable — skipped for reads (correct heap scan), refused for writes (read-only) — never a silent
// stale-key read. These cover what the corpus cannot express (host-API registration + on-disk reopen
// with a *different* registry); they must mirror the Go/TS host-function tests one-for-one.
// ---------------------------------------------------------------------------------------------

/// `geo_hash(i64) -> i64` — the canonical index-backing host function. `component`/`version` pin its
/// identity; `Immutable` + a `component_id` are the two admission requirements (§8.1).
fn geo_hash(component: &str, version: u32) -> HostFunction {
    HostFunction::new(
        "geo_hash",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(|args: &[Value]| -> jed::Result<Value> {
            let Value::Int(a) = &args[0] else {
                unreachable!("strict + resolved i64 arg")
            };
            Ok(Value::Int(a * 10))
        }),
    )
    .volatility(Volatility::Immutable)
    .cross_core(true)
    .component_id(component.to_string())
    .semantic_version(version)
}

fn err_code(r: jed::Result<Outcome>) -> String {
    match r {
        Ok(_) => panic!("expected an error, got Ok"),
        Err(e) => e.code().to_string(),
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Remove a closed test database and the `<path>.lock/` coordination bundle opening it created
/// beside it (spec/design/locking.md).
fn remove_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir_all(path.with_extension("jed.lock"));
}

fn create_file(path: &std::path::Path, ext: Environment, stmts: &[&str]) {
    let _ = std::fs::remove_file(path);
    let mut db = Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        skip_fsync: true,
        environment: ext,
        ..Default::default()
    })
    .unwrap();
    for s in stmts {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
}

fn open_file(path: &std::path::Path, ext: Environment) -> Database {
    Database::open_with_options(
        path,
        OpenOptions {
            skip_fsync: true,
            environment: ext,
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

fn ids(db: &mut Database, sql: &str) -> Vec<Value> {
    db.query(sql, &[])
        .unwrap()
        .map(|r| r.into_iter().next().unwrap())
        .collect()
}

#[test]
fn hostfunc_volatile_in_index_rejected() {
    // The latent-bug fix: a volatile host function used to leak silently into an index expression
    // (the immutability gate was purely syntactic and did not see host functions). Now 42P17.
    let volatile = HostFunction::new(
        "geo_hash",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(|args: &[Value]| {
            let Value::Int(a) = &args[0] else {
                unreachable!()
            };
            Ok(Value::Int(a * 10))
        }),
    )
    .component_id("com.example/geo_hash"); // Volatile by default
    let mut db = db_with_ext(registry(vec![volatile]), &["CREATE TABLE t (a i64)"]);
    assert_eq!(
        err_code(db.query_outcome("CREATE INDEX ix ON t (geo_hash(a))", &[])),
        "42P17"
    );
}

#[test]
fn hostfunc_unversioned_in_index_rejected() {
    // Immutable but no component identity → cannot persist a sound dependency (42P17).
    let unversioned = HostFunction::new(
        "geo_hash",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(|args: &[Value]| {
            let Value::Int(a) = &args[0] else {
                unreachable!()
            };
            Ok(Value::Int(a * 10))
        }),
    )
    .volatility(Volatility::Immutable);
    let mut db = db_with_ext(registry(vec![unversioned]), &["CREATE TABLE t (a i64)"]);
    assert_eq!(
        err_code(db.query_outcome("CREATE INDEX ix ON t (geo_hash(a))", &[])),
        "42P17"
    );
}

#[test]
fn hostfunc_immutable_versioned_in_index_ok() {
    let mut db = db_with_ext(
        registry(vec![geo_hash("com.example/geo_hash", 1)]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
            "INSERT INTO t VALUES (1, 3), (2, 7)",
            "CREATE INDEX ix ON t (geo_hash(a))",
        ],
    );
    // geo_hash(3) = 30 → row id 1.
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE geo_hash(a) = 30"),
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn hostfunc_index_reopen_matching_ok() {
    let path = tmp("hostfunc_index_match.jed");
    create_file(
        &path,
        registry(vec![geo_hash("com.example/geo_hash", 1)]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
            "INSERT INTO t VALUES (1, 3), (2, 7)",
            "CREATE INDEX ix ON t (geo_hash(a))",
        ],
    );
    // Reopen (v31 deserialize) with the SAME component + version: the dependency matches, so reads
    // use the index and writes maintain it.
    let mut db = open_file(&path, registry(vec![geo_hash("com.example/geo_hash", 1)]));
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE geo_hash(a) = 30"),
        vec![Value::Int(1)]
    );
    db.query_outcome("INSERT INTO t VALUES (3, 3)", &[])
        .expect("a write maintaining a matching host-dep index succeeds");
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM t WHERE geo_hash(a) = 30 ORDER BY id"
        ),
        vec![Value::Int(1), Value::Int(3)]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_index_reopen_version_bump_unusable() {
    let path = tmp("hostfunc_index_bump.jed");
    create_file(
        &path,
        registry(vec![geo_hash("com.example/geo_hash", 1)]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
            "INSERT INTO t VALUES (1, 3), (2, 7)",
            "CREATE INDEX ix ON t (geo_hash(a))",
        ],
    );
    // Reopen with a BUMPED semantic_version → the index's stored keys are stale.
    let mut db = open_file(&path, registry(vec![geo_hash("com.example/geo_hash", 2)]));
    // Reads still correct: a plain read (no index) and one that COULD use the index (skipped → heap
    // scan) both return the right rows — never a silent stale-key read.
    assert_eq!(
        ids(&mut db, "SELECT id FROM t ORDER BY id"),
        vec![Value::Int(1), Value::Int(2)]
    );
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE geo_hash(a) = 30"),
        vec![Value::Int(1)]
    );
    // A write that would maintain the stale index is refused (XX002) — the table is read-only.
    assert_eq!(
        err_code(db.query_outcome("INSERT INTO t VALUES (3, 3)", &[])),
        "XX002"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_index_reopen_different_component_unusable() {
    let path = tmp("hostfunc_index_component.jed");
    create_file(
        &path,
        registry(vec![geo_hash("com.example/geo_hash", 1)]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
            "INSERT INTO t VALUES (1, 3)",
            "CREATE INDEX ix ON t (geo_hash(a))",
        ],
    );
    // Reopen with a DIFFERENT component id for the same name/signature → a different implementation.
    let mut db = open_file(&path, registry(vec![geo_hash("org.other/geo_hash", 1)]));
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE geo_hash(a) = 30"),
        vec![Value::Int(1)]
    );
    assert_eq!(
        err_code(db.query_outcome("INSERT INTO t VALUES (2, 3)", &[])),
        "XX002"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_index_reopen_missing_function() {
    let path = tmp("hostfunc_index_missing.jed");
    create_file(
        &path,
        registry(vec![geo_hash("com.example/geo_hash", 1)]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
            "INSERT INTO t VALUES (1, 3), (2, 7)",
            "CREATE INDEX ix ON t (geo_hash(a))",
        ],
    );
    // Reopen with NO extensions: the index expression can no longer resolve.
    let mut db = open_file(&path, Environment::default());
    // A read that does not reference the missing function still works (the index is simply unused).
    assert_eq!(
        ids(&mut db, "SELECT id FROM t ORDER BY id"),
        vec![Value::Int(1), Value::Int(2)]
    );
    // A write that would maintain the index needs the missing function → 42883 (resolution fails).
    assert_eq!(
        err_code(db.query_outcome("INSERT INTO t VALUES (3, 3)", &[])),
        "42883"
    );
    let _ = std::fs::remove_file(&path);
}

// ── The batched kernel ABI (extensibility.md §4.2.1) ────────────────────────────────────────────
// Batching is host-observable only (how often, and over how many rows, a kernel runs); rows, cost,
// and errors must be identical to batch-of-one. These mirror the Go/TS batch tests one-for-one.

/// Records the row count of every kernel call.
type Calls = Arc<std::sync::Mutex<Vec<usize>>>;

/// `host_twice(i64) -> i64` as a BATCH kernel: doubles each row; raises at the row whose value is
/// `fail_at` (the prefix rule — the rows before it are appended first).
fn twice_batched(calls: Calls, vol: Volatility, fail_at: Option<i64>) -> HostFunction {
    HostFunction::batched(
        "host_twice",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(
            move |args: &[Vec<Value>], out: &mut Vec<Value>| -> jed::Result<()> {
                calls.lock().unwrap().push(args[0].len());
                for v in &args[0] {
                    let Value::Int(x) = v else {
                        unreachable!("strict + resolved i64 arg")
                    };
                    if Some(*x) == fail_at {
                        return Err(jed::EngineError::new(
                            jed::SqlState::InvalidParameterValue,
                            format!("host_twice refuses {x}"),
                        ));
                    }
                    out.push(Value::Int(x * 2));
                }
                Ok(())
            },
        ),
    )
    .volatility(vol)
    .cost(5)
}

/// The same function as a SINGLE-ROW kernel — the batch-of-one reference.
fn twice_row(calls: Calls, fail_at: Option<i64>) -> HostFunction {
    HostFunction::new(
        "host_twice",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(move |args: &[Value]| -> jed::Result<Value> {
            calls.lock().unwrap().push(1);
            let Value::Int(x) = &args[0] else {
                unreachable!("strict + resolved i64 arg")
            };
            if Some(*x) == fail_at {
                return Err(jed::EngineError::new(
                    jed::SqlState::InvalidParameterValue,
                    format!("host_twice refuses {x}"),
                ));
            }
            Ok(Value::Int(x * 2))
        }),
    )
    .volatility(Volatility::Immutable)
    .cost(5)
}

/// A table `t(id i64 PRIMARY KEY, a i64)` of `n` rows `(g, g)`, with `a` NULL where `g % 10 = 0`.
fn batch_db(f: HostFunction, n: i64) -> Session {
    db_with_ext(
        registry(vec![f]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i64)",
            &format!(
                "INSERT INTO t SELECT g, CASE WHEN g % 10 = 0 THEN NULL ELSE g END \
                 FROM generate_series(1, {n}) AS g"
            ),
        ],
    )
}

fn new_calls() -> Calls {
    Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// `(rows, cost)` of a query, or its error code.
fn run(db: &mut Session, sql: &str) -> std::result::Result<(Vec<Vec<Value>>, i64), String> {
    match db.query_outcome(sql, &[]) {
        Ok(Outcome::Query { rows, cost, .. }) => Ok((rows, cost)),
        Ok(Outcome::Statement { .. }) => panic!("expected a query result for {sql:?}"),
        Err(e) => Err(format!("{} {}", e.code(), e.message)),
    }
}

#[test]
fn batch_kernel_called_once_per_chunk() {
    // 2500 rows (250 of them NULL) through the buffered projection: three chunks of ≤1024 rows, each
    // one kernel call over its non-NULL rows only (strict).
    let calls = new_calls();
    let mut db = batch_db(
        twice_batched(calls.clone(), Volatility::Immutable, None),
        2500,
    );
    let (rows, _) = run(&mut db, "SELECT id, host_twice(a) FROM t").unwrap();
    assert_eq!(rows.len(), 2500);
    for r in &rows {
        let Value::Int(id) = r[0] else { panic!() };
        let want = if id % 10 == 0 {
            Value::Null
        } else {
            Value::Int(id * 2)
        };
        assert_eq!(r[1], want);
    }
    assert_eq!(*calls.lock().unwrap(), vec![922, 922, 406]);
}

#[test]
fn batch_matches_row_kernel_rows_and_cost() {
    // The same query against a batch kernel and a single-row kernel: identical rows and cost.
    let sqls = [
        "SELECT id, host_twice(a) FROM t",
        "SELECT id, host_twice(a), host_twice(id) FROM t WHERE id > 100",
        "SELECT a, host_twice(a) FROM t ORDER BY a DESC LIMIT 7 OFFSET 3",
        "SELECT x.id, host_twice(y.a) FROM t AS x JOIN t AS y ON x.id = y.id + 1",
        "SELECT host_twice(count(*)) FROM t",
    ];
    for sql in sqls {
        let mut batched = batch_db(
            twice_batched(new_calls(), Volatility::Immutable, None),
            1500,
        );
        let mut single = batch_db(twice_row(new_calls(), None), 1500);
        assert_eq!(run(&mut batched, sql), run(&mut single, sql), "{sql}");
    }
}

#[test]
fn volatile_batch_kernel_is_called_per_row() {
    // A volatile function's call set stays exactly the scalar one: one-row batches, one per
    // non-NULL row.
    let calls = new_calls();
    let mut db = batch_db(twice_batched(calls.clone(), Volatility::Volatile, None), 30);
    run(&mut db, "SELECT host_twice(a) FROM t").unwrap();
    assert_eq!(*calls.lock().unwrap(), vec![1; 27]);
}

#[test]
fn single_row_kernel_is_never_prefetched() {
    // A single-row kernel gains nothing from a batch (the registry loops it per row), so it is called
    // exactly on the rows replay evaluates — at the buffered projection and at a pulled site alike.
    let calls = new_calls();
    let mut db = batch_db(twice_row(calls.clone(), None), 30);
    run(&mut db, "SELECT host_twice(a) FROM t").unwrap();
    run(
        &mut db,
        "SELECT host_twice(a) FROM t ORDER BY id LIMIT 5 OFFSET 3",
    )
    .unwrap();
    assert_eq!(*calls.lock().unwrap(), vec![1; 27 + 5]);
}

#[test]
fn batch_error_raises_at_the_scalar_row() {
    // The kernel fails at a = 57. Alone, that is the error; with an earlier per-row error in the
    // same projection (division by zero at id = 40) the earlier row still wins, exactly as the
    // single-row kernel orders them.
    for sql in [
        "SELECT host_twice(a) FROM t",
        "SELECT 1 / (id - 40), host_twice(a) FROM t",
        "SELECT host_twice(a), 1 / (id - 40) FROM t",
        "SELECT host_twice(a), 1 / (id - 60) FROM t",
    ] {
        let mut batched = batch_db(
            twice_batched(new_calls(), Volatility::Immutable, Some(57)),
            200,
        );
        let mut single = batch_db(twice_row(new_calls(), Some(57)), 200);
        let got = run(&mut batched, sql);
        assert!(got.is_err(), "{sql}");
        assert_eq!(got, run(&mut single, sql), "{sql}");
    }
}

#[test]
fn batch_cost_abort_matches_row_kernel_and_bounds_speculation() {
    // Under a ceiling the abort is the single-row kernel's (same code, same accrued cost), and the
    // prefetch ran the kernel over no more rows than the budget could pay for (§4.2.1).
    let calls = new_calls();
    let mut batched = batch_db(
        twice_batched(calls.clone(), Volatility::Immutable, None),
        2000,
    );
    let mut single = batch_db(twice_row(new_calls(), None), 2000);
    let budget = 200;
    batched.set_max_cost(budget);
    single.set_max_cost(budget);
    let sql = "SELECT id, host_twice(a) FROM t";
    let b = batched.query_outcome(sql, &[]).unwrap_err();
    let s = single.query_outcome(sql, &[]).unwrap_err();
    assert_eq!(b.code(), "54P01");
    assert_eq!((b.code(), &b.message), (s.code(), &s.message));
    let speculated: usize = calls.lock().unwrap().iter().sum();
    assert!(
        speculated <= (budget / 5) as usize + 1,
        "kernel ran over {speculated} rows"
    );
}

#[test]
fn batch_through_streaming_cursor_matches() {
    // The lazy query() cursor drives the same buffered projection: same rows, chunked calls.
    let calls = new_calls();
    let mut db = batch_db(
        twice_batched(calls.clone(), Volatility::Immutable, None),
        1100,
    );
    let rows: Vec<Vec<Value>> = db
        .query("SELECT host_twice(a) FROM t", &[])
        .unwrap()
        .collect();
    assert_eq!(rows.len(), 1100);
    assert_eq!(rows[0], vec![Value::Int(2)]);
    assert_eq!(rows[9], vec![Value::Null]);
    assert_eq!(*calls.lock().unwrap(), vec![922, 68]);
}

#[test]
fn batch_shape_violations_are_22000() {
    // Too few results on success, and too many, are caught rather than misattributed.
    fn shaped(extra: bool) -> HostFunction {
        HostFunction::batched(
            "host_shape",
            vec![ScalarType::Int64],
            ScalarType::Int64,
            Box::new(
                move |args: &[Vec<Value>], out: &mut Vec<Value>| -> jed::Result<()> {
                    let n = args[0].len();
                    let want = if extra { n + 1 } else { n - 1 };
                    out.extend((0..want).map(|_| Value::Int(0)));
                    Ok(())
                },
            ),
        )
        .volatility(Volatility::Immutable)
    }
    for extra in [false, true] {
        let mut db = batch_db(shaped(extra), 20);
        let err = db
            .query_outcome("SELECT host_shape(a) FROM t", &[])
            .unwrap_err();
        assert_eq!(err.code(), "22000", "extra={extra}");
    }
    // A wrong-typed result in a batch is caught at its own row too.
    let liar = HostFunction::batched(
        "host_liar",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(
            |args: &[Vec<Value>], out: &mut Vec<Value>| -> jed::Result<()> {
                out.extend(args[0].iter().map(|_| Value::Text("oops".into())));
                Ok(())
            },
        ),
    )
    .volatility(Volatility::Immutable);
    let mut db = batch_db(liar, 20);
    assert_eq!(
        db.query_outcome("SELECT host_liar(a) FROM t", &[])
            .unwrap_err()
            .code(),
        "22000"
    );
}

#[test]
fn batch_kernel_serves_a_lone_call() {
    // Outside a prefetching site (a FROM-less SELECT, a WHERE filter) a batch kernel is handed a
    // one-row batch.
    let calls = new_calls();
    let mut db = batch_db(twice_batched(calls.clone(), Volatility::Immutable, None), 5);
    assert_eq!(one(&mut db, "SELECT host_twice(21)"), Value::Int(42));
    let (rows, _) = run(&mut db, "SELECT id FROM t WHERE host_twice(a) = 6").unwrap();
    assert_eq!(rows, vec![vec![Value::Int(3)]]);
    assert_eq!(*calls.lock().unwrap(), vec![1; 6]);
}

#[test]
fn zero_arg_batch_kernel_is_never_prefetched() {
    // A zero-argument batch kernel receives no columns, so it cannot learn a batch's row count;
    // the engine therefore never prefetches a zero-argument call and always hands it a one-row
    // batch (§4.2.1). A kernel that appends one result per call is correct.
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = calls.clone();
    let seven = HostFunction::batched(
        "host_seven",
        vec![],
        ScalarType::Int64,
        Box::new(
            move |args: &[Vec<Value>], out: &mut Vec<Value>| -> jed::Result<()> {
                assert!(args.is_empty());
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                out.push(Value::Int(7));
                Ok(())
            },
        ),
    )
    .volatility(Volatility::Immutable);
    let mut db = batch_db(seven, 30);
    let (rows, _) = run(&mut db, "SELECT id, host_seven() FROM t").unwrap();
    assert_eq!(rows.len(), 30);
    assert!(rows.iter().all(|r| r[1] == Value::Int(7)));
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 30);
}

/// `name(i64) -> result`: `f` of its argument, unchecked — so the SQL picks the returned integer.
fn narrow(name: &str, result: ScalarType, f: fn(i64) -> i64) -> HostFunction {
    HostFunction::new(
        name,
        vec![ScalarType::Int64],
        result,
        Box::new(move |args: &[Value]| -> jed::Result<Value> {
            let Value::Int(x) = &args[0] else {
                unreachable!("strict + resolved i64 arg")
            };
            Ok(Value::Int(f(*x)))
        }),
    )
    .volatility(Volatility::Immutable)
}

#[test]
fn out_of_range_integer_result_is_22003() {
    // `Value::Int` carries every integer width, so a kernel declared `-> i32` can hand back 2^40: the
    // kind matches, but the value does not fit. It is trapped 22003 at the call (spec/design/
    // extensibility.md §4.2) — before the fix it surfaced as an "i32" 1099511627776, compared as one,
    // and CAST to i64 silently kept it; only a column write's own range check caught it.
    let mut db = db_with_ext(
        registry(vec![
            narrow("host_i16", ScalarType::Int16, |x| x),
            narrow("host_i32", ScalarType::Int32, |x| x),
            narrow("host_i64", ScalarType::Int64, |x| x),
        ]),
        &["CREATE TABLE c (id i32 PRIMARY KEY, v i32)"],
    );
    let code = |db: &mut Session, sql: &str| db.query_outcome(sql, &[]).unwrap_err().code();
    // The declared width's boundaries are accepted; one past either end is 22003.
    assert_eq!(one(&mut db, "SELECT host_i16(32767)"), Value::Int(32767));
    assert_eq!(one(&mut db, "SELECT host_i16(-32768)"), Value::Int(-32768));
    assert_eq!(code(&mut db, "SELECT host_i16(32768)"), "22003");
    assert_eq!(code(&mut db, "SELECT host_i16(-32769)"), "22003");
    assert_eq!(
        one(&mut db, "SELECT host_i32(2147483647)"),
        Value::Int(2147483647)
    );
    assert_eq!(
        one(&mut db, "SELECT host_i32(-2147483648)"),
        Value::Int(-2147483648)
    );
    assert_eq!(code(&mut db, "SELECT host_i32(2147483648)"), "22003");
    assert_eq!(code(&mut db, "SELECT host_i32(-2147483649)"), "22003");
    assert_eq!(
        one(&mut db, "SELECT host_i64(9223372036854775807)"),
        Value::Int(i64::MAX)
    );
    // Formerly leaked: a comparison and a widening cast observed the out-of-range value.
    assert_eq!(code(&mut db, "SELECT host_i32(1099511627776) > 5"), "22003");
    assert_eq!(
        code(&mut db, "SELECT CAST(host_i32(1099511627776) AS i64)"),
        "22003"
    );
    // The error names the declared type (the built-in overflow shape).
    let err = db
        .query_outcome("SELECT host_i32(1099511627776)", &[])
        .unwrap_err();
    assert_eq!(err.message, "value out of range for type i32");
    // A write of it is the same 22003, and nothing is stored.
    assert_eq!(
        code(&mut db, "INSERT INTO c SELECT 1, host_i32(1099511627776)"),
        "22003"
    );
    assert!(query(&mut db, "SELECT id FROM c").is_empty());
}

/// `host_scale(i64) -> i32` as a BATCH kernel returning `x * 100_000_000` — in range for x <= 21,
/// out of range from x = 22. The kernel itself never fails; the engine's range check does. Its
/// batch-of-one reference is `narrow("host_scale", Int32, |x| x * 100_000_000)`.
fn scale_batched() -> HostFunction {
    HostFunction::batched(
        "host_scale",
        vec![ScalarType::Int64],
        ScalarType::Int32,
        Box::new(
            |args: &[Vec<Value>], out: &mut Vec<Value>| -> jed::Result<()> {
                for v in &args[0] {
                    let Value::Int(x) = v else {
                        unreachable!("strict + resolved i64 arg")
                    };
                    out.push(Value::Int(x * 100_000_000));
                }
                Ok(())
            },
        ),
    )
    .volatility(Volatility::Immutable)
}

#[test]
fn batch_out_of_range_raises_at_the_scalar_row() {
    // A prefetched batch result is range-checked at replay, row by row, so the 22003 at a = 22
    // orders against other per-row errors exactly as the single-row kernel's does (§4.2.1): an
    // earlier division by zero (id = 10) wins, a later one (id = 40) does not.
    for (sql, want) in [
        ("SELECT host_scale(a) FROM t", "22003"),
        ("SELECT 1 / (id - 10), host_scale(a) FROM t", "22012"),
        ("SELECT host_scale(a), 1 / (id - 10) FROM t", "22012"),
        ("SELECT host_scale(a), 1 / (id - 40) FROM t", "22003"),
        ("SELECT 1 / (id - 40), host_scale(a) FROM t", "22003"),
    ] {
        let mut batched = batch_db(scale_batched(), 200);
        let mut single = batch_db(
            narrow("host_scale", ScalarType::Int32, |x| x * 100_000_000),
            200,
        );
        let got = run(&mut batched, sql);
        assert_eq!(got.as_ref().unwrap_err()[..5], *want, "{sql}");
        assert_eq!(got, run(&mut single, sql), "{sql}");
    }
}

// ── Pulled sites: the read-ahead window (extensibility.md §4.2.1 "In-hand and pulled sites") ───────

/// `(rows, cost)` of a query, or its error code + message — either streamed at top level through the
/// lazy cursor, or `materialized` as a CTE body (`WITH c AS (…) SELECT * FROM c`), which runs it through
/// the eager drive.
fn run_via(
    db: &mut Session,
    sql: &str,
    materialized: bool,
) -> std::result::Result<(Vec<Vec<Value>>, i64), String> {
    if materialized {
        run(db, &format!("WITH c AS ({sql}) SELECT * FROM c"))
    } else {
        run(db, sql)
    }
}

/// The kernel call sizes a full streaming scan of `batch_db(_, n)` makes: one call per 64-row read-ahead
/// window, over that window's non-NULL rows (every 10th `a` is NULL).
fn window_calls(n: i64) -> Vec<usize> {
    (0..(n + 63) / 64)
        .map(|w| {
            let ids = w * 64 + 1..=(w * 64 + 64).min(n);
            ids.filter(|id| id % 10 != 0).count()
        })
        .collect()
}

#[test]
fn batch_streaming_scan_reads_ahead_one_window_per_chunk() {
    // A full scan in primary-key order streams; with a host call in the projection it reads 64 rows
    // ahead and the kernel runs once per window over its non-NULL rows — through both drives.
    for materialized in [false, true] {
        let calls = new_calls();
        let mut db = batch_db(
            twice_batched(calls.clone(), Volatility::Immutable, None),
            2500,
        );
        let sql = "SELECT id, host_twice(a) FROM t ORDER BY id";
        let (rows, _) = run_via(&mut db, sql, materialized).unwrap();
        assert_eq!(rows.len(), 2500);
        assert_eq!(rows[6], vec![Value::Int(7), Value::Int(14)]);
        assert_eq!(rows[9], vec![Value::Int(10), Value::Null]);
        assert_eq!(
            *calls.lock().unwrap(),
            window_calls(2500),
            "materialized={materialized}"
        );
    }
}

#[test]
fn batch_streaming_scan_batches_the_where_filter() {
    // The WHERE predicate's call (beneath a comparison) prefetches over every pulled row of the
    // window; the projection's call prefetches from the first row that reaches it.
    for materialized in [false, true] {
        let calls = new_calls();
        let mut db = batch_db(
            twice_batched(calls.clone(), Volatility::Immutable, None),
            2500,
        );
        let sql = "SELECT id FROM t WHERE host_twice(a) > 4990 ORDER BY id";
        let (rows, _) = run_via(&mut db, sql, materialized).unwrap();
        assert_eq!(rows.len(), 4); // a = 2496..2499 (2500 is NULL)
        assert_eq!(*calls.lock().unwrap(), window_calls(2500));
    }
}

#[test]
fn batch_streaming_window_is_bounded_by_limit() {
    // LIMIT 5 OFFSET 3 can emit at most 8 rows, so the window is 8 rows and the projection's chunk
    // starts at the first emitted row; LIMIT 1 degenerates to batch-of-one.
    for materialized in [false, true] {
        let calls = new_calls();
        let mut db = batch_db(
            twice_batched(calls.clone(), Volatility::Immutable, None),
            2500,
        );
        let sql = "SELECT host_twice(a) FROM t ORDER BY id LIMIT 5 OFFSET 3";
        run_via(&mut db, sql, materialized).unwrap();
        let sql = "SELECT host_twice(a) FROM t ORDER BY id LIMIT 1";
        run_via(&mut db, sql, materialized).unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![5, 1]);
    }
}

#[test]
fn batch_streaming_sort_reads_ahead() {
    // A single-table ORDER BY a non-key column streams out of the sorter; its projection reads the
    // sorted rows ahead one 64-row window at a time (250 NULLs sort first under DESC, so the fourth
    // window holds the first 6 values; then 35 full windows and a 4-row tail).
    for materialized in [false, true] {
        let calls = new_calls();
        let mut db = batch_db(
            twice_batched(calls.clone(), Volatility::Immutable, None),
            2500,
        );
        let sql = "SELECT a, host_twice(a) FROM t ORDER BY a DESC";
        let (rows, _) = run_via(&mut db, sql, materialized).unwrap();
        assert_eq!(rows[250], vec![Value::Int(2499), Value::Int(4998)]);
        let mut want = vec![6];
        want.extend([64; 35]);
        want.push(4);
        assert_eq!(*calls.lock().unwrap(), want, "materialized={materialized}");
    }
}

#[test]
fn batch_pulled_sites_match_row_kernel_rows_and_cost() {
    // Rows and cost are identical to the single-row kernel at every pulled site and shape, through
    // both the materialized and the lazy drive.
    let sqls = [
        "SELECT id, host_twice(a) FROM t ORDER BY id",
        "SELECT id, host_twice(a) + 1, -host_twice(id) FROM t WHERE host_twice(a) > 100 ORDER BY id DESC",
        "SELECT id, host_twice(a) FROM t WHERE id BETWEEN 100 AND 1300 ORDER BY id",
        "SELECT id, host_twice(a) FROM t WHERE id IN (5, 50, 500, 1400) ORDER BY id",
        "SELECT DISTINCT id, host_twice(a) FROM t ORDER BY id",
        "SELECT host_twice(a) FROM t ORDER BY id LIMIT 7 OFFSET 1000",
        "SELECT id FROM t WHERE host_twice(a) IS NULL ORDER BY id",
        "SELECT a, host_twice(a), host_twice(id) > 50 FROM t ORDER BY a DESC",
        "SELECT a, host_twice(a) FROM t ORDER BY a LIMIT 9 OFFSET 1100",
    ];
    for sql in sqls {
        for materialized in [false, true] {
            let mut batched = batch_db(
                twice_batched(new_calls(), Volatility::Immutable, None),
                1500,
            );
            let mut single = batch_db(twice_row(new_calls(), None), 1500);
            let (b, s) = (
                run_via(&mut batched, sql, materialized),
                run_via(&mut single, sql, materialized),
            );
            assert!(b.is_ok(), "{sql}: {b:?}");
            assert_eq!(b, s, "{sql} materialized={materialized}");
        }
    }
}

#[test]
fn batch_pulled_sites_raise_at_the_scalar_row() {
    // The kernel fails at a = 57; an earlier per-row error (division by zero at id = 40, in the
    // projection or the filter) still wins, exactly as the single-row kernel orders them.
    let sqls = [
        "SELECT host_twice(a) FROM t ORDER BY id",
        "SELECT id FROM t WHERE host_twice(a) > 0 ORDER BY id",
        "SELECT 1 / (id - 40), host_twice(a) FROM t ORDER BY id",
        "SELECT host_twice(a) FROM t WHERE 1 / (id - 40) > 0 ORDER BY id",
        "SELECT host_twice(a) FROM t WHERE host_twice(id) / (id - 60) > -10 ORDER BY id",
        "SELECT a, host_twice(a) FROM t ORDER BY a",
    ];
    for sql in sqls {
        for materialized in [false, true] {
            let mut batched = batch_db(
                twice_batched(new_calls(), Volatility::Immutable, Some(57)),
                200,
            );
            let mut single = batch_db(twice_row(new_calls(), Some(57)), 200);
            let (b, s) = (
                run_via(&mut batched, sql, materialized),
                run_via(&mut single, sql, materialized),
            );
            assert!(b.is_err(), "{sql}");
            assert_eq!(b, s, "{sql} materialized={materialized}");
        }
    }
}

#[test]
fn batch_error_on_an_unreached_row_starts_a_new_chunk() {
    // The kernel fails at a = 57, but the WHERE rejects that row, so replay never evaluates the call
    // there: the speculative error is dropped and the next row starts a new chunk.
    for materialized in [false, true] {
        let calls = new_calls();
        let mut batched = batch_db(
            twice_batched(calls.clone(), Volatility::Immutable, Some(57)),
            2500,
        );
        let mut single = batch_db(twice_row(new_calls(), Some(57)), 2500);
        let sql = "SELECT id, host_twice(a) FROM t WHERE id % 1000 <> 57 ORDER BY id";
        let got = run_via(&mut batched, sql, materialized);
        assert!(got.is_ok());
        assert_eq!(got, run_via(&mut single, sql, materialized));
        // Window 1 (rows 1-64): the chunk from row 1 fails at 57; the chunk from row 58 covers the
        // rest of the window (58, 59, 61..64). Every later window is one call.
        let mut want = window_calls(2500);
        want.insert(1, 6);
        assert_eq!(*calls.lock().unwrap(), want);
    }
}

#[test]
fn batch_pulled_cost_abort_matches_row_kernel() {
    // Under a ceiling the abort row is the single-row kernel's — in the projection and in the
    // filter, through both drives — and the kernel ran over no more rows than the budget could pay.
    for sql in [
        "SELECT id, host_twice(a) FROM t ORDER BY id",
        "SELECT id FROM t WHERE host_twice(a) > 0 ORDER BY id",
        "SELECT a, host_twice(a) FROM t ORDER BY a",
    ] {
        for materialized in [false, true] {
            let calls = new_calls();
            let mut batched = batch_db(
                twice_batched(calls.clone(), Volatility::Immutable, None),
                2000,
            );
            let mut single = batch_db(twice_row(new_calls(), None), 2000);
            let budget = 300;
            batched.set_max_cost(budget);
            single.set_max_cost(budget);
            let (b, s) = (
                run_via(&mut batched, sql, materialized),
                run_via(&mut single, sql, materialized),
            );
            let b = b.unwrap_err();
            assert!(b.starts_with("54P01"), "{sql}: {b}");
            assert_eq!(b, s.unwrap_err(), "{sql} materialized={materialized}");
            let speculated: usize = calls.lock().unwrap().iter().sum();
            assert!(
                speculated <= (budget / 5) as usize + 1,
                "{sql}: kernel ran over {speculated} rows"
            );
        }
    }
}

#[test]
fn batch_streaming_scan_storage_errors_match_row_kernel() {
    // A corrupted page must surface exactly as under the single-row kernel — a read-ahead that meets
    // it defers the error to the row the per-row pull would have met it at, so the kernel's own error
    // at a = 57 still wins when it comes first, and a LIMIT that stops first never sees it. Corrupt
    // each body page of a multi-leaf file in turn.
    const PAGE: u32 = 256;
    let dir = std::env::temp_dir();
    let path = dir.join("jed_host_batch_defer_seed.jed");
    let cpath = dir.join("jed_host_batch_defer_corrupt.jed");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(path.clone()),
            skip_fsync: true,
            page_size: PAGE,
            ..Default::default()
        })
        .unwrap()
        .session(SessionOptions::default());
        db.query_outcome("CREATE TABLE t (id i64 PRIMARY KEY, a i64)", &[])
            .unwrap();
        db.query_outcome(
            "INSERT INTO t SELECT g, CASE WHEN g % 10 = 0 THEN NULL ELSE g END \
             FROM generate_series(1, 300) AS g",
            &[],
        )
        .unwrap();
        let image = db.to_image(PAGE, db.txid()).unwrap();
        drop(db);
        std::fs::write(&path, image).unwrap();
    }
    // Separate copies: each handle locks its own file.
    let spath = dir.join("jed_host_batch_defer_single.jed");
    let open = |p: &std::path::Path, f: HostFunction| {
        Database::open_with_options(
            p,
            OpenOptions {
                skip_fsync: true,
                environment: registry(vec![f]),
                ..OpenOptions::default()
            },
        )
        .map(|d| d.session(SessionOptions::default()))
    };
    let clean = std::fs::read(&path).unwrap();
    let ps = PAGE as usize;
    let mut detected = 0;
    for i in 2..clean.len() / ps {
        let mut bytes = clean.clone();
        bytes[i * ps + 16] ^= 0xFF;
        std::fs::write(&cpath, &bytes).unwrap();
        std::fs::write(&spath, &bytes).unwrap();
        let (Ok(mut batched), Ok(mut single)) = (
            open(
                &cpath,
                twice_batched(new_calls(), Volatility::Immutable, Some(57)),
            ),
            open(&spath, twice_row(new_calls(), Some(57))),
        ) else {
            continue; // a corrupted routing/catalog page fails at open, before any scan
        };
        for sql in [
            "SELECT id, host_twice(a) FROM t ORDER BY id",
            "SELECT id, host_twice(a) FROM t ORDER BY id LIMIT 40",
            "SELECT id, host_twice(a) FROM t ORDER BY id LIMIT 200",
            "SELECT id, host_twice(id) FROM t ORDER BY id",
        ] {
            for materialized in [false, true] {
                let (b, s) = (
                    run_via(&mut batched, sql, materialized),
                    run_via(&mut single, sql, materialized),
                );
                if matches!(&s, Err(e) if e.starts_with("XX001")) {
                    detected += 1;
                }
                assert_eq!(b, s, "page {i}: {sql} materialized={materialized}");
            }
        }
    }
    assert!(detected > 0, "a corrupted leaf should fail a scan");
    remove_db(&path);
    remove_db(&cpath);
    remove_db(&spath);
}

// ---- Host functions in DEFAULT / CHECK (spec/design/extensibility.md §7.1, step 4b). ----

/// `geo_hash(i64) -> i64 = a * factor` under the given component/version, left VOLATILE (the
/// default): a CHECK has no volatility gate (§7.1.2), only a component-identity requirement.
fn scaled_geo(component: &str, version: u32, factor: i64) -> HostFunction {
    HostFunction::new(
        "geo_hash",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(move |args: &[Value]| -> jed::Result<Value> {
            let Value::Int(a) = &args[0] else {
                unreachable!("strict + resolved i64 arg")
            };
            Ok(Value::Int(a * factor))
        }),
    )
    .component_id(component.to_string())
    .semantic_version(version)
}

/// `geo_hash(i64) -> i64 = a * 10` with NO component identity.
fn unversioned_geo() -> HostFunction {
    HostFunction::new(
        "geo_hash",
        vec![ScalarType::Int64],
        ScalarType::Int64,
        Box::new(|args: &[Value]| -> jed::Result<Value> {
            let Value::Int(a) = &args[0] else {
                unreachable!("strict + resolved i64 arg")
            };
            Ok(Value::Int(a * 10))
        }),
    )
}

/// `lim(<ty>) -> <ty>` = the identity, under `component`, with no version (0).
fn lim(ty: ScalarType, component: Option<&str>) -> HostFunction {
    let f = HostFunction::new(
        "lim",
        vec![ty],
        ty,
        Box::new(|args: &[Value]| -> jed::Result<Value> { Ok(args[0].clone()) }),
    );
    match component {
        Some(c) => f.component_id(c.to_string()),
        None => f,
    }
}

fn want_host_code(db: &mut Database, sql: &str, code: &str) {
    match db.query_outcome(sql, &[]) {
        Ok(_) => panic!("{sql:?} = Ok, want {code}"),
        Err(e) => assert_eq!(e.code(), code, "{sql:?}: {}", e.message),
    }
}

fn want_host_ok(db: &mut Database, sql: &str) {
    db.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
}

#[test]
fn hostfunc_check_admission() {
    // An unversioned host function cannot back a CHECK in any position (42P17)...
    let mut s = db_with_ext(
        registry(vec![unversioned_geo()]),
        &["CREATE TABLE u (a i64)"],
    );
    for sql in [
        "CREATE TABLE t (a i64 CHECK (geo_hash(a) < 100))",
        "CREATE TABLE t (a i64, CHECK (geo_hash(a) < 100))",
        "ALTER TABLE u ADD CONSTRAINT c CHECK (geo_hash(a) < 100)",
    ] {
        assert_eq!(err_code(s.query_outcome(sql, &[])), "42P17", "{sql}");
    }
    // ...but a DEFAULT has no admission rule: any host function, any volatility, no identity.
    for sql in [
        "CREATE TABLE d (id i64 PRIMARY KEY, b i64 DEFAULT geo_hash(5))",
        "INSERT INTO d (id) VALUES (1)",
    ] {
        s.query_outcome(sql, &[])
            .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
    }
    assert_eq!(one(&mut s, "SELECT b FROM d"), Value::Int(50));
    // A VOLATILE but versioned host function is admitted into a CHECK and enforced.
    let mut s2 = db_with_ext(
        registry(vec![scaled_geo("com.example/geo_hash", 1, 10)]),
        &["CREATE TABLE t (a i64 CHECK (geo_hash(a) < 100))"],
    );
    s2.query_outcome("INSERT INTO t VALUES (9)", &[])
        .unwrap_or_else(|e| panic!("INSERT 9: {}", e.message));
    assert_eq!(
        err_code(s2.query_outcome("INSERT INTO t VALUES (10)", &[])),
        "23514"
    );
}

/// A file whose CHECK `small` calls geo_hash v1 (a * 10 < 100, i.e. a < 10), holding the row (1, 9).
fn create_check_file(path: &std::path::Path) {
    create_file(
        path,
        registry(vec![scaled_geo("com.example/geo_hash", 1, 10)]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i64, CONSTRAINT small CHECK (geo_hash(a) < 100))",
            "INSERT INTO t VALUES (1, 9)",
        ],
    );
}

#[test]
fn hostfunc_check_reopen_matching() {
    let path = tmp("hostfunc_check_match.jed");
    create_check_file(&path);
    let mut db = open_file(
        &path,
        registry(vec![scaled_geo("com.example/geo_hash", 1, 10)]),
    );
    want_host_ok(&mut db, "INSERT INTO t VALUES (2, 5)");
    want_host_code(&mut db, "INSERT INTO t VALUES (3, 10)", "23514");
    want_host_ok(&mut db, "UPDATE t SET a = 1 WHERE id = 2");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_check_reopen_missing() {
    let path = tmp("hostfunc_check_missing.jed");
    create_check_file(&path);
    let mut db = open_file(&path, Environment::default());
    // Writes that evaluate the check cannot resolve it (42883); reads and DELETE never evaluate it.
    assert_eq!(ids(&mut db, "SELECT id FROM t"), vec![Value::Int(1)]);
    want_host_code(&mut db, "INSERT INTO t VALUES (2, 1)", "42883");
    want_host_code(&mut db, "UPDATE t SET a = 1", "42883");
    want_host_ok(&mut db, "DELETE FROM t WHERE id = 1");
    assert_eq!(ids(&mut db, "SELECT id FROM t"), Vec::<Value>::new());
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_check_reopen_mismatch() {
    for (name, component, version) in [
        ("version", "com.example/geo_hash", 2),
        ("component", "org.other/geo_hash", 1),
    ] {
        let path = tmp(&format!("hostfunc_check_mismatch_{name}.jed"));
        create_check_file(&path);
        let mut db = open_file(&path, registry(vec![scaled_geo(component, version, 10)]));
        // Every write that evaluates the stale check is refused before mutating anything.
        want_host_code(&mut db, "INSERT INTO t VALUES (2, 1)", "XX002");
        want_host_code(&mut db, "UPDATE t SET a = 1", "XX002");
        assert_eq!(
            ids(&mut db, "SELECT a FROM t WHERE id = 1"),
            vec![Value::Int(9)],
            "{name}: a after refused UPDATE"
        );
        // Reads and DELETE never evaluate a check, so a stale one costs them nothing.
        want_host_ok(&mut db, "DELETE FROM t WHERE id = 1");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn hostfunc_check_rebuild() {
    let path = tmp("hostfunc_check_rebuild.jed");
    create_check_file(&path);
    // v2 doubles the factor: the rule becomes a < 5, which the stored row (1, 9) violates.
    let v2 = || registry(vec![scaled_geo("com.example/geo_hash", 2, 20)]);
    let rebuild =
        "ALTER TABLE t DROP CONSTRAINT small, ADD CONSTRAINT small CHECK (geo_hash(a) < 100)";
    let mut db = open_file(&path, v2());
    // The one-statement rebuild re-validates every row with the current implementation: 23514, and
    // the statement rolls back whole, so the stale check is still in place.
    want_host_code(&mut db, rebuild, "23514");
    want_host_code(&mut db, "INSERT INTO t VALUES (2, 1)", "XX002");
    want_host_ok(&mut db, "DELETE FROM t WHERE id = 1");
    want_host_ok(&mut db, rebuild);
    want_host_ok(&mut db, "INSERT INTO t VALUES (2, 1)");
    want_host_code(&mut db, "INSERT INTO t VALUES (3, 9)", "23514");
    drop(db);
    // The rebuild persisted the v2 dependency: v2 writes, v1 is now the stale one.
    let mut db = open_file(&path, v2());
    want_host_ok(&mut db, "INSERT INTO t VALUES (4, 2)");
    drop(db);
    let mut db = open_file(
        &path,
        registry(vec![scaled_geo("com.example/geo_hash", 1, 10)]),
    );
    want_host_code(&mut db, "INSERT INTO t VALUES (5, 2)", "XX002");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_check_unrelated_statements() {
    let path = tmp("hostfunc_check_unrelated.jed");
    create_file(
        &path,
        registry(vec![scaled_geo("com.example/geo_hash", 1, 10)]),
        &[
            "CREATE TABLE p1 (id i64 PRIMARY KEY)",
            "CREATE TABLE p2 (id i64 PRIMARY KEY)",
            "CREATE TABLE c (id i64 PRIMARY KEY, pid i64 REFERENCES p1 ON DELETE SET NULL, a i64, CHECK (geo_hash(a) < 100))",
            "CREATE TABLE k (id i64 PRIMARY KEY, pid i64 REFERENCES p2 ON DELETE CASCADE, a i64, CHECK (geo_hash(a) < 100))",
            "INSERT INTO p1 VALUES (1)",
            "INSERT INTO p2 VALUES (1)",
            "INSERT INTO c VALUES (1, 1, 1)",
            "INSERT INTO k VALUES (1, 1, 1)",
        ],
    );
    let mut db = open_file(
        &path,
        registry(vec![scaled_geo("com.example/geo_hash", 2, 10)]),
    );
    // Adding an unrelated constraint validates only the new one, so the stale check does not block it.
    want_host_ok(&mut db, "ALTER TABLE c ADD CONSTRAINT pos CHECK (a > -100)");
    // An ON DELETE SET NULL action UPDATEs the child, evaluating its stale check: refused, nothing
    // changes.
    want_host_code(&mut db, "DELETE FROM p1", "XX002");
    assert_eq!(ids(&mut db, "SELECT id FROM p1"), vec![Value::Int(1)]);
    // An ON DELETE CASCADE action only deletes child rows, so it never evaluates the check.
    want_host_ok(&mut db, "DELETE FROM p2");
    assert_eq!(ids(&mut db, "SELECT id FROM k"), Vec::<Value>::new());
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_check_alter_type_recollects() {
    let lim32 = || lim(ScalarType::Int32, Some("a/lim"));
    let lim64 = || lim(ScalarType::Int64, Some("b/lim"));
    let path = tmp("hostfunc_check_retype.jed");
    create_file(
        &path,
        registry(vec![lim32(), lim64()]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i32, CHECK (lim(a) < 100))",
            "INSERT INTO t VALUES (1, 5)",
        ],
    );
    // The check depends on lim(i32), so a registry without it cannot write.
    let mut db = open_file(&path, registry(vec![lim64()]));
    assert!(
        db.query_outcome("INSERT INTO t VALUES (2, 6)", &[])
            .is_err(),
        "a write without the lim(i32) dependency should be refused"
    );
    drop(db);
    // ALTER COLUMN TYPE re-validates every row and re-collects the dependency from the new
    // resolution.
    let mut db = open_file(&path, registry(vec![lim32(), lim64()]));
    want_host_ok(&mut db, "ALTER TABLE t ALTER COLUMN a TYPE i64");
    drop(db);
    let mut db = open_file(&path, registry(vec![lim64()]));
    want_host_ok(&mut db, "INSERT INTO t VALUES (2, 6)");
    want_host_code(&mut db, "INSERT INTO t VALUES (3, 100)", "23514");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_default_resolved_only_when_applied() {
    let path = tmp("hostfunc_default_lazy.jed");
    create_file(
        &path,
        registry(vec![unversioned_geo()]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, b i64 DEFAULT geo_hash(5), c i64)",
            "INSERT INTO t (id) VALUES (1)",
            "CREATE TABLE p (id i64 PRIMARY KEY)",
            "INSERT INTO p VALUES (0), (1)",
            "CREATE TABLE ch (id i64 PRIMARY KEY, pid i64 DEFAULT geo_hash(0) REFERENCES p ON DELETE SET DEFAULT)",
            "INSERT INTO ch VALUES (1, 1)",
        ],
    );
    let mut db = open_file(&path, Environment::default());
    // Statements that never apply the default do not need its host function.
    want_host_ok(&mut db, "INSERT INTO t VALUES (2, 7, 7)");
    want_host_ok(&mut db, "INSERT INTO t (id, b) VALUES (3, 1)");
    want_host_ok(
        &mut db,
        "INSERT INTO t (id, b) VALUES (3, 1) ON CONFLICT DO NOTHING",
    );
    want_host_ok(&mut db, "INSERT INTO t (id, b) SELECT 4, 4");
    want_host_ok(&mut db, "UPDATE t SET c = 1");
    // Deletes no parent row, so SET DEFAULT never fires.
    want_host_ok(&mut db, "DELETE FROM p WHERE id = 99");
    // Statements that would apply it cannot resolve it.
    want_host_code(&mut db, "INSERT INTO t (id) VALUES (5)", "42883");
    want_host_code(&mut db, "INSERT INTO t VALUES (5, DEFAULT, 1)", "42883");
    want_host_code(&mut db, "INSERT INTO t (id) SELECT 5", "42883");
    want_host_code(&mut db, "INSERT INTO t DEFAULT VALUES", "42883");
    want_host_code(&mut db, "UPDATE t SET b = DEFAULT", "42883");
    // Deleting a parent row fires its SET DEFAULT action, whose child UPDATE resolves the default per
    // statement — whether or not a child row matches (here one does).
    want_host_code(&mut db, "DELETE FROM p WHERE id = 1", "42883");
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn hostfunc_check_error_precedes_default() {
    let path = tmp("hostfunc_check_default_order.jed");
    create_file(
        &path,
        registry(vec![
            scaled_geo("com.example/geo_hash", 1, 10),
            lim(ScalarType::Int64, None),
        ]),
        &[
            "CREATE TABLE t (id i64 PRIMARY KEY, a i64, b i64 DEFAULT lim(1), CHECK (geo_hash(a) < 100))",
        ],
    );
    // A stale check (XX002) and a missing default function (42883): the check resolves first.
    let mut db = open_file(
        &path,
        registry(vec![scaled_geo("com.example/geo_hash", 2, 10)]),
    );
    want_host_code(&mut db, "INSERT INTO t (id, a) VALUES (1, 1)", "XX002");
    drop(db);
    let _ = std::fs::remove_file(&path);
}
