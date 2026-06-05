//! Cross-check: the Rust timestamp parser/renderer must reproduce the byte-exact vectors in
//! spec/encoding/timestamps.toml (CLAUDE.md §8) — the parse/render arithmetic that must be
//! identical across the Rust/Go/TS cores. TOML is a test-time-only dependency.

use jed::timestamp::{parse_timestamp, parse_timestamptz, render_timestamp, render_timestamptz};
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn vectors() -> toml::Value {
    toml::from_str(&spec("encoding/timestamps.toml")).unwrap()
}

fn parse(ty: &str, input: &str) -> jed::Result<i64> {
    match ty {
        "timestamp" => parse_timestamp(input),
        "timestamptz" => parse_timestamptz(input),
        other => panic!("unknown type {other}"),
    }
}

fn render(ty: &str, micros: i64) -> String {
    match ty {
        "timestamp" => render_timestamp(micros),
        "timestamptz" => render_timestamptz(micros),
        other => panic!("unknown type {other}"),
    }
}

#[test]
fn parse_vectors_match() {
    let v = vectors();
    for group in v["parse"].as_array().unwrap() {
        let ty = group["type"].as_str().unwrap();
        for case in group["cases"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let want = case["micros"].as_integer().unwrap();
            let got = parse(ty, input)
                .unwrap_or_else(|e| panic!("{ty} parse {input:?} failed: {}", e.message));
            assert_eq!(got, want, "{ty} parse {input:?}");
        }
    }
}

#[test]
fn parse_error_vectors_match() {
    let v = vectors();
    for group in v["parse_error"].as_array().unwrap() {
        let ty = group["type"].as_str().unwrap();
        for case in group["cases"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let want = case["error"].as_str().unwrap();
            match parse(ty, input) {
                Ok(m) => panic!("{ty} parse {input:?} should have failed, got {m}"),
                Err(e) => assert_eq!(e.code(), want, "{ty} parse {input:?} error code"),
            }
        }
    }
}

#[test]
fn render_vectors_match() {
    let v = vectors();
    for group in v["render"].as_array().unwrap() {
        let ty = group["type"].as_str().unwrap();
        for case in group["cases"].as_array().unwrap() {
            let micros = case["micros"].as_integer().unwrap();
            let want = case["text"].as_str().unwrap();
            assert_eq!(render(ty, micros), want, "{ty} render {micros}");
        }
    }
}

/// Every parse vector round-trips: rendering the parsed instant and re-parsing yields the
/// same micros (for the non-rounding, non-offset-discarding inputs this is exact; we assert
/// the weaker render∘parse == parse∘render∘parse, i.e. the instant is stable).
#[test]
fn parse_render_round_trip_is_stable() {
    let v = vectors();
    for group in v["parse"].as_array().unwrap() {
        let ty = group["type"].as_str().unwrap();
        for case in group["cases"].as_array().unwrap() {
            let micros = case["micros"].as_integer().unwrap();
            let text = render(ty, micros);
            let reparsed =
                parse(ty, &text).unwrap_or_else(|e| panic!("{ty} reparse {text:?}: {}", e.message));
            assert_eq!(reparsed, micros, "{ty} render→parse stable for {micros}");
        }
    }
}
