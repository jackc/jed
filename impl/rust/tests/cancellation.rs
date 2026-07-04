//! Cooperative cancellation through the cost meter (spec/design/api.md §11.4). Per-core unit tests,
//! NOT the shared corpus: cancellation is timing-dependent (CLAUDE.md §10), so it cannot live there.
//! These pin the mechanism deterministically — the meter's `guard` honors the cancel poll, and a
//! flipped [`CancellationToken`] aborts a statement with `57014 query_canceled` at the boundary, on a
//! session, and inside a transaction (the mid-scan-via-meter proof is the white-box inline test in
//! `src/shared.rs`, which can reach the private session state to bypass the boundary poll).

use std::cell::Cell;
use std::rc::Rc;

use jed::cost::{Lifetime, Meter};
use jed::{CancellationToken, CreateOptions, Database, SessionOptions};

/// The meter's `guard` aborts with `57014` the instant the cancel poll is flipped, independently of
/// the cost ceilings (a zero-limit meter never aborts on cost). The token is `Arc`-shared, so flipping
/// it is visible to the meter that already holds a clone.
#[test]
fn meter_guard_honors_cancel() {
    let token = CancellationToken::new();
    let lifetime = Lifetime {
        total: Rc::new(Cell::new(0)),
        limit: 0, // unlimited budget — only cancellation can abort
    };
    let m = Meter::for_session(0, lifetime, Some(token.clone()));
    assert!(m.guard().is_ok(), "un-cancelled: guard passes");

    token.cancel();
    assert_eq!(
        m.guard().err().unwrap().code(),
        "57014",
        "cancelled: guard aborts 57014"
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
