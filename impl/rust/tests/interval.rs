//! Cross-check: the Rust interval parser/renderer must reproduce the byte-exact vectors in
//! spec/encoding/intervals.toml (CLAUDE.md §8) — the parse/render/cascade arithmetic that must be
//! identical across the Rust/Go/TS cores. TOML is a test-time-only dependency.

use jed::interval::{Interval, parse_interval, render_interval};
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn vectors() -> toml::Value {
    toml::from_str(&spec("encoding/intervals.toml")).unwrap()
}

#[test]
fn parse_vectors_match() {
    let v = vectors();
    for group in v["parse"].as_array().unwrap() {
        for case in group["cases"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let months = case["months"].as_integer().unwrap() as i32;
            let days = case["days"].as_integer().unwrap() as i32;
            let micros = case["micros"].as_integer().unwrap();
            let got = parse_interval(input)
                .unwrap_or_else(|e| panic!("parse {input:?} failed: {}", e.message));
            assert_eq!(got.months, months, "parse {input:?} months");
            assert_eq!(got.days, days, "parse {input:?} days");
            assert_eq!(got.micros, micros, "parse {input:?} micros");
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
            match parse_interval(input) {
                Ok(iv) => panic!("parse {input:?} should have failed, got {iv:?}"),
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
            let iv = Interval {
                months: case["months"].as_integer().unwrap() as i32,
                days: case["days"].as_integer().unwrap() as i32,
                micros: case["micros"].as_integer().unwrap(),
            };
            let want = case["text"].as_str().unwrap();
            assert_eq!(render_interval(&iv), want, "render {iv:?}");
        }
    }
}

/// The canonical span: span-equal intervals compare equal and hash equal (the dedup contract),
/// while span order is the total ORDER BY order.
#[test]
fn span_is_canonical() {
    let one_month = parse_interval("1 mon").unwrap();
    let thirty_days = parse_interval("30 days").unwrap();
    let hours = parse_interval("720:00:00").unwrap();
    assert_eq!(one_month.span(), thirty_days.span());
    assert_eq!(one_month, thirty_days);
    assert_eq!(one_month, hours);
    // but the fields are distinct (render preserves them)
    assert_ne!(render_interval(&one_month), render_interval(&thirty_days));

    let day = parse_interval("1 day").unwrap();
    let two_days = parse_interval("2 days").unwrap();
    assert!(day < two_days);
    assert!(parse_interval("-1 day").unwrap() < day);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The order-preserving KEY body (`interval-span-i128`, encoding.md §2.10): the 16-byte i128
/// span (bias `2^127` + big-endian). Sorting by `encode_key` must equal span order; span-equal
/// intervals share a key (the "equal but not identical" UNIQUE wrinkle, decimal's 1.5/1.50);
/// byte-exact against the canonical vectors (spec/encoding/interval.toml + the §2.10 table).
#[test]
fn key_encoding_is_order_preserving() {
    let iv = |m: i32, d: i32, u: i64| Interval {
        months: m,
        days: d,
        micros: u,
    };
    // Ascending by span — sorting by key must reproduce this order (sign boundary, zero, ±µs).
    let ordered = [
        iv(-1200, 0, 0),      // -100 years
        iv(-1, 0, 0),         // -1 mon
        iv(0, -1, 0),         // -1 day
        iv(0, 0, -1_000_000), // -00:00:01
        iv(0, 0, -1),         // -0.000001 s
        iv(0, 0, 0),          // zero
        iv(0, 0, 1),          // 0.000001 s
        iv(0, 0, 1_000_000),  // 00:00:01
        iv(0, 1, 0),          // 1 day
        iv(1, 0, 0),          // 1 mon
        iv(1200, 0, 0),       // 100 years
    ];
    let mut by_key = ordered;
    by_key.sort_by(|a, b| a.encode_key().cmp(&b.encode_key()));
    let want: Vec<i128> = ordered.iter().map(|x| x.span()).collect();
    let got: Vec<i128> = by_key.iter().map(|x| x.span()).collect();
    assert_eq!(got, want, "encode_key order must equal span order");

    // Span-equal intervals share a key (1 mon == 30 days == 720:00:00) — the UNIQUE wrinkle.
    assert_eq!(iv(1, 0, 0).encode_key(), iv(0, 30, 0).encode_key());
    assert_eq!(
        iv(1, 0, 0).encode_key(),
        iv(0, 0, 30 * 86_400_000_000).encode_key()
    );

    // Byte-exact canonical vectors (the §2.10 worked-bytes table).
    assert_eq!(
        hex(&iv(0, 0, 0).encode_key()),
        "80000000000000000000000000000000"
    );
    assert_eq!(
        hex(&iv(0, 0, 1).encode_key()),
        "80000000000000000000000000000001"
    );
    assert_eq!(
        hex(&iv(0, 0, -1).encode_key()),
        "7fffffffffffffffffffffffffffffff"
    );
    assert_eq!(
        hex(&iv(0, 1, 0).encode_key()),
        "8000000000000000000000141dd76000"
    );
    assert_eq!(
        hex(&iv(1, 0, 0).encode_key()),
        "80000000000000000000025b7f3d4000"
    );
    assert_eq!(
        hex(&iv(0, -1, 0).encode_key()),
        "7fffffffffffffffffffffebe228a000"
    );
}
