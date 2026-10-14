// Parameter-sensitive ("custom") plans (spec/design/estimator.md §6.1). That a bound statement plans,
// estimates, and costs exactly like its literal twin is pinned for every core by the shared corpus
// (suites/params/custom_plans.test). These per-core tests pin the plan cache through the public API:
// a value-blind statement keeps one cached plan, a sensitive one re-plans as its value moves between
// statistics classes, and a memo hit equals fresh planning.

use jed::value::Value;
use jed::{CreateOptions, Database, Session, SessionOptions};

fn mem() -> Session {
    Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default())
}

fn exec(s: &mut Session, sql: &str) {
    s.query_outcome(sql, &[]).unwrap();
}

fn rows(s: &mut Session, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    let mut r = s.query(sql, params).unwrap();
    let mut out = Vec::new();
    for row in &mut r {
        out.push(row);
    }
    r.error().unwrap();
    out
}

/// Drain a prepared query: (rows, final cost).
fn drain(
    s: &mut Session,
    stmt: &jed::PreparedStatement,
    params: &[Value],
) -> (Vec<Vec<Value>>, i64) {
    let mut r = s.query_prepared(stmt, params).unwrap();
    let mut out = Vec::new();
    for row in &mut r {
        out.push(row);
    }
    r.error().unwrap();
    let cost = r.cost();
    (out, cost)
}

/// Drain an ad-hoc query: (rows, final cost).
fn drain_sql(s: &mut Session, sql: &str, params: &[Value]) -> (Vec<Vec<Value>>, i64) {
    let mut r = s.query(sql, params).unwrap();
    let mut out = Vec::new();
    for row in &mut r {
        out.push(row);
    }
    r.error().unwrap();
    let cost = r.cost();
    (out, cost)
}

/// 1000 items: `cat` is 1 for nine rows in ten (an MCV) and unique otherwise; `tags` holds the
/// common tag `a` on every row and the rare tag `r<i>` on one. Both are indexed and analyzed.
fn seed(s: &mut Session) {
    exec(
        s,
        "CREATE TABLE items (id i32 PRIMARY KEY, cat i32, tags text[])",
    );
    exec(
        s,
        "INSERT INTO items SELECT CAST(g AS i32), \
         CAST(CASE WHEN g % 10 = 0 THEN g ELSE 1 END AS i32), \
         ARRAY['a', 'r' || CAST(g AS text)] FROM generate_series(1, 1000) AS g",
    );
    exec(s, "CREATE INDEX items_cat ON items (cat)");
    exec(s, "CREATE INDEX items_tags ON items USING gin (tags)");
    exec(s, "ANALYZE items");
}

fn cached(stmt: &jed::PreparedStatement) -> (bool, usize, *const ()) {
    let cache = stmt.cache().borrow();
    let cp = cache.as_ref().expect("expected a cache entry");
    (
        cp.sensitive,
        cp.memo.borrow().len(),
        std::rc::Rc::as_ptr(&cp.plan).cast(),
    )
}

#[test]
fn key_lookup_is_planned_once() {
    let mut s = mem();
    seed(&mut s);
    let stmt = s.prepare("SELECT cat FROM items WHERE id = $1").unwrap();
    let (r1, c1) = drain(&mut s, &stmt, &[Value::Int(5)]);
    assert_eq!(r1, vec![vec![Value::Int(1)]]);
    let (sensitive, _, plan) = cached(&stmt);
    assert!(!sensitive, "a full-key equality is value-blind");
    let (r2, c2) = drain(&mut s, &stmt, &[Value::Int(900)]);
    assert_eq!(r2, vec![vec![Value::Int(900)]]);
    assert_eq!(c2, c1);
    assert_eq!(cached(&stmt).2, plan, "the value-blind plan is reused");
    // A NULL binding takes its own custom plan (a known NULL proves no row) without disturbing the
    // cached one.
    let (r3, _) = drain(&mut s, &stmt, &[Value::Null]);
    assert!(r3.is_empty());
    assert_eq!(cached(&stmt).2, plan);
}

#[test]
fn sensitive_statement_replans_and_memoizes() {
    let mut s = mem();
    seed(&mut s);
    let stmt = s.prepare("SELECT id FROM items WHERE cat = $1").unwrap();
    let (common_rows, common_cost) = drain(&mut s, &stmt, &[Value::Int(1)]);
    assert_eq!(common_rows.len(), 900);
    let (sensitive, memo, _) = cached(&stmt);
    assert!(sensitive);
    assert_eq!(memo, 1);
    let (rare_rows, rare_cost) = drain(&mut s, &stmt, &[Value::Int(70)]);
    assert_eq!(rare_rows, vec![vec![Value::Int(70)]]);
    assert_eq!(cached(&stmt).1, 2);
    // The same costs as the literal queries, each through its own plan.
    assert_eq!(
        common_cost,
        drain_sql(&mut s, "SELECT id FROM items WHERE cat = 1", &[]).1
    );
    assert_eq!(
        rare_cost,
        drain_sql(&mut s, "SELECT id FROM items WHERE cat = 70", &[]).1
    );
    // A memo hit equals fresh planning.
    let (again_rows, again_cost) = drain(&mut s, &stmt, &[Value::Int(70)]);
    assert_eq!(again_rows, rare_rows);
    assert_eq!(again_cost, rare_cost);
    assert_eq!(cached(&stmt).1, 2, "a memo hit adds nothing");
    // The memo is bounded.
    for v in 0..20 {
        drain(&mut s, &stmt, &[Value::Int(1000 + v)]);
    }
    assert!(cached(&stmt).1 <= 8);
    // A relevant estimator-input change replaces the entry and its memo.
    exec(&mut s, "INSERT INTO items VALUES (5000, 70, ARRAY['a'])");
    let (after, _) = drain(&mut s, &stmt, &[Value::Int(70)]);
    assert_eq!(after.len(), 2);
    assert_eq!(cached(&stmt).1, 1);
}

/// The classification's soundness (estimator.md §6.1, "never the reverse"): a statement classified
/// value-blind has one plan for every non-NULL binding, so its unbound EXPLAIN equals its EXPLAIN
/// with values, estimates included.
#[test]
fn value_blind_classification_is_sound() {
    let mut s = mem();
    seed(&mut s);
    exec(
        &mut s,
        "CREATE TABLE t2 (id i32 PRIMARY KEY, v i32, w text)",
    );
    exec(
        &mut s,
        "INSERT INTO t2 SELECT CAST(g AS i32), CAST(g % 7 AS i32), CAST(g AS text) \
         FROM generate_series(1, 50) AS g",
    );
    exec(&mut s, "CREATE INDEX t2_v ON t2 (v)");
    let int = Value::Int;
    let text = |t: &str| Value::Text(t.to_string());
    let cases: Vec<(&str, Vec<Value>, Option<bool>)> = vec![
        (
            "SELECT cat FROM items WHERE id = $1",
            vec![int(5)],
            Some(false),
        ),
        (
            "SELECT id FROM items WHERE cat = $1",
            vec![int(70)],
            Some(true),
        ),
        (
            "SELECT id FROM items WHERE id > $1",
            vec![int(990)],
            Some(true),
        ),
        (
            "SELECT count(*) FROM items i JOIN items j ON i.id = j.cat WHERE i.id = $1",
            vec![int(70)],
            None,
        ),
        ("SELECT * FROM t2 WHERE v = $1", vec![int(3)], Some(false)),
        ("SELECT * FROM t2 WHERE v > $1", vec![int(3)], Some(false)),
        (
            "SELECT * FROM t2 WHERE v > $1 AND v < $2",
            vec![int(5), int(1)],
            Some(true),
        ),
        (
            "SELECT * FROM t2 WHERE w = $1 OR v = $2",
            vec![text("4"), int(2)],
            None,
        ),
        (
            "SELECT * FROM t2 WHERE id = $1 AND v = $2",
            vec![int(4), int(4)],
            None,
        ),
        (
            "SELECT * FROM t2 WHERE v = $1 OR v = $2",
            vec![int(4), int(4)],
            None,
        ),
        (
            "SELECT CAST($1 AS i32) + id FROM t2 WHERE id = 3",
            vec![int(9)],
            Some(false),
        ),
        (
            "SELECT * FROM t2 WHERE v = 2 AND $1 = TRUE",
            vec![Value::Bool(false)],
            Some(true),
        ),
        (
            "SELECT * FROM t2 WHERE v BETWEEN $1 AND $2",
            vec![int(2), int(3)],
            None,
        ),
    ];
    for (sql, params, expect) in cases {
        let stmt = s.prepare(sql).unwrap();
        drain(&mut s, &stmt, &params);
        let sensitive = cached(&stmt).0;
        if let Some(expect) = expect {
            assert_eq!(sensitive, expect, "classification of {sql}");
        }
        if !sensitive {
            let unbound = rows(&mut s, &format!("EXPLAIN {sql}"), &[]);
            let bound = rows(&mut s, &format!("EXPLAIN {sql}"), &params);
            assert_eq!(unbound, bound, "{sql} is classified value-blind");
        }
    }
}
