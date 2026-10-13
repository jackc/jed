// Parameter-sensitive ("custom") plans (spec/design/estimator.md §6.1). The shared corpus binds no
// parameters, and its literal EXPLAIN records already pin every custom plan, so these per-core tests
// pin the binding path through the public API: a statement planned with bound values estimates,
// selects, and costs exactly like the equivalent literal query; a value-blind statement keeps one
// cached plan; a sensitive one re-plans as its value moves between statistics classes; and a memo
// hit equals fresh planning.

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

/// EXPLAIN with bound values equals EXPLAIN of the literal query, row for row (the estimates and
/// the selected access path), and the prepared execution's actual cost equals the literal's.
fn assert_custom_matches_literal(
    s: &mut Session,
    param_sql: &str,
    params: &[Value],
    literal_sql: &str,
) {
    let custom = rows(s, &format!("EXPLAIN {param_sql}"), params);
    let literal = rows(s, &format!("EXPLAIN {literal_sql}"), &[]);
    assert_eq!(
        strip_params(&custom),
        strip_params(&literal),
        "custom plan of {param_sql} {params:?} differs from {literal_sql}"
    );
    let stmt = s.prepare(param_sql).unwrap();
    let (prepared_rows, prepared_cost) = drain(s, &stmt, params);
    let (literal_rows, literal_cost) = drain_sql(s, literal_sql, &[]);
    assert_eq!(prepared_rows, literal_rows);
    assert_eq!(
        prepared_cost, literal_cost,
        "actual cost of {param_sql} {params:?}"
    );
}

/// A `$N` still renders symbolically; the comparison is over everything else.
fn strip_params(plan: &[Vec<Value>]) -> Vec<Vec<Value>> {
    plan.iter()
        .map(|row| {
            let mut row = row.clone();
            row[2] = Value::Text(String::new());
            row
        })
        .collect()
}

fn access(plan: &[Vec<Value>]) -> String {
    plan.iter()
        .filter_map(|row| match &row[2] {
            Value::Text(t) if t.contains("scan") || t.contains("bound") => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn mcv_and_rare_values_take_their_literal_plans() {
    let mut s = mem();
    seed(&mut s);
    let common = rows(&mut s, "EXPLAIN SELECT id FROM items WHERE cat = 1", &[]);
    let rare = rows(&mut s, "EXPLAIN SELECT id FROM items WHERE cat = 70", &[]);
    assert_ne!(
        access(&common),
        access(&rare),
        "the data must flip the plan"
    );
    let sql = "SELECT id FROM items WHERE cat = $1";
    assert_custom_matches_literal(
        &mut s,
        sql,
        &[Value::Int(1)],
        "SELECT id FROM items WHERE cat = 1",
    );
    assert_custom_matches_literal(
        &mut s,
        sql,
        &[Value::Int(70)],
        "SELECT id FROM items WHERE cat = 70",
    );
    // A histogram range.
    assert_custom_matches_literal(
        &mut s,
        "SELECT id FROM items WHERE cat > $1",
        &[Value::Int(500)],
        "SELECT id FROM items WHERE cat > 500",
    );
}

#[test]
fn gin_term_facts_apply_to_a_bound_operand() {
    let mut s = mem();
    seed(&mut s);
    let common = rows(
        &mut s,
        "EXPLAIN SELECT id FROM items WHERE tags @> ARRAY['a']",
        &[],
    );
    let rare = rows(
        &mut s,
        "EXPLAIN SELECT id FROM items WHERE tags @> ARRAY['r7']",
        &[],
    );
    assert_ne!(common, rare);
    let tag = |t: &str| Value::Text(t.to_string());
    for sql in [
        "SELECT id FROM items WHERE tags @> ARRAY[$1]",
        "SELECT id FROM items WHERE $1 = ANY(tags)",
    ] {
        for t in ["a", "r7"] {
            let literal = sql.replace("$1", &format!("'{t}'"));
            assert_custom_matches_literal(&mut s, sql, &[tag(t)], &literal);
        }
    }
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
    assert_custom_matches_literal(
        &mut s,
        "SELECT cat FROM items WHERE id = $1",
        &[Value::Null],
        "SELECT cat FROM items WHERE id = NULL",
    );
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

#[test]
fn dml_target_scans_use_the_custom_plan() {
    let mut s = mem();
    seed(&mut s);
    for (param_sql, literal_sql) in [
        (
            "UPDATE items SET tags = NULL WHERE cat = $1",
            "UPDATE items SET tags = NULL WHERE cat = 70",
        ),
        (
            "DELETE FROM items WHERE cat = $1",
            "DELETE FROM items WHERE cat = 70",
        ),
        (
            "DELETE FROM items WHERE id IN (SELECT id FROM items WHERE cat = $1)",
            "DELETE FROM items WHERE id IN (SELECT id FROM items WHERE cat = 70)",
        ),
    ] {
        let custom = rows(&mut s, &format!("EXPLAIN {param_sql}"), &[Value::Int(70)]);
        let literal = rows(&mut s, &format!("EXPLAIN {literal_sql}"), &[]);
        assert_eq!(strip_params(&custom), strip_params(&literal), "{param_sql}");
        let custom = rows(
            &mut s,
            &format!("EXPLAIN ANALYZE {param_sql}"),
            &[Value::Int(70)],
        );
        exec(
            &mut s,
            "INSERT INTO items VALUES (70, 70, ARRAY['a', 'r70']) ON CONFLICT (id) DO UPDATE SET cat = 70, tags = ARRAY['a', 'r70']",
        );
        let literal = rows(&mut s, &format!("EXPLAIN ANALYZE {literal_sql}"), &[]);
        assert_eq!(strip_params(&custom), strip_params(&literal), "{param_sql}");
        exec(
            &mut s,
            "INSERT INTO items VALUES (70, 70, ARRAY['a', 'r70']) ON CONFLICT (id) DO UPDATE SET cat = 70, tags = ARRAY['a', 'r70']",
        );
    }
}

#[test]
fn insert_select_source_uses_the_custom_plan() {
    let mut s = mem();
    seed(&mut s);
    exec(&mut s, "CREATE TABLE picked (id i32 PRIMARY KEY)");
    let custom = rows(
        &mut s,
        "EXPLAIN ANALYZE INSERT INTO picked SELECT id FROM items WHERE cat = $1",
        &[Value::Int(70)],
    );
    exec(&mut s, "DELETE FROM picked");
    let literal = rows(
        &mut s,
        "EXPLAIN ANALYZE INSERT INTO picked SELECT id FROM items WHERE cat = 70",
        &[],
    );
    assert_eq!(strip_params(&custom), strip_params(&literal));
}

#[test]
fn generate_series_bounds_are_plan_time_literals() {
    let mut s = mem();
    assert_custom_matches_literal(
        &mut s,
        "SELECT count(*) FROM generate_series(1, $1) AS g",
        &[Value::Int(50)],
        "SELECT count(*) FROM generate_series(1, 50) AS g",
    );
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
