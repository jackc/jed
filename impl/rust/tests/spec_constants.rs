//! Cross-check: the hand-written type and error constants in the Rust core must
//! match the canonical spec data tables (CLAUDE.md §5). TOML is a test-time-only
//! dependency. If the spec changes and the core doesn't (or vice versa), this fails.

use abide::error::SqlState;
use abide::types::ScalarType;
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn scalar_types_match_spec() {
    let v: toml::Value = toml::from_str(&spec("types/scalars.toml")).unwrap();
    let types = v["type"].as_array().expect("[[type]] array");
    assert_eq!(types.len(), 3, "step-1 has exactly three scalar types");

    for t in types {
        let id = t["id"].as_str().unwrap();
        let st = ScalarType::from_name(id).unwrap_or_else(|| panic!("unknown type id {id}"));
        assert_eq!(st.canonical_name(), id, "canonical name");

        let bits = t["bits"].as_integer().unwrap();
        assert_eq!(st.width_bytes() as i64 * 8, bits, "{id} bits");
        assert_eq!(st.min(), t["min"].as_integer().unwrap(), "{id} min");
        assert_eq!(st.max(), t["max"].as_integer().unwrap(), "{id} max");
        assert_eq!(
            st.rank() as i64,
            t["rank"].as_integer().unwrap(),
            "{id} rank"
        );
        assert_eq!(
            st.width_bytes() as i64,
            t["encoding"]["width_bytes"].as_integer().unwrap(),
            "{id} encoding width"
        );

        for alias in t["aliases"].as_array().unwrap() {
            let a = alias.as_str().unwrap();
            assert_eq!(
                ScalarType::from_name(a),
                Some(st),
                "alias {a} resolves to {id}"
            );
        }
    }
}

#[test]
fn error_codes_are_registered() {
    let v: toml::Value = toml::from_str(&spec("errors/registry.toml")).unwrap();
    let codes: std::collections::BTreeSet<String> = v["error"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap().to_string())
        .collect();

    // Every SQLSTATE the core can raise must exist in the registry.
    for st in [
        SqlState::NumericValueOutOfRange,
        SqlState::NotNullViolation,
        SqlState::UniqueViolation,
        SqlState::SyntaxError,
        SqlState::UndefinedTable,
        SqlState::UndefinedColumn,
        SqlState::UndefinedObject,
        SqlState::DatatypeMismatch,
        SqlState::DuplicateTable,
        SqlState::DuplicateColumn,
        SqlState::InvalidTableDefinition,
        SqlState::FeatureNotSupported,
        SqlState::DataCorrupted,
    ] {
        assert!(
            codes.contains(st.code()),
            "code {} missing from registry",
            st.code()
        );
    }

    // The integer-overflow code the corpus matches on, by name (CLAUDE.md §8).
    let overflow = v["error"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"].as_str() == Some("22003"))
        .expect("22003 in registry");
    assert_eq!(
        overflow["name"].as_str(),
        Some("numeric_value_out_of_range")
    );
    assert_eq!(SqlState::NumericValueOutOfRange.code(), "22003");
}
