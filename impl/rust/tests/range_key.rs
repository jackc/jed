//! Cross-check: the Rust `range` key codec (`encode_range_key`, spec/design/encoding.md §2.11) must
//! produce the byte-exact, order-preserving vectors the Go/TS cores and the Ruby reference reproduce
//! (CLAUDE.md §8). Range is the first *container* key — empty/±∞/inclusivity framing around the
//! element's own key — so this pins both the structural layout and the `memcmp == range_total_cmp`
//! ordering. The behavioral side (a range PRIMARY KEY/index/UNIQUE/FK actually works) lives in
//! types/range.test; this is the encoding contract.

use jed::range::encode_range_key;
use jed::types::ScalarType;
use jed::value::{RangeVal, Value};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A canonical i32range from optional finite bounds (discrete `[)` form: lower inclusive, upper
/// exclusive — what the engine stores). `None` is an infinite bound.
fn i32r(lo: Option<i32>, hi: Option<i32>) -> RangeVal {
    RangeVal {
        empty: false,
        lower: lo.map(|n| Box::new(Value::Int(n as i64))),
        upper: hi.map(|n| Box::new(Value::Int(n as i64))),
        lower_inc: lo.is_some(),
        upper_inc: false,
    }
}

fn enc(rv: &RangeVal) -> Vec<u8> {
    encode_range_key(ScalarType::Int32, rv)
}

/// The byte layout is exactly the §2.11 worked vectors (i32 element = 4-byte `int-be-signflip`).
#[test]
fn i32range_byte_exact() {
    assert_eq!(hex(&enc(&RangeVal::empty())), "00");
    // (,5): non-empty, lower −∞ (00), upper finite 5 exclusive
    assert_eq!(hex(&enc(&i32r(None, Some(5)))), "0100018000000500");
    // (,): both unbounded — lower −∞ (00), upper +∞ (02)
    assert_eq!(hex(&enc(&i32r(None, None))), "010002");
    // [1,5): the §2.11 worked example
    assert_eq!(
        hex(&enc(&i32r(Some(1), Some(5)))),
        "01018000000100018000000500"
    );
    // [2,): lower 2 inclusive, upper +∞
    assert_eq!(hex(&enc(&i32r(Some(2), None))), "0101800000020002");
}

/// `memcmp` over the keys reproduces `range_total_cmp`: empty first, then by lower bound (−∞ first),
/// then by upper bound (+∞ last), with the inclusivity tie-break.
#[test]
fn order_preserving() {
    // a strictly ascending sequence under range_total_cmp
    let ranges = [
        RangeVal::empty(),       // empty sorts below all
        i32r(None, Some(5)),     // (,5)   lower −∞, upper 5
        i32r(None, None),        // (,)    lower −∞, upper +∞
        i32r(Some(1), Some(5)),  // [1,5)
        i32r(Some(1), Some(10)), // [1,10)
        i32r(Some(2), Some(4)),  // [2,4)
        i32r(Some(2), None),     // [2,)   lower 2, upper +∞
    ];
    let keys: Vec<Vec<u8>> = ranges.iter().map(enc).collect();
    for w in keys.windows(2) {
        assert!(
            w[0] < w[1],
            "keys must be strictly ascending: {} !< {}",
            hex(&w[0]),
            hex(&w[1])
        );
    }
}

/// Inclusivity is significant on continuous ranges (numrange = decimal element): equal bounds with
/// different brackets get distinct keys, ordered as PG `range_cmp_bounds` ranks them — an inclusive
/// lower before an exclusive lower, an exclusive upper before an inclusive upper.
#[test]
fn inclusivity_tiebreak() {
    let dec = |digits: &str, scale: u32| {
        Box::new(Value::Decimal(jed::decimal::Decimal::from_digits_scale(
            false, digits, scale,
        )))
    };
    let numr = |lo_inc: bool, hi_inc: bool| RangeVal {
        empty: false,
        lower: Some(dec("1", 0)),
        upper: Some(dec("2", 0)),
        lower_inc: lo_inc,
        upper_inc: hi_inc,
    };
    let enc_n = |rv: &RangeVal| encode_range_key(ScalarType::Decimal, rv);
    // [1,2) < (1,2)  (inclusive lower sorts before exclusive lower)
    assert!(enc_n(&numr(true, false)) < enc_n(&numr(false, false)));
    // (1,2) < (1,2]  (exclusive upper sorts before inclusive upper)
    assert!(enc_n(&numr(false, false)) < enc_n(&numr(false, true)));
}

/// Decimal scale-independence propagates to a numrange bound: `[1.5,…` and `[1.50,…` share a key
/// (the §2.5 decimal-key wrinkle, the analogue of interval span-equality).
#[test]
fn decimal_bound_scale_independence() {
    let dec = |digits: &str, scale: u32| {
        Box::new(Value::Decimal(jed::decimal::Decimal::from_digits_scale(
            false, digits, scale,
        )))
    };
    let r = |lo: Box<Value>| RangeVal {
        empty: false,
        lower: Some(lo),
        upper: Some(dec("2", 0)),
        lower_inc: true,
        upper_inc: false,
    };
    // 1.5 (digits "15", scale 1) and 1.50 (digits "150", scale 2) are equal → same key
    assert_eq!(
        encode_range_key(ScalarType::Decimal, &r(dec("15", 1))),
        encode_range_key(ScalarType::Decimal, &r(dec("150", 2))),
    );
}
