//! Cross-check: the Rust key encoder must reproduce the byte-exact vectors in
//! spec/encoding/integers.toml (CLAUDE.md §8). This is what guarantees the Rust and
//! Go cores iterate keys in the same order. TOML is a test-time-only dependency.
//!
//! The file also carries the NON-integer key vectors `uuid` (method `uuid-raw16`): a uuid key
//! is the bare 16 bytes — exactly what `parse_uuid` produces and the executor stores for a uuid
//! PRIMARY KEY (encoding.md §2.7) — and `boolean` (method `bool-byte`, §2.9): a single byte
//! 0x00 false / 0x01 true, what `encode_bool` produces and the executor stores for a boolean
//! PRIMARY KEY. The nullable/descending vectors follow the shared §2.2/§2.3 framing (presence
//! tag, one's-complement).

use jed::encoding::{decode_int, encode_bool, encode_int, encode_nullable};
use jed::types::ScalarType;
use jed::value::{Value, parse_uuid};
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

fn is_null(case: &toml::Value) -> bool {
    case.get("null").and_then(|b| b.as_bool()) == Some(true)
}

/// The nullable key slot for a uuid case: `0x01` for NULL, else `0x00` + the 16 raw bytes.
fn nullable_uuid(case: &toml::Value) -> Vec<u8> {
    if is_null(case) {
        return vec![0x01];
    }
    let u = parse_uuid(case["value"].as_str().unwrap()).unwrap();
    let mut out = vec![0x00];
    out.extend_from_slice(&u);
    out
}

/// The nullable key slot for a boolean case: `0x01` for NULL, else `0x00` + the 1-byte body.
fn nullable_bool(case: &toml::Value) -> Vec<u8> {
    if is_null(case) {
        return vec![0x01];
    }
    let mut out = vec![0x00];
    out.extend_from_slice(&encode_bool(case["value"].as_bool().unwrap()));
    out
}

#[test]
fn bare_vectors_match_and_roundtrip() {
    let v: toml::Value = toml::from_str(&spec("encoding/integers.toml")).unwrap();
    for group in v["bare"].as_array().unwrap() {
        let name = group["type"].as_str().unwrap();
        for case in group["cases"].as_array().unwrap() {
            let want = case["bytes"].as_str().unwrap();
            if name == "uuid" {
                // A uuid key is the bare 16 bytes `parse_uuid` produces (and the executor
                // stores). Round-trip is the public render (canonical form ↔ bytes).
                let value = case["value"].as_str().unwrap();
                let bytes = parse_uuid(value).unwrap();
                assert_eq!(hex(&bytes), want, "uuid value {value}");
                assert_eq!(
                    Value::Uuid(bytes).render(),
                    value,
                    "uuid round-trip {value}"
                );
                continue;
            }
            if name == "boolean" {
                // A boolean key is the single `bool-byte` (0x00 false / 0x01 true) that
                // `encode_bool` produces and the executor stores for a boolean PRIMARY KEY.
                let value = case["value"].as_bool().unwrap();
                assert_eq!(hex(&encode_bool(value)), want, "boolean value {value}");
                continue;
            }
            let t = ty(name);
            let value = case["value"].as_integer().unwrap();
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
        let name = group["type"].as_str().unwrap();
        for case in group["cases"].as_array().unwrap() {
            let want = case["bytes"].as_str().unwrap();
            if name == "uuid" {
                assert_eq!(hex(&nullable_uuid(case)), want, "nullable uuid");
                continue;
            }
            if name == "boolean" {
                assert_eq!(hex(&nullable_bool(case)), want, "nullable boolean");
                continue;
            }
            let t = ty(name);
            let value = if is_null(case) {
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
        let name = group["type"].as_str().unwrap();
        for case in group["cases"].as_array().unwrap() {
            let want = case["bytes"].as_str().unwrap();
            if name == "uuid" {
                assert_eq!(hex(&invert(&nullable_uuid(case))), want, "descending uuid");
                continue;
            }
            if name == "boolean" {
                assert_eq!(
                    hex(&invert(&nullable_bool(case))),
                    want,
                    "descending boolean"
                );
                continue;
            }
            let t = ty(name);
            let value = if is_null(case) {
                None
            } else {
                Some(case["value"].as_integer().unwrap())
            };
            let got = invert(&encode_nullable(t, value));
            assert_eq!(hex(&got), want, "descending {value:?}");
        }
    }
}

/// The GIN `jsonb_ops` term vectors (spec/encoding/gin_jsonb_terms.toml, gin.md §4.1): each
/// document's deduplicated, byte-sorted index term set, and each `?` / `?|` / `?&` key's query term.
#[test]
fn gin_jsonb_terms_match_vectors() {
    let v: toml::Value = toml::from_str(&spec("encoding/gin_jsonb_terms.toml")).unwrap();
    let mut checked = 0;
    for case in v["index"].as_array().unwrap() {
        let doc = case["doc"].as_str().unwrap();
        let node = jed::json::jsonb_in(doc).unwrap();
        let got: Vec<String> = jed::json::gin_jsonb_index_terms(&node)
            .iter()
            .map(|t| hex(t))
            .collect();
        let want: Vec<&str> = case["terms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert_eq!(got, want, "index terms of {doc}");
        checked += 1;
    }
    for case in v["key"].as_array().unwrap() {
        let key = case["key"].as_str().unwrap();
        assert_eq!(
            hex(&jed::json::gin_jsonb_key_term(key)),
            case["term"].as_str().unwrap(),
            "key term of {key:?}"
        );
        checked += 1;
    }
    assert!(checked > 20);
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex_list(v: &toml::Value) -> Vec<Vec<u8>> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| unhex(s.as_str().unwrap()))
        .collect()
}

/// The GIN posting-run vectors (spec/encoding/gin_posting_runs.toml, gin.md §4.5): the run codec,
/// the build's greedy packing, and run maintenance over an in-memory index store.
#[test]
fn gin_posting_runs_match_vectors() {
    use jed::gin_runs::{decode_run, encode_run, gin_insert, gin_remove, pack_runs};
    let v: toml::Value = toml::from_str(&spec("encoding/gin_posting_runs.toml")).unwrap();
    let mut checked = 0;
    for case in v["run"].as_array().unwrap() {
        let term = unhex(case["term"].as_str().unwrap());
        let keys = hex_list(&case["keys"]);
        let run = encode_run(&term, &keys);
        assert_eq!(hex(&run), case["run"].as_str().unwrap());
        assert_eq!(decode_run(term.len(), &run).unwrap(), keys);
        checked += 1;
    }
    for case in v["pack"].as_array().unwrap() {
        let term = unhex(case["term"].as_str().unwrap());
        let got: Vec<String> = pack_runs(&term, &hex_list(&case["keys"]))
            .iter()
            .map(|r| hex(r))
            .collect();
        let want: Vec<String> = hex_list(&case["runs"]).iter().map(|r| hex(r)).collect();
        assert_eq!(got, want);
        checked += 1;
    }
    for case in v["history"].as_array().unwrap() {
        let term = unhex(case["term"].as_str().unwrap());
        let mut store = jed::storage::TableStore::new(4080, Vec::new());
        for op in case["ops"].as_array().unwrap() {
            let op = op.as_str().unwrap();
            let key = unhex(&op[1..]);
            if op.starts_with('+') {
                gin_insert(&mut store, &term, &key).unwrap();
            } else {
                gin_remove(&mut store, &term, &key).unwrap();
            }
        }
        let got: Vec<String> = store
            .iter_entries()
            .unwrap()
            .iter()
            .map(|(k, _)| hex(k))
            .collect();
        let want: Vec<String> = hex_list(&case["runs"]).iter().map(|r| hex(r)).collect();
        assert_eq!(got, want, "history {}", case["name"]);
        checked += 1;
    }
    assert!(checked >= 10);
}
