//! Cross-check: the hand-written type and error constants in the Rust core must
//! match the canonical spec data tables (CLAUDE.md §5). TOML is a test-time-only
//! dependency. If the spec changes and the core doesn't (or vice versa), this fails.

use abide::costs::COSTS;
use abide::error::SqlState;
use abide::operators::OPERATORS;
use abide::types::{is_boolean_type_name, ScalarType};
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

    // The storable scalar types are exactly the three integers; each maps to a
    // `ScalarType` with matching width/range/rank/encoding (CLAUDE.md §5 cross-check).
    let integers: Vec<&toml::Value> = types
        .iter()
        .filter(|t| t["family"].as_str() == Some("integer"))
        .collect();
    assert_eq!(integers.len(), 3, "three storable integer scalar types");

    for t in &integers {
        let id = t["id"].as_str().unwrap();
        let st = ScalarType::from_name(id).unwrap_or_else(|| panic!("unknown type id {id}"));
        assert_eq!(st.canonical_name(), id, "canonical name");
        assert_eq!(t["storable"].as_bool(), Some(true), "{id} storable");

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

    // boolean is the first non-integer scalar: expression-only (storable = false), so
    // it is NOT a column `ScalarType`, only a recognized non-storable type name.
    let boolean = types
        .iter()
        .find(|t| t["id"].as_str() == Some("boolean"))
        .expect("boolean type present");
    assert_eq!(boolean["family"].as_str(), Some("boolean"), "boolean family");
    assert_eq!(
        boolean["storable"].as_bool(),
        Some(false),
        "boolean is not storable this slice"
    );
    assert!(
        ScalarType::from_name("boolean").is_none() && ScalarType::from_name("bool").is_none(),
        "boolean is not a storable column type"
    );
    assert!(
        is_boolean_type_name("boolean") && is_boolean_type_name("BOOL"),
        "boolean type name is recognized (case-insensitively)"
    );
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
        SqlState::DivisionByZero,
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

#[test]
fn operators_match_spec() {
    // The generated operator descriptor table (codegen middle path, CLAUDE.md §5) must
    // match the canonical catalog field-for-field. This also compiles the generated
    // table into the crate, so a malformed generation fails the build.
    let v: toml::Value = toml::from_str(&spec("functions/catalog.toml")).unwrap();
    let ops = v["operator"].as_array().expect("[[operator]] array");
    assert_eq!(ops.len(), OPERATORS.len(), "operator count");

    for row in ops {
        let name = row["name"].as_str().unwrap();
        let desc = OPERATORS
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("generated table missing operator {name}"));

        assert_eq!(desc.kind, row["kind"].as_str().unwrap(), "{name} kind");
        assert_eq!(
            desc.arity as i64,
            row["arity"].as_integer().unwrap(),
            "{name} arity"
        );
        assert_eq!(
            desc.arg_resolution,
            row["arg_resolution"].as_str().unwrap(),
            "{name} arg_resolution"
        );
        assert_eq!(desc.result, row["result"].as_str().unwrap(), "{name} result");
        assert_eq!(desc.null, row["null"].as_str().unwrap(), "{name} null");
        assert_eq!(
            desc.precedence as i64,
            row.get("precedence").and_then(|p| p.as_integer()).unwrap_or(0),
            "{name} precedence"
        );

        let fams: Vec<&str> = row["arg_families"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(desc.arg_families, fams.as_slice(), "{name} arg_families");

        let errs: Vec<&str> = row["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(desc.errors, errs.as_slice(), "{name} errors");

        match row.get("symbol").and_then(|s| s.as_str()) {
            Some(sym) => assert_eq!(desc.symbol, Some(sym), "{name} symbol"),
            None => assert_eq!(desc.symbol, None, "{name} symbol absent"),
        }
    }
}

#[test]
fn cost_schedule_matches_spec() {
    // The generated cost schedule (codegen middle path, CLAUDE.md §5/§13) must match the
    // canonical schedule.toml weight-for-weight. This also compiles the generated table
    // into the crate. Cost is a cross-core contract (§8): every core reads these weights.
    let v: toml::Value = toml::from_str(&spec("cost/schedule.toml")).unwrap();
    let units = v["unit"].as_array().expect("[[unit]] array");

    // Every unit id maps to a field on COSTS; a new unit forces this cross-check to be
    // updated (so a core cannot silently ignore a unit the schedule adds).
    let weight = |id: &str| -> i64 {
        match id {
            "storage_row_read" => COSTS.storage_row_read,
            "row_produced" => COSTS.row_produced,
            "operator_eval" => COSTS.operator_eval,
            other => panic!("cost unit {other} has no COSTS field — update this cross-check"),
        }
    };

    assert_eq!(units.len(), 3, "the three phase-1 cost units");
    for u in units {
        let id = u["id"].as_str().unwrap();
        assert_eq!(weight(id), u["weight"].as_integer().unwrap(), "{id} weight");
    }
}
