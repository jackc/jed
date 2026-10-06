//! Cross-check the query-memory logical size schedule against the shared vectors in
//! spec/cost/memory_sizes.toml (spec/design/memory.md §3). Every core runs the same vectors, so the
//! measurement — and therefore a `54P05` threshold — is cross-core identical.

use jed::{CreateOptions, Database, SessionOptions};
use std::path::Path;

#[test]
fn memory_size_vectors_match_spec() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/cost/memory_sizes.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let doc: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(doc["schema_version"].as_integer(), Some(1));
    for v in doc["vector"].as_array().expect("[[vector]]") {
        let sql = v["sql"].as_str().unwrap();
        let db = Database::create(CreateOptions::default()).unwrap();
        let mut s = db.session(SessionOptions::default());
        if let Some(setup) = v.get("setup") {
            for stmt in setup.as_array().unwrap() {
                s.execute(stmt.as_str().unwrap(), &[])
                    .unwrap_or_else(|e| panic!("{sql}: setup: {e:?}"));
            }
        }
        let rows: Vec<Vec<jed::Value>> = s
            .query(sql, &[])
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
            .collect();
        assert_eq!(rows.len(), 1, "{sql}: one row");
        let got = match v["measure"].as_str().unwrap() {
            "value" => {
                assert_eq!(rows[0].len(), 1, "{sql}: one column");
                jed::memsize::value_bytes(&rows[0][0])
            }
            "row" => jed::memsize::row_bytes(&rows[0]),
            other => panic!("unknown measure {other}"),
        };
        assert_eq!(got, v["bytes"].as_integer().unwrap(), "{sql}");
    }
}
