// Prepared-statement plan-cache contract (spec/design/api.md §2.4). A prepared statement caches its
// resolved scan plan and reuses it across executes while its exact estimator inputs remain unchanged. The
// behavior is invisible to the conformance corpus (which drives the materialized execute path and
// never reuses a plan), so these per-core tests pin the OBSERVABLE contract through the public API:
// reuse is result/cost-identical (the regex-cost-drift guard), a DDL between executes re-plans (no
// stale plan served), and a non-cacheable plan (subquery / precompiled regex) stays correct. That the
// cache actually engages (skips planning) is proved by the point_lookup_pk benchmark, not here.

use jed::value::Value;
use jed::{AttachSource, CreateOptions, Database, Session, SessionOptions};

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
    s.query_outcome(sql, &[]).unwrap();
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

/// Render the exact physical plan held by a prepared statement's cache. Public EXPLAIN performs a
/// fresh planning pass; this white-box helper compares the refilled cache with an independently
/// fresh prepared plan.
fn cached_explain(s: &Session, stmt: &jed::PreparedStatement) -> Vec<Vec<Value>> {
    let cache = stmt.cache().borrow();
    let plan = &cache.as_ref().expect("expected cached plan").plan;
    let mut render = crate::executor::ExplainRender::default();
    s.test_engine()
        .render_select_plan(&mut render, plan, 0)
        .unwrap();
    render.rows
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

/// P2 row-statistics validity is relation-scoped: unrelated writes retain the exact cached plan,
/// while a referenced relation mutation (even when its count returns to the old value) re-plans.
#[test]
fn estimator_revision_tracks_relevant_relations() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE a (id i32 PRIMARY KEY, v i32)");
    exec(&mut s, "CREATE TABLE b (id i32 PRIMARY KEY, v i32)");
    exec(&mut s, "INSERT INTO a VALUES (1, 10)");
    exec(&mut s, "INSERT INTO b VALUES (1, 10)");
    let stmt = s.prepare("SELECT id FROM a WHERE v = $1").unwrap();
    let _ = drain(&mut s, &stmt, &[Value::Int(10)]);
    let first_plan = {
        let cache = stmt.cache().borrow();
        std::rc::Rc::clone(&cache.as_ref().expect("cache filled").plan)
    };
    let first_revision = {
        let cache = stmt.cache().borrow();
        cache.as_ref().unwrap().inputs[0].revision.clone()
    };

    exec(&mut s, "INSERT INTO b VALUES (2, 20)");
    let _ = drain(&mut s, &stmt, &[Value::Int(10)]);
    {
        let cache = stmt.cache().borrow();
        assert!(std::rc::Rc::ptr_eq(
            &cache.as_ref().unwrap().plan,
            &first_plan
        ));
    }

    exec(&mut s, "INSERT INTO a VALUES (2, 20)");
    exec(&mut s, "DELETE FROM a WHERE id = 2");
    let (rows, cost) = drain(&mut s, &stmt, &[Value::Int(10)]);
    {
        let cache = stmt.cache().borrow();
        let refilled = cache.as_ref().unwrap();
        assert!(!std::rc::Rc::ptr_eq(&refilled.plan, &first_plan));
        assert!(!std::sync::Arc::ptr_eq(
            &refilled.inputs[0].revision,
            &first_revision
        ));
    }
    let fresh = s.prepare("SELECT id FROM a WHERE v = $1").unwrap();
    let (fresh_rows, fresh_cost) = drain(&mut s, &fresh, &[Value::Int(10)]);
    assert_eq!(rows, fresh_rows);
    assert_eq!(
        cost, fresh_cost,
        "refilled and fresh actual cost must match"
    );
    assert_eq!(cached_explain(&s, &stmt), cached_explain(&s, &fresh));

    // Cover each distinct row-mutation executor path. A no-op conflict remains a hit; real UPDATE,
    // INSERT ... SELECT, UPSERT-update, and DELETE statements each replace the relation revision.
    let plan = {
        let cache = stmt.cache().borrow();
        std::rc::Rc::clone(&cache.as_ref().unwrap().plan)
    };
    exec(
        &mut s,
        "INSERT INTO a VALUES (1, 99) ON CONFLICT DO NOTHING",
    );
    let _ = drain(&mut s, &stmt, &[Value::Int(10)]);
    {
        let cache = stmt.cache().borrow();
        assert!(std::rc::Rc::ptr_eq(&cache.as_ref().unwrap().plan, &plan));
    }
    for (sql, param) in [
        ("UPDATE a SET v = 11 WHERE id = 1", 11),
        ("INSERT INTO a SELECT 2, 20", 11),
        (
            "INSERT INTO a VALUES (1, 12) ON CONFLICT (id) DO UPDATE SET v = excluded.v",
            12,
        ),
        ("DELETE FROM a WHERE id = 2", 12),
    ] {
        let before = {
            let cache = stmt.cache().borrow();
            std::rc::Rc::clone(&cache.as_ref().unwrap().plan)
        };
        exec(&mut s, sql);
        let _ = drain(&mut s, &stmt, &[Value::Int(param)]);
        let cache = stmt.cache().borrow();
        assert!(
            !std::rc::Rc::ptr_eq(&cache.as_ref().unwrap().plan, &before),
            "row mutation did not invalidate: {sql}"
        );
    }
}

/// Working statistics may invalidate a committed entry for the transaction's read, but may never
/// replace it. Rollback restores the old revision and therefore the original cache hit.
#[test]
fn estimator_revision_rollback_restores_cache_hit() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    exec(&mut s, "INSERT INTO t VALUES (1, 10)");
    let stmt = s.prepare("SELECT id FROM t WHERE v = $1").unwrap();
    let _ = drain(&mut s, &stmt, &[Value::Int(10)]);
    let committed_plan = {
        let cache = stmt.cache().borrow();
        std::rc::Rc::clone(&cache.as_ref().unwrap().plan)
    };

    s.begin(true).unwrap();
    exec(&mut s, "INSERT INTO t VALUES (2, 10)");
    let (inside, _) = drain(&mut s, &stmt, &[Value::Int(10)]);
    assert_eq!(inside, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    {
        let cache = stmt.cache().borrow();
        assert!(std::rc::Rc::ptr_eq(
            &cache.as_ref().unwrap().plan,
            &committed_plan
        ));
    }
    s.rollback().unwrap();
    let (after, _) = drain(&mut s, &stmt, &[Value::Int(10)]);
    assert_eq!(after, vec![vec![Value::Int(1)]]);
    let cache = stmt.cache().borrow();
    assert!(std::rc::Rc::ptr_eq(
        &cache.as_ref().unwrap().plan,
        &committed_plan
    ));
}

/// Attachment-only plans key their owning attachment, not main's catalog/statistics state.
#[test]
fn attachment_has_independent_estimator_signature() {
    let db = Database::create(CreateOptions::default()).unwrap();
    db.attach("aux", AttachSource::memory(), false).unwrap();
    let mut s = db.session(SessionOptions::default());
    exec(&mut s, "CREATE TABLE aux.t (id i32 PRIMARY KEY, v i32)");
    exec(&mut s, "INSERT INTO aux.t VALUES (1, 10)");
    let stmt = db.prepare("SELECT id FROM aux.t WHERE v = $1").unwrap();
    let _ = drain(&mut s, &stmt, &[Value::Int(10)]);
    let first_plan = {
        let cache = stmt.cache().borrow();
        std::rc::Rc::clone(&cache.as_ref().unwrap().plan)
    };

    exec(&mut s, "CREATE TABLE local_only (id i32 PRIMARY KEY)");
    let _ = drain(&mut s, &stmt, &[Value::Int(10)]);
    {
        let cache = stmt.cache().borrow();
        assert!(std::rc::Rc::ptr_eq(
            &cache.as_ref().unwrap().plan,
            &first_plan
        ));
    }

    exec(&mut s, "INSERT INTO aux.t VALUES (2, 10)");
    let (rows, cost) = drain(&mut s, &stmt, &[Value::Int(10)]);
    {
        let cache = stmt.cache().borrow();
        assert!(!std::rc::Rc::ptr_eq(
            &cache.as_ref().unwrap().plan,
            &first_plan
        ));
    }
    let fresh = db.prepare("SELECT id FROM aux.t WHERE v = $1").unwrap();
    let (fresh_rows, fresh_cost) = drain(&mut s, &fresh, &[Value::Int(10)]);
    assert_eq!(rows, fresh_rows);
    assert_eq!(cost, fresh_cost);

    // A new attachment at the same name may have the same generation and schema; its opaque
    // database identity must still reject the old entry.
    let (old_plan, old_database) = {
        let cache = stmt.cache().borrow();
        let cached = cache.as_ref().unwrap();
        (
            std::rc::Rc::clone(&cached.plan),
            cached.inputs[0].database.clone(),
        )
    };
    db.detach("aux").unwrap();
    db.attach("aux", AttachSource::memory(), false).unwrap();
    exec(&mut s, "CREATE TABLE aux.t (id i32 PRIMARY KEY, v i32)");
    exec(&mut s, "INSERT INTO aux.t VALUES (9, 10), (10, 10)");
    let (replaced_rows, _) = drain(&mut s, &stmt, &[Value::Int(10)]);
    let cache = stmt.cache().borrow();
    let replaced = cache.as_ref().unwrap();
    assert!(!std::sync::Arc::ptr_eq(
        &replaced.inputs[0].database,
        &old_database
    ));
    assert!(!std::rc::Rc::ptr_eq(&replaced.plan, &old_plan));
    assert_eq!(
        replaced_rows,
        vec![vec![Value::Int(9)], vec![Value::Int(10)]]
    );
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

/// A statement is a standalone value: a plan filled on one session is reused by a different session
/// over the same Database (the cache is keyed on the shared core's committed catalog generation, not
/// the filling session), and reuse stays result- and cost-identical.
#[test]
fn statement_shared_across_sessions() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut a = db.session(SessionOptions::default());
    seed_orders(&mut a, 5);
    let stmt = db
        .prepare("SELECT id, amount FROM orders WHERE id = $1")
        .unwrap();

    let (ra, ca) = drain(&mut a, &stmt, &[Value::Int(3)]);
    assert_eq!(ra, vec![vec![Value::Int(3), Value::Int(300)]]);

    let mut b = db.session(SessionOptions::default());
    let (rb, cb) = drain(&mut b, &stmt, &[Value::Int(3)]);
    assert_eq!(rb, vec![vec![Value::Int(3), Value::Int(300)]]);
    assert_eq!(cb, ca, "cross-session reuse must be cost-identical");
}

/// A statement executed against a DIFFERENT Database must not falsely hit: `cat_gen` is only
/// monotonic within one core, so two databases can sit at the same generation with different schemas.
/// The entry's core identity forces a re-plan against the other database.
#[test]
fn distinct_databases_no_false_hit() {
    let db1 = Database::create(CreateOptions::default()).unwrap();
    let db2 = Database::create(CreateOptions::default()).unwrap();
    let mut s1 = db1.session(SessionOptions::default());
    let mut s2 = db2.session(SessionOptions::default());
    // One CREATE each → both cores sit at the SAME catalog generation with different table shapes.
    exec(&mut s1, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
    exec(&mut s2, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)");
    exec(&mut s1, "INSERT INTO t VALUES (1, 10)");
    exec(&mut s2, "INSERT INTO t VALUES (1, 10, 20)");

    let stmt = db1.prepare("SELECT * FROM t WHERE id = $1").unwrap();
    let (r1, _) = drain(&mut s1, &stmt, &[Value::Int(1)]);
    assert_eq!(r1, vec![vec![Value::Int(1), Value::Int(10)]]);

    // Same cat_gen, different core: a false hit would serve db1's 2-column plan against db2.
    let (r2, _) = drain(&mut s2, &stmt, &[Value::Int(1)]);
    assert_eq!(
        r2,
        vec![vec![Value::Int(1), Value::Int(10), Value::Int(20)]],
        "stale cross-database plan served?"
    );
}

/// A plan cached where a relation name is persistent must not be served on a session whose
/// session-local temp table shadows that name — the hit path re-checks the plan's relations against
/// the executing session's temp domain and re-plans.
#[test]
fn temp_shadow_replans() {
    let db = Database::create(CreateOptions::default()).unwrap();
    // Session B creates its temp table FIRST (a temp name may not shadow an existing persistent
    // table, but a later persistent CREATE in another session cannot see B's temp domain).
    let mut b = db.session(SessionOptions::default());
    exec(&mut b, "CREATE TEMP TABLE t (id i32 PRIMARY KEY, v i32)");
    exec(&mut b, "INSERT INTO t VALUES (1, 111)");

    let mut a = db.session(SessionOptions::default());
    exec(&mut a, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)");
    exec(&mut a, "INSERT INTO t VALUES (1, 10, 20)");

    let stmt = db.prepare("SELECT * FROM t WHERE id = $1").unwrap();
    let (ra, _) = drain(&mut a, &stmt, &[Value::Int(1)]);
    assert_eq!(
        ra,
        vec![vec![Value::Int(1), Value::Int(10), Value::Int(20)]]
    );

    // Session B: same core, same cat_gen — but t resolves temp-first there. The cached persistent
    // plan must not be served (and B's temp plan is never cached).
    let (rb, _) = drain(&mut b, &stmt, &[Value::Int(1)]);
    assert_eq!(
        rb,
        vec![vec![Value::Int(1), Value::Int(111)]],
        "stale persistent plan served on a temp-shadowed session?"
    );

    // Back on A the persistent plan still serves (B's run did not poison the cache).
    let (ra2, _) = drain(&mut a, &stmt, &[Value::Int(1)]);
    assert_eq!(
        ra2,
        vec![vec![Value::Int(1), Value::Int(10), Value::Int(20)]]
    );
}

/// The Transaction handle runs prepared statements too (the converged trio, api.md §2.4):
/// read-your-writes inside the block, and the same statement value works before and after.
#[test]
fn transaction_runs_prepared() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut s = db.session(SessionOptions::default());
    exec(&mut s, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    let insert = db.prepare("INSERT INTO t VALUES ($1, $2)").unwrap();
    let select = db.prepare("SELECT v FROM t WHERE id = $1").unwrap();

    s.execute_prepared(&insert, &[Value::Int(1), Value::Int(100)])
        .unwrap();
    s.update(|tx| {
        assert_eq!(
            tx.execute_prepared(&insert, &[Value::Int(2), Value::Int(200)])?,
            1
        );
        let mut rows = tx.query_prepared(&select, &[Value::Int(2)])?;
        let row = (&mut rows).next().expect("in-tx prepared read");
        assert_eq!(row, vec![Value::Int(200)]);
        rows.error()?;
        Ok(())
    })
    .unwrap();

    let (r, _) = drain(&mut s, &select, &[Value::Int(2)]);
    assert_eq!(r, vec![vec![Value::Int(200)]], "the block committed");
}
