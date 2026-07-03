// Prepared-statement plan-cache contract (spec/design/api.md §2.4). A prepared statement caches its
// resolved scan plan and reuses it across executes, re-planning only when the catalog changes. The
// behavior is invisible to the conformance corpus (which drives the materialized execute path and
// never reuses a plan), so these per-core tests pin the OBSERVABLE contract through the public API:
// reuse is result/cost-identical (the regex-cost-drift guard), a DDL between executes re-plans (no
// stale plan served), and a non-cacheable plan (subquery / precompiled regex) stays correct. That the
// cache actually engages (skips planning) is proved by the point_lookup_pk benchmark, not here.

use jed::value::Value;
use jed::{CreateOptions, Database, Session, SessionOptions};

// Compile-time guard (the `static_assertions::assert_not_impl_any!` pattern): `PreparedStatement` is
// INTENTIONALLY `!Send` — its plan cache holds an `Rc<SelectPlan>` (the plan is `!Sync` via a regex
// `Cell`, so `Arc` buys nothing). A non-regression: the whole query/cursor path is already
// thread-affine (spec/design/api.md §2.4, cores.md). If `PreparedStatement` ever became `Send`, the
// second blanket impl would also apply and `some_item`'s inference would be ambiguous → this stops
// compiling.
const _: fn() = || {
    trait AmbiguousIfSend<A> {
        fn some_item() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<u8> for T {}
    fn assert_not_send<T: ?Sized>() {
        let _ = <T as AmbiguousIfSend<_>>::some_item;
    }
    assert_not_send::<jed::PreparedStatement>();
    // Database stays Send + Sync (the shared core) — a positive guard that mint-a-session per thread
    // is unaffected.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Database>();
};

fn mem() -> Session {
    Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default())
}

fn exec(s: &mut Session, sql: &str) {
    s.execute(sql, &[]).unwrap();
}

/// Run a prepared query, fully drain it, return (rows, final cost).
fn drain(
    s: &mut Session,
    stmt: &jed::PreparedStatement,
    params: &[Value],
) -> (Vec<Vec<Value>>, i64) {
    let mut rows = s.query_prepared(stmt, params).unwrap();
    let mut out = Vec::new();
    for r in &mut rows {
        out.push(r);
    }
    rows.error().unwrap();
    let cost = rows.cost();
    (out, cost)
}

fn seed_orders(s: &mut Session, n: i64) {
    exec(s, "CREATE TABLE orders (id i32 PRIMARY KEY, amount i32)");
    for i in 1..=n {
        exec(s, &format!("INSERT INTO orders VALUES ({i}, {})", i * 100));
    }
}

/// Reusing a cached point-lookup plan across executes is result- AND cost-identical (the guard that a
/// plan with per-execution mutable cost state — e.g. a precompiled regex — is never cached: that would
/// make the 2nd execute report a different cost). Params still bind per execute.
#[test]
fn point_lookup_reuse_is_cost_identical() {
    let mut s = mem();
    seed_orders(&mut s, 5);
    let stmt = s
        .prepare("SELECT id, amount FROM orders WHERE id = $1")
        .unwrap();

    let (r1, c1) = drain(&mut s, &stmt, &[Value::Int(3)]);
    assert_eq!(r1, vec![vec![Value::Int(3), Value::Int(300)]]);

    // Second execute (cache hit) — identical cost, correct rows.
    let (r2, c2) = drain(&mut s, &stmt, &[Value::Int(3)]);
    assert_eq!(r2, vec![vec![Value::Int(3), Value::Int(300)]]);
    assert_eq!(c2, c1, "reusing the cached plan must be cost-identical");

    // Different param binds against the same cached plan.
    let (r3, _) = drain(&mut s, &stmt, &[Value::Int(5)]);
    assert_eq!(r3, vec![vec![Value::Int(5), Value::Int(500)]]);

    // A no-match param.
    let (r4, _) = drain(&mut s, &stmt, &[Value::Int(999)]);
    assert!(r4.is_empty());
}

/// DROP + re-CREATE with a different shape must re-plan (the catalog generation moved), so the next
/// execute reflects the new column set — a stale cached plan would return the old shape.
#[test]
fn drop_create_invalidates() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
    exec(&mut s, "INSERT INTO t VALUES (1, 10)");
    let stmt = s.prepare("SELECT * FROM t WHERE id = $1").unwrap();

    let (r1, _) = drain(&mut s, &stmt, &[Value::Int(1)]);
    assert_eq!(r1, vec![vec![Value::Int(1), Value::Int(10)]]);

    exec(&mut s, "DROP TABLE t");
    exec(&mut s, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, c i32)");
    exec(&mut s, "INSERT INTO t VALUES (1, 10, 20)");

    let (r2, _) = drain(&mut s, &stmt, &[Value::Int(1)]);
    assert_eq!(
        r2,
        vec![vec![Value::Int(1), Value::Int(10), Value::Int(20)]],
        "a stale 2-column plan was served after DROP/CREATE"
    );
}

/// CREATE INDEX between executes invalidates the cached full-scan plan; the re-plan picks up the new
/// secondary index (cheaper cost), proving the invalidation forces a fresh plan. DROP INDEX reverses.
#[test]
fn index_ddl_invalidates() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
    for i in 1..=50 {
        exec(&mut s, &format!("INSERT INTO t VALUES ({i}, {i})"));
    }
    let stmt = s.prepare("SELECT id FROM t WHERE a = $1").unwrap();

    let (r_scan, cost_scan) = drain(&mut s, &stmt, &[Value::Int(25)]);
    assert_eq!(r_scan, vec![vec![Value::Int(25)]]);

    exec(&mut s, "CREATE INDEX t_a ON t (a)");
    let (r_idx, cost_idx) = drain(&mut s, &stmt, &[Value::Int(25)]);
    assert_eq!(r_idx, vec![vec![Value::Int(25)]]);
    assert!(
        cost_idx < cost_scan,
        "expected index lookup cheaper than full scan after CREATE INDEX: idx={cost_idx} scan={cost_scan} (cached full-scan plan served?)"
    );

    exec(&mut s, "DROP INDEX t_a");
    let (r_scan2, cost_scan2) = drain(&mut s, &stmt, &[Value::Int(25)]);
    assert_eq!(r_scan2, vec![vec![Value::Int(25)]]);
    assert!(
        cost_scan2 > cost_idx,
        "expected full scan costlier than index after DROP INDEX: scan={cost_scan2} idx={cost_idx} (stale index plan served?)"
    );
}

/// A precompiled (constant-pattern) regex is never cached — reusing its plan would under-charge the
/// 2nd+ execute (the one-shot compile flag). Re-planned each execute, so the two are cost-identical;
/// this would FAIL if the regex plan were wrongly cached.
#[test]
fn regex_plan_not_cached_no_cost_drift() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (id i32 PRIMARY KEY, note text)");
    exec(
        &mut s,
        "INSERT INTO t VALUES (1, 'abc'), (2, 'xyz'), (3, 'abd')",
    );
    let stmt = s.prepare("SELECT id FROM t WHERE note ~ 'ab'").unwrap();

    let (r1, c1) = drain(&mut s, &stmt, &[]);
    assert_eq!(r1, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
    let (_, c2) = drain(&mut s, &stmt, &[]);
    assert_eq!(
        c1, c2,
        "regex cost drifted across executes (regex plan wrongly cached?)"
    );
}

/// A plan with an uncorrelated subquery is never cached; results stay correct across executes.
#[test]
fn subquery_plan_correct_across_executes() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
    exec(&mut s, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
    let stmt = s
        .prepare("SELECT id FROM t WHERE id = (SELECT max(id) FROM t)")
        .unwrap();

    let (r1, _) = drain(&mut s, &stmt, &[]);
    assert_eq!(r1, vec![vec![Value::Int(3)]]);
    // Insert a larger id; the (uncached, re-planned + re-evaluated) subquery must reflect it.
    exec(&mut s, "INSERT INTO t VALUES (4, 40)");
    let (r2, _) = drain(&mut s, &stmt, &[]);
    assert_eq!(r2, vec![vec![Value::Int(4)]]);
}
