use jed::{CreateOptions, Database, SessionOptions};

fn main() -> jed::Result<()> {
    let mut db = Database::create(CreateOptions { path: Some("app.jed".into()), ..Default::default() })?;

    // Serve untrusted queries through a session bounded TWO ways:
    //   max_cost          — a per-STATEMENT ceiling: one runaway query aborts 54P01.
    //   lifetime_max_cost — a per-SESSION budget: the session's cumulative cost is capped, so a
    //                       flood of cheap queries can't burn unbounded CPU. It aborts 54P02.
    let mut untrusted = db.session(SessionOptions {
        max_cost: 10_000,
        lifetime_max_cost: 3, // tiny, for illustration
        ..SessionOptions::default()
    });

    // Each statement accrues into the session's running total; read it with lifetime_cost().
    untrusted.execute("SELECT 1", &[])?; // cost 1 — cumulative 1
    untrusted.execute("SELECT 1", &[])?; // cost 1 — cumulative 2

    // The third drives the cumulative to the budget — the in-flight statement aborts 54P02, and the
    // partial cost still counts, so the session is now spent.
    let denied = untrusted.execute("SELECT 1", &[]);
    assert_eq!(denied.unwrap_err().code(), "54P02");
    assert_eq!(untrusted.lifetime_cost(), 3);

    // Once spent, every further statement is rejected at admission — the session is done.
    let after = untrusted.execute("SELECT 1", &[]);
    assert_eq!(after.unwrap_err().code(), "54P02");
    untrusted.close();

    Ok(())
}
