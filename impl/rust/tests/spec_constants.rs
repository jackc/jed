//! Cross-check: the hand-written type and error constants in the Rust core must
//! match the canonical spec data tables (CLAUDE.md §5). TOML is a test-time-only
//! dependency. If the spec changes and the core doesn't (or vice versa), this fails.

use jed::costs::COSTS;
use jed::error::SqlState;
use jed::operators::{AGGREGATES, OPERATORS, SET_RETURNING};
use jed::sqlstate::ERRORS;
use jed::types::ScalarType;
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

    // boolean is a storable non-integer scalar (storable = true): it resolves to a column
    // `ScalarType::Bool`, canonical-names to "boolean", and its aliases resolve too. It has
    // no integer fields (bits/min/max/rank), so those accessors are not exercised here.
    let boolean = types
        .iter()
        .find(|t| t["id"].as_str() == Some("boolean"))
        .expect("boolean type present");
    assert_eq!(
        boolean["family"].as_str(),
        Some("boolean"),
        "boolean family"
    );
    assert_eq!(
        boolean["storable"].as_bool(),
        Some(true),
        "boolean is storable this slice"
    );
    let bool_ty = ScalarType::from_name("boolean").expect("boolean resolves to a ScalarType");
    assert_eq!(
        bool_ty.canonical_name(),
        "boolean",
        "boolean canonical name"
    );
    for alias in boolean["aliases"].as_array().unwrap() {
        let a = alias.as_str().unwrap();
        assert_eq!(
            ScalarType::from_name(a),
            Some(bool_ty),
            "alias {a} resolves to boolean"
        );
    }

    // text: storable, variable-width; its aliases resolve to ScalarType::Text.
    let text = types
        .iter()
        .find(|t| t["id"].as_str() == Some("text"))
        .expect("text type present");
    assert_eq!(text["storable"].as_bool(), Some(true));
    assert_eq!(ScalarType::from_name("text"), Some(ScalarType::Text));
    for alias in text["aliases"].as_array().unwrap() {
        assert_eq!(
            ScalarType::from_name(alias.as_str().unwrap()),
            Some(ScalarType::Text)
        );
    }

    // decimal: storable, the decimal family; aliases resolve; the precision/scale caps match
    // the decimal module's constants (a cross-core contract, spec/design/decimal.md §2).
    let decimal = types
        .iter()
        .find(|t| t["id"].as_str() == Some("decimal"))
        .expect("decimal type present");
    assert_eq!(
        decimal["family"].as_str(),
        Some("decimal"),
        "decimal family"
    );
    assert_eq!(
        decimal["storable"].as_bool(),
        Some(true),
        "decimal storable"
    );
    assert_eq!(ScalarType::Decimal.canonical_name(), "decimal");
    for name in ["decimal", "numeric", "dec"] {
        assert_eq!(
            ScalarType::from_name(name),
            Some(ScalarType::Decimal),
            "{name} resolves to decimal"
        );
    }
    assert_eq!(
        decimal["max_precision"].as_integer().unwrap() as u32,
        jed::decimal::MAX_PRECISION,
        "max_precision matches the decimal module"
    );
    assert_eq!(
        decimal["max_scale"].as_integer().unwrap() as u32,
        jed::decimal::MAX_SCALE,
        "max_scale matches the decimal module"
    );
    assert_eq!(
        decimal["max_int_digits"].as_integer().unwrap() as u32,
        jed::decimal::MAX_INT_DIGITS,
        "max_int_digits matches the decimal module"
    );

    // uuid: storable, the uuid family, fixed-width (the first non-integer with a width_bytes).
    // Its on-disk width (16) is a cross-core contract, so cross-check it against the spec.
    let uuid = types
        .iter()
        .find(|t| t["id"].as_str() == Some("uuid"))
        .expect("uuid type present");
    assert_eq!(uuid["family"].as_str(), Some("uuid"), "uuid family");
    assert_eq!(uuid["storable"].as_bool(), Some(true), "uuid storable");
    assert_eq!(ScalarType::from_name("uuid"), Some(ScalarType::Uuid));
    assert_eq!(ScalarType::Uuid.canonical_name(), "uuid");
    assert_eq!(ScalarType::Uuid.width_bytes(), 16, "uuid is fixed 16 bytes");
    assert_eq!(
        ScalarType::Uuid.width_bytes() as i64,
        uuid["encoding"]["width_bytes"].as_integer().unwrap(),
        "uuid encoding width matches the spec"
    );
}

#[test]
fn is_fixed_width_partitions_width_bytes_domain() {
    // The index tail-slot skip (executor.rs `index_bound_rows`) advances over each trailing key
    // component by `width_bytes`, which PANICS on the variable-width scalars. `detect_scan_bound`
    // must therefore exclude a variable-width tail column from the index-bound pushdown — it gates
    // on `is_fixed_width`. So `is_fixed_width` MUST be true exactly when `width_bytes` does not
    // panic; this pins the two partitions together so a newly-added type cannot drift them apart
    // (the latent gap this guards against — a panic on a multi-column index with a text/decimal/
    // bytea/interval trailing key column under an equality on the leading column).
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // silence the expected width_bytes panics
    let domain: Vec<(ScalarType, bool)> = ScalarType::all()
        .into_iter()
        .map(|st| (st, std::panic::catch_unwind(|| st.width_bytes()).is_ok()))
        .collect();
    std::panic::set_hook(prev);
    for (st, width_ok) in domain {
        assert_eq!(
            st.is_fixed_width(),
            width_ok,
            "{st:?}: is_fixed_width must match whether width_bytes is defined"
        );
    }

    // The concrete partition, spelled out so the intent is legible and all 14 types are accounted
    // for (4 variable-width + 10 fixed-width).
    for st in [
        ScalarType::Text,
        ScalarType::Decimal,
        ScalarType::Bytea,
        ScalarType::Interval,
    ] {
        assert!(!st.is_fixed_width(), "{st:?} is variable-width");
    }
    for st in [
        ScalarType::Int16,
        ScalarType::Int32,
        ScalarType::Int64,
        ScalarType::Bool,
        ScalarType::Uuid,
        ScalarType::Timestamp,
        ScalarType::Timestamptz,
        ScalarType::Float32,
        ScalarType::Float64,
        ScalarType::Date,
    ] {
        assert!(st.is_fixed_width(), "{st:?} is fixed-width");
    }
}

#[test]
fn error_codes_are_registered() {
    // The generated SqlState table (codegen middle path, CLAUDE.md §5) must match the canonical
    // registry. The drift gate (`rake verify`) pins the generated file byte-for-byte; this test
    // additionally COMPILES the generated `ERRORS` slice into the crate and asserts it matches
    // registry.toml row-for-row — a genuinely compiled-and-verified artifact (like
    // operators_match_spec). The enum is not iterable, so the cross-check walks `ERRORS`.
    let v: toml::Value = toml::from_str(&spec("errors/registry.toml")).unwrap();
    let rows = v["error"].as_array().expect("[[error]] array");
    assert_eq!(rows.len(), ERRORS.len(), "error count");

    for (row, desc) in rows.iter().zip(ERRORS.iter()) {
        assert_eq!(desc.code, row["code"].as_str().unwrap(), "code");
        assert_eq!(
            desc.name,
            row["name"].as_str().unwrap(),
            "{} name",
            desc.code
        );
        assert_eq!(
            desc.class,
            row["class"].as_str().unwrap(),
            "{} class",
            desc.code
        );
    }

    // Spot-check that the enum's code() agrees with the data — ties the type-safe enum to the
    // table (the integer-overflow code the corpus matches on by name, CLAUDE.md §8).
    assert_eq!(SqlState::NumericValueOutOfRange.code(), "22003");
    assert_eq!(SqlState::DivisionByZero.code(), "22012");
    assert_eq!(SqlState::FeatureNotSupported.code(), "0A000");
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
        let fams: Vec<&str> = row["arg_families"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        // Operators are overloaded across operand families (one row per (name,
        // arg_families) — e.g. `eq` for integer and for text), so match on the full
        // signature, not the name alone.
        let desc = OPERATORS
            .iter()
            .find(|d| d.name == name && d.arg_families == fams.as_slice())
            .unwrap_or_else(|| panic!("generated table missing operator {name} {fams:?}"));

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
        assert_eq!(
            desc.result,
            row["result"].as_str().unwrap(),
            "{name} result"
        );
        assert_eq!(desc.null, row["null"].as_str().unwrap(), "{name} null");
        assert_eq!(
            desc.precedence as i64,
            row.get("precedence")
                .and_then(|p| p.as_integer())
                .unwrap_or(0),
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

        // Optional named/default-argument metadata (functions.md §11); absent ⇒ empty slice.
        let str_array = |key: &str| -> Vec<String> {
            row.get(key)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect())
                .unwrap_or_default()
        };
        assert_eq!(
            desc.arg_names,
            str_array("arg_names").as_slice(),
            "{name} arg_names"
        );
        assert_eq!(
            desc.arg_defaults,
            str_array("arg_defaults").as_slice(),
            "{name} arg_defaults"
        );

        match row.get("symbol").and_then(|s| s.as_str()) {
            Some(sym) => assert_eq!(desc.symbol, Some(sym), "{name} symbol"),
            None => assert_eq!(desc.symbol, None, "{name} symbol absent"),
        }
    }
}

#[test]
fn aggregates_match_spec() {
    // The generated aggregate descriptor table must match the canonical catalog's
    // [[aggregate]] rows field-for-field (the codegen middle path, CLAUDE.md §5). Aggregates
    // are overloaded across operand families (one row per (name, arg_families)), like operators.
    let v: toml::Value = toml::from_str(&spec("functions/catalog.toml")).unwrap();
    let aggs = v["aggregate"].as_array().expect("[[aggregate]] array");
    assert_eq!(aggs.len(), AGGREGATES.len(), "aggregate count");

    for row in aggs {
        let name = row["name"].as_str().unwrap();
        let fams: Vec<&str> = row
            .get("arg_families")
            .and_then(|f| f.as_array())
            .map(|a| a.iter().map(|x| x.as_str().unwrap()).collect())
            .unwrap_or_default();
        let desc = AGGREGATES
            .iter()
            .find(|d| d.name == name && d.arg_families == fams.as_slice())
            .unwrap_or_else(|| panic!("generated table missing aggregate {name} {fams:?}"));

        assert_eq!(row["kind"].as_str().unwrap(), "aggregate", "{name} kind");
        assert_eq!(
            desc.surface,
            row["surface"].as_str().unwrap(),
            "{name} surface"
        );
        assert_eq!(desc.arg, row["arg"].as_str().unwrap(), "{name} arg");
        assert_eq!(
            desc.result,
            row["result"].as_str().unwrap(),
            "{name} result"
        );
        assert_eq!(desc.null, row["null"].as_str().unwrap(), "{name} null");

        let errs: Vec<&str> = row["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(desc.errors, errs.as_slice(), "{name} errors");
    }
}

#[test]
fn set_returning_match_spec() {
    // The generated set-returning descriptor table must match the canonical catalog's
    // [[set_returning]] rows field-for-field (the codegen middle path, CLAUDE.md §5). SRFs are
    // overloaded across ARITY (one row per (name, arity)) — functions.md §10.
    let v: toml::Value = toml::from_str(&spec("functions/catalog.toml")).unwrap();
    let srfs = v["set_returning"]
        .as_array()
        .expect("[[set_returning]] array");
    assert_eq!(srfs.len(), SET_RETURNING.len(), "set_returning count");

    for row in srfs {
        let name = row["name"].as_str().unwrap();
        let arity = row["arity"].as_integer().unwrap() as u8;
        let desc = SET_RETURNING
            .iter()
            .find(|d| d.name == name && d.arity == arity)
            .unwrap_or_else(|| {
                panic!("generated table missing set_returning {name}/arity-{arity}")
            });

        assert_eq!(
            row["kind"].as_str().unwrap(),
            "set_returning",
            "{name} kind"
        );
        assert_eq!(
            desc.surface,
            row["surface"].as_str().unwrap(),
            "{name} surface"
        );
        let fams: Vec<&str> = row["arg_families"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(desc.arg_families, fams.as_slice(), "{name} arg_families");
        assert_eq!(
            desc.arg_resolution,
            row["arg_resolution"].as_str().unwrap(),
            "{name} arg_resolution"
        );
        assert_eq!(
            desc.result,
            row["result"].as_str().unwrap(),
            "{name} result"
        );
        assert_eq!(
            desc.column,
            row["column"].as_str().unwrap(),
            "{name} column"
        );
        assert_eq!(desc.null, row["null"].as_str().unwrap(), "{name} null");

        let errs: Vec<&str> = row["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(desc.errors, errs.as_slice(), "{name} errors");
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
            "page_read" => COSTS.page_read,
            "value_compress" => COSTS.value_compress,
            "value_decompress" => COSTS.value_decompress,
            "decimal_work" => COSTS.decimal_work,
            "row_produced" => COSTS.row_produced,
            "operator_eval" => COSTS.operator_eval,
            "aggregate_accumulate" => COSTS.aggregate_accumulate,
            "generated_row" => COSTS.generated_row,
            "cte_scan_row" => COSTS.cte_scan_row,
            "sequence_advance" => COSTS.sequence_advance,
            "gin_entry" => COSTS.gin_entry,
            "collate" => COSTS.collate,
            "timezone" => COSTS.timezone,
            other => panic!("cost unit {other} has no COSTS field — update this cross-check"),
        }
    };

    // The weight() closure above forces this cross-check to be updated whenever a unit is
    // added (a new unit with no COSTS field panics), so we don't pin an exact count here.
    for u in units {
        let id = u["id"].as_str().unwrap();
        assert_eq!(weight(id), u["weight"].as_integer().unwrap(), "{id} weight");
    }
}
