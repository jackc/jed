//! Build-time tool (Rust-only, compiled out of the production engine — timezones.md §12): pack the
//! committed TZif source under `spec/tz/<version>/` into the shippable `JTZ` bundle
//! `spec/tz/fixtures/tzdata.jtz`. It reads `zones/**` (each file's path under `zones/` is its IANA
//! name), `links.tsv` (`alias<TAB>target`), and `VERSION`, sorts zones by name + links by alias, and
//! writes the bundle (spec/tz/README.md §3). It does **not** run `zic`; the TZif bytes are committed
//! source (§3.4). The other cores only *load* the bundle.

use jed::timezone::{TzBundle, save_bundle};
use std::path::{Path, PathBuf};

fn tz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/tz")
}

/// Recursively collect `(relative-name, bytes)` for every file under `root`.
fn collect_zones(root: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
    let mut entries: Vec<_> = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", root.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            collect_zones(&path, &rel, out);
        } else {
            let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            out.push((rel, bytes));
        }
    }
}

fn main() {
    let version = std::fs::read_to_string(tz_dir().join("2026a/VERSION"))
        .expect("read VERSION")
        .trim()
        .to_string();

    let mut zones = Vec::new();
    collect_zones(&tz_dir().join(format!("{version}/zones")), "", &mut zones);
    zones.sort_by(|a, b| a.0.cmp(&b.0));

    let links_raw = std::fs::read_to_string(tz_dir().join(format!("{version}/links.tsv")))
        .unwrap_or_default();
    let mut links: Vec<(String, String)> = links_raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split('\t');
            let alias = it.next().expect("link alias").trim().to_string();
            let target = it.next().expect("link target").trim().to_string();
            (alias, target)
        })
        .collect();
    links.sort_by(|a, b| a.0.cmp(&b.0));

    // Validate every link target is a present zone (a dangling link is a build error, §3).
    let names: std::collections::BTreeSet<&str> = zones.iter().map(|(n, _)| n.as_str()).collect();
    for (alias, target) in &links {
        assert!(
            names.contains(target.as_str()),
            "link {alias} -> {target}: target not in bundle"
        );
    }

    let bundle = TzBundle {
        tzdata_version: version.clone(),
        description: format!("jed tz starter set (IANA tzdata {version})"),
        zones,
        links,
    };
    let bytes = save_bundle(&bundle);

    let out = tz_dir().join("fixtures/tzdata.jtz");
    std::fs::create_dir_all(out.parent().unwrap()).expect("mkdir fixtures");
    std::fs::write(&out, &bytes).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
    eprintln!(
        "wrote {} ({} bytes): {} zones + {} links @ tzdata {}",
        out.display(),
        bytes.len(),
        bundle.zones.len(),
        bundle.links.len(),
        version
    );
}
