//! Cross-check: the Rust key encoder must reproduce the byte-exact vectors in
//! spec/encoding/integers.toml (CLAUDE.md §8). This is what guarantees the Rust and
//! Go cores iterate keys in the same order. TOML is a test-time-only dependency.

use abide::encoding::{decode_int, encode_int, encode_nullable};
use abide::types::ScalarType;
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ty(name: &str) -> ScalarType {
    ScalarType::from_name(name).unwrap()
}

fn invert(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|b| b ^ 0xFF).collect()
}

#[test]
fn bare_vectors_match_and_roundtrip() {
    let v: toml::Value = toml::from_str(&spec("encoding/integers.toml")).unwrap();
    for group in v["bare"].as_array().unwrap() {
        let t = ty(group["type"].as_str().unwrap());
        for case in group["cases"].as_array().unwrap() {
            let value = case["value"].as_integer().unwrap();
            let want = case["bytes"].as_str().unwrap();
            let bytes = encode_int(t, value);
            assert_eq!(hex(&bytes), want, "{} value {value}", t.canonical_name());
            assert_eq!(decode_int(t, &bytes), value, "round-trip {value}");
        }
    }
}

#[test]
fn nullable_vectors_match() {
    let v: toml::Value = toml::from_str(&spec("encoding/integers.toml")).unwrap();
    for group in v["nullable"].as_array().unwrap() {
        let t = ty(group["type"].as_str().unwrap());
        for case in group["cases"].as_array().unwrap() {
            let want = case["bytes"].as_str().unwrap();
            let value = if case.get("null").and_then(|b| b.as_bool()) == Some(true) {
                None
            } else {
                Some(case["value"].as_integer().unwrap())
            };
            assert_eq!(hex(&encode_nullable(t, value)), want, "nullable {value:?}");
        }
    }
}

#[test]
fn descending_is_inverted_nullable() {
    let v: toml::Value = toml::from_str(&spec("encoding/integers.toml")).unwrap();
    for group in v["descending"].as_array().unwrap() {
        let t = ty(group["type"].as_str().unwrap());
        for case in group["cases"].as_array().unwrap() {
            let want = case["bytes"].as_str().unwrap();
            let value = if case.get("null").and_then(|b| b.as_bool()) == Some(true) {
                None
            } else {
                Some(case["value"].as_integer().unwrap())
            };
            let got = invert(&encode_nullable(t, value));
            assert_eq!(hex(&got), want, "descending {value:?}");
        }
    }
}
