//! Collation slice 1c — the host `db.import_collation` API (spec/design/collation.md §4). These are
//! the host-API behaviors the conformance corpus cannot express (CLAUDE.md §10): the import call
//! itself, its idempotency, the same-name conflict, and the `C` rejection. The SQL behavior a loaded
//! collation drives (COLLATE / ORDER BY / errors) lives in suites/collation/collate.test, which runs
//! on every core. Mirrored by impl/go/collation_host_test.go and impl/ts/tests/collation_host.test.ts.

use jed::collation::compile_collation;
use jed::value::Value;
use jed::{Database, Outcome, execute};
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn dev_root() -> jed::collation::Collation {
    compile_collation("dev-root", &spec("collation/fixtures/dev-root.allkeys")).unwrap()
}

/// A collation under the name "dev-root" but with the dev-nordic *table* (a different content hash)
/// — the conflicting import.
fn dev_root_named_but_nordic_table() -> jed::collation::Collation {
    let def = format!(
        "{}\n{}",
        spec("collation/fixtures/dev-root.allkeys"),
        spec("collation/fixtures/dev-nordic.ldml")
    );
    compile_collation("dev-root", &def).unwrap()
}

fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

#[test]
fn import_then_use_in_a_query() {
    let mut db = Database::new();
    assert_eq!(db.import_collation(dev_root()).unwrap(), "dev-root");
    // The imported collation is usable by name: 'ä' < 'z' is true under dev-root (ä near a),
    // the opposite of the C byte order where it is false.
    let rows = query(&mut db, "SELECT 'ä' < 'z' COLLATE \"dev-root\"");
    assert_eq!(rows, vec![vec![Value::Bool(true)]]);
}

#[test]
fn import_is_idempotent_by_name_and_hash() {
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
    // Re-importing the identical (name, content) collation is a no-op success, returning the name.
    assert_eq!(db.import_collation(dev_root()).unwrap(), "dev-root");
}

#[test]
fn import_conflict_same_name_different_table_is_42710() {
    let mut db = Database::new();
    db.import_collation(dev_root()).unwrap();
    // A DIFFERENT table under a name already in use is a conflict (it would invalidate any structure
    // built under that name — collation.md §4).
    let err = db
        .import_collation(dev_root_named_but_nordic_table())
        .unwrap_err();
    assert_eq!(err.code(), "42710");
}

#[test]
fn importing_C_is_rejected() {
    let mut db = Database::new();
    // `C` is table-free and built in; it is never imported (collation.md §4).
    let c = compile_collation("C", &spec("collation/fixtures/dev-root.allkeys")).unwrap();
    let err = db.import_collation(c).unwrap_err();
    assert_eq!(err.code(), "42710");
}
