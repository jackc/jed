//! Cross-check: the Rust date parser/renderer must reproduce the byte-exact vectors in
//! spec/encoding/dates.toml (CLAUDE.md §8) — the parse/render arithmetic that must be identical
//! across the Rust/Go/TS cores. TOML is a test-time-only dependency.

use jed::date::{parse_date, render_date};
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn vectors() -> toml::Value {
    toml::from_str(&spec("encoding/dates.toml")).unwrap()
}

#[test]
fn parse_vectors_match() {
    let v = vectors();
    for group in v["parse"].as_array().unwrap() {
        for case in group["cases"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let want = case["days"].as_integer().unwrap();
            let got = parse_date(input)
                .unwrap_or_else(|e| panic!("parse {input:?} failed: {}", e.message));
            assert_eq!(got as i64, want, "parse {input:?}");
        }
    }
}

#[test]
fn parse_error_vectors_match() {
    let v = vectors();
    for group in v["parse_error"].as_array().unwrap() {
        for case in group["cases"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let want = case["error"].as_str().unwrap();
            match parse_date(input) {
                Ok(d) => panic!("parse {input:?} should have failed, got {d}"),
                Err(e) => assert_eq!(e.code(), want, "parse {input:?} error code"),
            }
        }
    }
}

#[test]
fn render_vectors_match() {
    let v = vectors();
    for group in v["render"].as_array().unwrap() {
        for case in group["cases"].as_array().unwrap() {
            let days = case["days"].as_integer().unwrap() as i32;
            let want = case["text"].as_str().unwrap();
            assert_eq!(render_date(days), want, "render {days}");
        }
    }
}

/// Every finite parse vector round-trips: render then re-parse yields the same day count
/// (a date carries no time/offset, so render→parse is exact for finite values and infinities).
#[test]
fn parse_render_round_trip_is_stable() {
    let v = vectors();
    for group in v["render"].as_array().unwrap() {
        for case in group["cases"].as_array().unwrap() {
            let days = case["days"].as_integer().unwrap() as i32;
            let text = render_date(days);
            let reparsed =
                parse_date(&text).unwrap_or_else(|e| panic!("reparse {text:?}: {}", e.message));
            assert_eq!(reparsed, days, "render→parse stable for {days}");
        }
    }
}
