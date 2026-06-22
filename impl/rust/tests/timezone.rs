//! Cross-core time-zone contract: the Rust RFC 8536 reader + the `JTZ` bundle codec must reproduce
//! the byte-exact vectors in spec/tz/vectors/{tzif,bundle}.toml (CLAUDE.md §8; spec/tz/README.md
//! §3/§4). These vectors are the shared contract that guarantees Rust, Go, and TS compute identical
//! offsets and parse the bundle identically. Mirrors impl/go/timezone_test.go and
//! impl/ts/tests/timezone.test.ts.

use jed::timezone::{load_time_zone_data, offset_at_ref, open_bundle, resolve_zone, save_bundle};
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn bundle_bytes() -> Vec<u8> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/tz/fixtures/tzdata.jtz");
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn reader_matches_the_pinned_vectors() {
    load_time_zone_data(&bundle_bytes()).expect("load tzdata.jtz");
    let v: toml::Value = toml::from_str(&spec("tz/vectors/tzif.toml")).unwrap();
    let cases = v["case"].as_array().expect("[[case]] array");
    assert!(!cases.is_empty(), "no tzif vectors");
    for c in cases {
        let zone = c["zone"].as_str().unwrap();
        let inst = c["instant_micros"].as_integer().unwrap();
        let zr = resolve_zone(zone).unwrap_or_else(|| panic!("resolve {zone}"));
        let off = offset_at_ref(&zr, inst.div_euclid(1_000_000));
        assert_eq!(
            off.utoff as i64,
            c["utoff_secs"].as_integer().unwrap(),
            "{zone} @ {inst}: utoff"
        );
        assert_eq!(off.abbrev, c["abbrev"].as_str().unwrap(), "{zone} @ {inst}: abbrev");
        assert_eq!(off.is_dst, c["is_dst"].as_bool().unwrap(), "{zone} @ {inst}: is_dst");
    }
}

#[test]
fn bundle_matches_the_pinned_vectors() {
    let bytes = bundle_bytes();
    let parsed = open_bundle(&bytes).expect("open tzdata.jtz");
    let v: toml::Value = toml::from_str(&spec("tz/vectors/bundle.toml")).unwrap();
    let b = &v["bundle"];

    assert_eq!(parsed.tzdata_version, b["tzdata_version"].as_str().unwrap());

    let want_zones: Vec<&str> = b["zones"]
        .as_array()
        .unwrap()
        .iter()
        .map(|z| z.as_str().unwrap())
        .collect();
    let got_zones: Vec<&str> = parsed.zones.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(got_zones, want_zones, "zone manifest");

    let want_links: Vec<(&str, &str)> = b["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let a = l.as_array().unwrap();
            (a[0].as_str().unwrap(), a[1].as_str().unwrap())
        })
        .collect();
    let got_links: Vec<(&str, &str)> = parsed
        .links
        .iter()
        .map(|(a, t)| (a.as_str(), t.as_str()))
        .collect();
    assert_eq!(got_links, want_links, "link table");

    // The Open∘Save round-trip is byte-identical (§3).
    assert_eq!(save_bundle(&parsed), bytes, "bundle round-trip");
    assert!(b["roundtrip_byte_identical"].as_bool().unwrap());
}
