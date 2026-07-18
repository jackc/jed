use std::sync::Arc;

use jed::value::Value;
use jed::{CreateOptions, Database, ExtensionRegistry, HostFunction, ScalarType, Volatility};

fn main() -> jed::Result<()> {
    // A host registers its own SCALAR FUNCTIONS over the built-in types. Build a registry, add
    // functions, and hand it to create/open — the engine freezes it for the handle's lifetime and
    // shares it into every session. It is a *handle* setting, never written to the file (a reopening
    // host brings its own).
    let mut registry = ExtensionRegistry::new();

    // discount(cents, pct) -> the price after a whole-percent discount. STRICT — a NULL argument
    // short-circuits to NULL before the kernel runs, so the closure never sees one — and reached by
    // an EXACT (i64, i64) signature (no implicit promotion; a built-in of the same signature would
    // win). `.cost(2)` is charged once per call and gated against a session's max_cost, so the
    // function stays inside the untrusted-query bound.
    registry.register_function(
        HostFunction::new(
            "discount",
            vec![ScalarType::Int64, ScalarType::Int64],
            ScalarType::Int64,
            Box::new(|args: &[Value]| -> jed::Result<Value> {
                let (Value::Int(cents), Value::Int(pct)) = (&args[0], &args[1]) else {
                    unreachable!("strict + resolved (i64, i64) args")
                };
                Ok(Value::Int(cents - cents * pct / 100))
            }),
        )
        .volatility(Volatility::Immutable) // same inputs ⇒ same output
        .cross_core(true) // results are byte-identical on every core
        .cost(2)
        .component_id("com.example/discount") // a stable identity for index-backing
        .semantic_version(1), // bump when a formula change would invalidate stored index keys
    )?;

    let mut db = Database::create(CreateOptions { extensions: Arc::new(registry), ..Default::default() })?;

    db.execute("CREATE TABLE product (id i32 PRIMARY KEY, name text, price_cents i64)", &[])?;
    db.execute("INSERT INTO product VALUES (1, 'Mug', 1250), (2, 'Notebook', 400)", &[])?;

    // Because discount is IMMUTABLE and carries a component identity, it can back a persisted index.
    // On reopen, if the registry supplies a different component/version, the index is skipped for
    // reads (a correct heap scan) and refused for writes — never a silently stale result.
    db.execute("CREATE INDEX ON product (discount(price_cents, 10))", &[])?;

    // Call it by name from SQL, exactly like a built-in.
    let sql = "SELECT name, discount(price_cents, 15) AS sale FROM product ORDER BY id";
    for row in db.query(sql, &[])? {
        println!("{} -> {}", row[0].render(), row[1].render()); // Mug -> 1063, Notebook -> 340
    }

    Ok(())
}
