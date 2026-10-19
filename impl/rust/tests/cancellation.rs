//! Cooperative cancellation through the cost meter (spec/design/api.md §11.4). Per-core unit tests,
//! NOT the shared corpus: cancellation is timing-dependent (CLAUDE.md §10), so it cannot live there.
//! These pin the mechanism deterministically — the meter's `checkpoint` honors the cancel poll, and a
//! flipped [`CancellationToken`] aborts a statement with `57014 query_canceled` at the boundary, on a
//! session, and inside a transaction (the mid-scan-via-meter proof is the white-box inline test in
//! `src/shared.rs`, which can reach the private session state to bypass the boundary poll).

use std::cell::Cell;
use std::rc::Rc;

use jed::cost::{Lifetime, Meter};
use jed::{CancellationToken, CreateOptions, Database, SessionOptions};

/// The meter's `checkpoint` aborts with `57014` the instant the cancel poll is flipped, independently of
/// the cost ceilings (a zero-limit meter never aborts on cost). The token is `Arc`-shared, so flipping
/// it is visible to the meter that already holds a clone.
#[test]
fn meter_checkpoint_honors_cancel() {
    let token = CancellationToken::new();
    let lifetime = Lifetime {
        total: Rc::new(Cell::new(0)),
        limit: 0, // unlimited budget — only cancellation can abort
    };
    let m = Meter::for_session(0, lifetime, Some(token.clone()));
    assert!(m.checkpoint().is_ok(), "un-cancelled: checkpoint passes");

    token.cancel();
    assert_eq!(
        m.checkpoint().err().unwrap().code(),
        "57014",
        "cancelled: checkpoint aborts 57014"
    );
}

/// A token already cancelled at the API entry aborts with `57014` before any work — the cheap
/// boundary poll, on both the execute and the query path (the autocommit `Database` surface, which
/// runs through `Session::execute_cancelable`).
#[test]
fn cancel_before_run_aborts_at_boundary() {
    let mut db = Database::create(CreateOptions::default()).unwrap();
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    let token = CancellationToken::new();
    token.cancel();

    let err = db
        .execute_cancelable("INSERT INTO t VALUES (1)", &[], &token)
        .expect_err("a cancelled token must abort the insert");
    assert_eq!(err.code(), "57014");

    // `Rows` is not `Debug`, so match rather than `expect_err` to pull out the error.
    let err = match db.query_cancelable("SELECT id FROM t", &[], &token) {
        Err(e) => e,
        Ok(_) => panic!("a cancelled token must abort the query"),
    };
    assert_eq!(err.code(), "57014");

    // The aborted INSERT rolled back like any error — the table is untouched.
    let rows = db.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(rows.count(), 0, "the cancelled INSERT wrote nothing");
}

/// An armed-but-never-flipped token is zero-effect: the statement runs to completion and returns all
/// rows, proving the arming adds no spurious abort (the §8 cost determinism is untouched).
#[test]
fn armed_but_not_cancelled_completes() {
    let mut db = Database::create(CreateOptions::default()).unwrap();
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    for i in 1..=20 {
        db.query_outcome(&format!("INSERT INTO t VALUES ({i})"), &[])
            .unwrap();
    }

    let token = CancellationToken::new(); // never cancelled
    let rows = db
        .query_cancelable("SELECT id FROM t", &[], &token)
        .unwrap();
    assert_eq!(rows.count(), 20, "an un-cancelled query returns every row");
}

/// Inside an explicit transaction (the `Transaction::execute_cancelable` path), a cancelled token
/// aborts the statement `57014`; surfaced from the closure, it rolls the block back, committing
/// nothing. (The boundary poll returns before the executor runs, so a pre-cancelled token does not
/// itself mark the block Failed — that is the meter-driven mid-statement abort's job; here the
/// closure returns the error and `update` rolls back, exactly as for any other error.)
#[test]
fn cancel_in_transaction_rolls_back() {
    let mut db = Database::create(CreateOptions::default()).unwrap();
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    let token = CancellationToken::new();
    token.cancel();

    let mut session = db.session(SessionOptions::default());
    let res = session.update(|tx| {
        let err = tx
            .execute_cancelable("INSERT INTO t VALUES (1)", &[], &token)
            .expect_err("a cancelled token aborts the statement");
        assert_eq!(err.code(), "57014");
        Err::<(), _>(err) // surface it so update() rolls the block back
    });
    assert_eq!(res.err().unwrap().code(), "57014");

    // The whole block rolled back — nothing committed.
    let rows = db.query("SELECT id FROM t", &[]).unwrap();
    assert_eq!(
        rows.count(),
        0,
        "the cancelled transaction committed nothing"
    );
}

#[test]
fn cancellation_batches_and_cost_limits() {
    let mut db = Database::create(CreateOptions::default()).unwrap();
    for sql in [
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t SELECT i, i % 7 FROM generate_series(1, 2000) AS g(i)",
    ] {
        db.execute(sql, &[]).unwrap();
    }
    for sql in [
        "SELECT sum(v), count(*) FROM t",
        "SELECT v FROM t",
        "SELECT v, count(*) FROM t GROUP BY v",
    ] {
        let token = CancellationToken::new();
        let mut session = db.session(SessionOptions::default());
        let mut rows = session.query_cancelable(sql, &[], &token).unwrap();
        token.cancel(); // Query returned; cancellation must reach the batch scan before it does work.
        assert!(rows.next().is_none());
        assert_eq!(rows.error().unwrap_err().code(), "57014");
        assert!(
            rows.cost() > 0 && rows.cost() < 2000,
            "cancellation should account only the accessed tree prefix"
        );
        assert_eq!(session.lifetime_cost(), rows.cost());
        for limit in [0, 1, 2] {
            let mut expected = None;
            for enabled in [false, true] {
                let mut s = db.session(SessionOptions::default());
                if limit == 1 {
                    s.set_max_cost(50);
                }
                if limit == 2 {
                    s.set_lifetime_max_cost(50);
                }
                let token = CancellationToken::new();
                let mut rows = if enabled {
                    s.query_cancelable(sql, &[], &token)
                } else {
                    s.query(sql, &[])
                }
                .unwrap();
                let n = rows.by_ref().count();
                let got = (
                    n,
                    rows.cost(),
                    rows.error().err().map(|e| e.code().to_owned()),
                );
                if let Some(want) = &expected {
                    assert_eq!(&got, want);
                } else {
                    expected = Some(got);
                }
            }
        }
    }
}

#[test]
fn cancel_during_next_with_executor_barrier() {
    use jed::{Environment, HostFunction, ScalarType};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};
    let gate = Arc::new(Barrier::new(2));
    let first = AtomicBool::new(true);
    let g = gate.clone();
    let env = Environment::builder()
        .register_function(HostFunction::new(
            "cancel_gate",
            vec![ScalarType::Int32],
            ScalarType::Int32,
            Box::new(move |args| {
                if first.swap(false, Ordering::Relaxed) {
                    g.wait();
                    g.wait();
                }
                Ok(args[0].clone())
            }),
        ))
        .unwrap()
        .build();
    let mut db = Database::create(CreateOptions {
        environment: env,
        ..Default::default()
    })
    .unwrap();
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1),(2),(3)", &[]).unwrap();
    let token = CancellationToken::new();
    let mut rows = db
        .query_cancelable(
            "SELECT sum(id) FROM t WHERE cancel_gate(id) > 0",
            &[],
            &token,
        )
        .unwrap();
    let worker = std::thread::spawn(move || {
        gate.wait();
        token.cancel();
        gate.wait();
    });
    assert!(rows.next().is_none());
    assert_eq!(rows.error().unwrap_err().code(), "57014");
    worker.join().unwrap();
    assert_eq!(db.query("SELECT id FROM t", &[]).unwrap().count(), 3);
}

#[test]
fn cancellation_checkpoints_are_independent_of_cost() {
    let token = CancellationToken::new();
    let lifetime = Lifetime {
        total: Rc::new(Cell::new(0)),
        limit: 0,
    };
    let m = Meter::for_session(0, lifetime.clone(), Some(token.clone()));
    m.checkpoint().unwrap();
    token.cancel();
    for _ in 0..255 {
        m.guard().unwrap();
    }
    assert_eq!(m.checkpoint().unwrap_err().code(), "57014");
    let mut m = Meter::for_session(2, lifetime, Some(CancellationToken::new()));
    m.checkpoint().unwrap();
    m.charge(2);
    assert_eq!(m.checkpoint().unwrap_err().code(), "54P01");
}
