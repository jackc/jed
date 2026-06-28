//! Build-time tool (Rust-only — timezones.md §12): generate the cross-core time-zone vectors
//! `spec/tz/vectors/{tzif,bundle}.toml` from the committed bundle `spec/tz/fixtures/tzdata.jtz`. The
//! Rust core is the source of truth; Go and TS cross-confirm byte-for-byte (the collation-vectors
//! precedent, CLAUDE.md §8). `tzif.toml` pins the reader `(zone, instant) → (offset, abbrev, dst)`
//! (§4); `bundle.toml` pins the parsed manifest + the `Open`∘`Save` round-trip (§3). Do not hand-edit.

use jed::tooling::timezone::{
    load_time_zone_data, offset_at_ref, open_bundle, resolve_zone, save_bundle,
};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn tz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/tz")
}

const SECS_PER_DAY: i64 = 86_400;

/// days since 1970-01-01 for a civil date (the §2.4/timestamp calendar; duplicated tiny here so the
/// generator needs no engine-internal access).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn micros(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    (days_from_civil(y, mo, d) * SECS_PER_DAY + h * 3600 + mi * 60 + s) * 1_000_000
}

fn main() {
    let bundle_bytes =
        std::fs::read(tz_dir().join("fixtures/tzdata.jtz")).expect("read tzdata.jtz");
    load_time_zone_data(&bundle_bytes).expect("load bundle");

    // (zone, y, mo, d, H, M, S, note) — the §6 corner cases. Instants are UTC.
    let cases: &[(&str, i64, i64, i64, i64, i64, i64, &str)] = &[
        ("Etc/UTC", 2024, 1, 15, 12, 0, 0, "fixed UTC"),
        ("America/New_York", 2024, 1, 15, 12, 0, 0, "EST (standard)"),
        ("America/New_York", 2024, 7, 15, 12, 0, 0, "EDT (daylight)"),
        (
            "America/New_York",
            2024,
            3,
            10,
            6,
            59,
            59,
            "1s before spring-forward → EST",
        ),
        (
            "America/New_York",
            2024,
            3,
            10,
            7,
            0,
            0,
            "spring-forward boundary → EDT",
        ),
        (
            "America/New_York",
            1800,
            1,
            1,
            0,
            0,
            0,
            "before first transition (LMT)",
        ),
        (
            "America/New_York",
            2099,
            7,
            1,
            12,
            0,
            0,
            "past last transition → footer EDT",
        ),
        (
            "America/New_York",
            2099,
            1,
            1,
            12,
            0,
            0,
            "past last transition → footer EST",
        ),
        (
            "Asia/Kolkata",
            2024,
            1,
            15,
            12,
            0,
            0,
            "IST +05:30 (non-hour, no DST)",
        ),
        ("Asia/Kolkata", 2099, 1, 15, 12, 0, 0, "IST +05:30 footer"),
        (
            "Australia/Lord_Howe",
            2024,
            1,
            15,
            12,
            0,
            0,
            "southern summer → +11:00 DST",
        ),
        (
            "Australia/Lord_Howe",
            2024,
            7,
            15,
            12,
            0,
            0,
            "southern winter → +10:30 std",
        ),
        (
            "Australia/Lord_Howe",
            2099,
            1,
            15,
            12,
            0,
            0,
            "footer southern summer (start>end)",
        ),
        (
            "US/Eastern",
            2024,
            1,
            15,
            12,
            0,
            0,
            "alias → America/New_York EST",
        ),
        ("+05:30", 2024, 1, 15, 12, 0, 0, "fixed offset +05:30"),
        ("-08:00", 2024, 1, 15, 12, 0, 0, "fixed offset -08:00"),
    ];

    let mut tzif = String::new();
    tzif.push_str(
        "# Time-zone reader vectors — (zone, instant) → (utoff_secs, abbrev, is_dst), the\n\
         # primary cross-core contract for the RFC 8536 reader (spec/tz/README.md §4). GENERATED\n\
         # by impl/rust/src/bin/gen_timezone_vectors.rs; cross-confirmed by all three cores\n\
         # (CLAUDE.md §8). Do not hand-edit. Each core loads spec/tz/fixtures/tzdata.jtz, resolves\n\
         # the zone, and asserts offset_at(instant). Covers standard/daylight, a transition\n\
         # boundary, before-first/after-last (footer), a non-hour offset, a sub-hour DST step, a\n\
         # southern (start>end) footer, an alias, and built-in fixed offsets.\n\n\
         schema_version = 1\n",
    );
    for (zone, y, mo, d, h, mi, s, note) in cases {
        let inst = micros(*y, *mo, *d, *h, *mi, *s);
        let zr = resolve_zone(zone).unwrap_or_else(|| panic!("resolve {zone}"));
        let off = offset_at_ref(&zr, inst.div_euclid(1_000_000));
        write!(
            tzif,
            "\n[[case]]\n# {note}\nzone = {:?}\ninstant_micros = {inst}\nutoff_secs = {}\nabbrev = {:?}\nis_dst = {}\n",
            zone, off.utoff, off.abbrev, off.is_dst
        )
        .unwrap();
    }
    std::fs::create_dir_all(tz_dir().join("vectors")).expect("mkdir vectors");
    std::fs::write(tz_dir().join("vectors/tzif.toml"), &tzif).expect("write tzif.toml");

    // bundle.toml: the parsed manifest + the round-trip identity (save(open(bytes)) == bytes).
    let parsed = open_bundle(&bundle_bytes).expect("open bundle");
    let roundtrip = save_bundle(&parsed) == bundle_bytes;
    assert!(roundtrip, "bundle round-trip is not byte-identical");
    let mut b = String::new();
    b.push_str(
        "# JTZ bundle vectors — the parsed manifest + the Open∘Save byte-identity (spec/tz/README.md\n\
         # §3). GENERATED by gen_timezone_vectors.rs; cross-confirmed by all three cores. The harness\n\
         # opens spec/tz/fixtures/tzdata.jtz, checks these fields, and asserts save(open(bytes)) == bytes.\n\
         # Links are flat \"alias=target\" strings so every core's minimal TOML reader handles them.\n\n\
         schema_version = 1\n\n[[bundle]]\n",
    );
    write!(b, "tzdata_version = {:?}\n", parsed.tzdata_version).unwrap();
    write!(b, "roundtrip_byte_identical = {roundtrip}\n").unwrap();
    let names: Vec<String> = parsed.zones.iter().map(|(n, _)| format!("{n:?}")).collect();
    write!(b, "zones = [{}]\n", names.join(", ")).unwrap();
    let links: Vec<String> = parsed
        .links
        .iter()
        .map(|(a, t)| format!("{:?}", format!("{a}={t}")))
        .collect();
    write!(b, "links = [{}]\n", links.join(", ")).unwrap();
    std::fs::write(tz_dir().join("vectors/bundle.toml"), &b).expect("write bundle.toml");

    eprintln!(
        "wrote spec/tz/vectors/{{tzif,bundle}}.toml ({} cases)",
        cases.len()
    );
}
