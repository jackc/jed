use jed::{CreateOptions, Database, SessionOptions};

fn main() -> jed::Result<()> {
    let mut db = Database::create(CreateOptions { path: Some("app.jed".into()), ..Default::default() })?;

    // Session variables are PostgreSQL's GUC model — they live on a SESSION, so mint one from the
    // database rather than using the bare handle. A custom variable must be NAMESPACED — a dotted name
    // like `myapp.tenant`; a non-dotted name is 42704.
    let mut s = db.session(SessionOptions::default());
    s.set_var("myapp.tenant", "acme")?;

    // Read it back through the host API — the name is case-insensitive; an unset name is None.
    assert_eq!(s.var("myapp.tenant"), Some("acme".to_string()));

    // ... or in SQL with current_setting(): `SELECT current_setting('myapp.tenant')` -> "acme".
    let _rows = s.query("SELECT current_setting('myapp.tenant')", &[])?;

    // An unset name is 42704, unless the two-arg form passes missing_ok = true, which returns NULL:
    //   SELECT current_setting('myapp.unset')        -- 42704
    //   SELECT current_setting('myapp.unset', true)  -- NULL

    // Variables are SESSION state, not data — they do NOT roll back with a transaction. reset_var
    // clears one by name.
    s.reset_var("myapp.tenant")?;
    s.close();

    Ok(())
}
