//! The collation **builder tool** (spec/design/collation.md §4.1/§13, Slice 3b): assemble selected
//! compiled `.coll` tables into the shippable **`JUCD` bundle** — a shared DUCET **root** section,
//! per-locale tailoring **deltas** against it (root-sharing, §9/§5.1), and (when it lands, §16) the
//! Unicode **property/casing** section — with footprint **presets**. Build-time only: it is compiled
//! **out** of the production engine (which ships `OpenCollation` + the load-time merge + the executor,
//! §4.2). Deterministic — the bundle bytes are a §10 cross-core fixture: the cores LOAD the output and
//! the sort-key vectors + the on-disk golden round-trip pin it.
//!
//!   cargo run --release --bin build_collation_bundle -- [--preset non-cjk|everything|casing-only] [--out PATH]
//!
//! With no args it (re)writes the canonical production bundle `spec/collation/fixtures/unicode.jucd` at
//! the **`non-cjk`** preset — the common bundle the cores' tests/harnesses LOAD. Run it after the
//! compiler tool (`gen_collation_vectors`) regenerates the `.coll` set (a Unicode bump / a new
//! tailoring), then re-run the cores' suites. The builder READS the committed `.coll` artifacts (it
//! does not recompile the ~2.3 MB root), exactly the pipeline `ExtractHostCollation → CompileCollation
//! → SaveCollation(.coll) → builder(.coll → JUCD)` (§4.1).

use jed::collation::{
    Bundle, Collation, PropertyTable, Section, build_bundle, compile_casing, load_bundle,
    open_bundle, open_collation, save_bundle, serialize_table,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// One available collation (spec/design/collation.md §9/§13). `coll` is the committed `.coll`
/// artifact under `spec/collation/fixtures/`; `root` marks the shared DUCET root (exactly one);
/// `cjk` marks the Han tailoring the **non-CJK** preset drops (§13, the one footprint outlier).
/// Adding a tailoring is a data edit here. Today only the CLDR-DUCET root `unicode` + the Spanish
/// `es` tailoring are authored (the broader sv/da/de set needs deferred LDML features, §14).
struct Entry {
    coll: &'static str,
    root: bool,
    cjk: bool,
}

const REGISTRY: &[Entry] = &[
    Entry {
        coll: "unicode.coll",
        root: true,
        cjk: false,
    },
    Entry {
        coll: "es.coll",
        root: false,
        cjk: false,
    },
];

/// The footprint presets (spec/design/collation.md §13) — a *selection of sections*, chosen when the
/// bundle is produced and swappable without rebuilding the engine.
#[derive(Clone, Copy)]
enum Preset {
    /// property/casing section only (no collations) — lands with the property data in slice 3e (§16).
    CasingOnly,
    /// property + shared root + all non-CJK tailorings — the common bundle (`< ~1 MB`).
    NonCjk,
    /// non-CJK + the CJK (Han) tailoring (the single-digit-MB outlier).
    Everything,
}

fn collation_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/collation")
}

fn fixtures_dir() -> PathBuf {
    collation_dir().join("fixtures")
}

/// Read + deserialize a committed `.coll` artifact.
fn open_fixture(coll: &str) -> Collation {
    let path = fixtures_dir().join(coll);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    open_collation(&bytes).unwrap_or_else(|e| panic!("open {}: {}", path.display(), e.message))
}

/// Compile the committed casing source into the property section, plus the `@version` it pins (the
/// bundle header axis for a property-only `casing-only` bundle, §16). The source is the casing
/// analogue of the `.coll` set: a build-time input, never read by the production engine.
fn read_casing() -> (PropertyTable, String) {
    let path = collation_dir().join("17.0.0/casing.txt");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let version = text
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("@version")
                .map(|v| v.trim().to_string())
        })
        .unwrap_or_default();
    let prop = compile_casing(&text).unwrap_or_else(|e| panic!("compile casing: {}", e.message));
    (prop, version)
}

fn main() -> ExitCode {
    // --- args: --preset <name> (default non-cjk) and --out <path> (default the production fixture) ---
    let mut preset = Preset::NonCjk;
    let mut out: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--preset" => {
                preset = match args.next().as_deref() {
                    Some("casing-only") => Preset::CasingOnly,
                    Some("non-cjk") => Preset::NonCjk,
                    Some("everything") => Preset::Everything,
                    other => {
                        eprintln!(
                            "unknown --preset {other:?} (want casing-only|non-cjk|everything)"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--out" => match args.next() {
                Some(p) => out = Some(PathBuf::from(p)),
                None => {
                    eprintln!("--out needs a path");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("unknown argument {other:?}");
                return ExitCode::FAILURE;
            }
        }
    }
    // The property/casing section is the only content of `casing-only` and rides every other preset
    // too (§13/§16) — one `(unicode_version)` axis keeps casing and collation from mismatching.
    let (property, casing_version) = read_casing();

    if matches!(preset, Preset::CasingOnly) {
        // A property-only bundle: the casing tables, no collation root/tailorings (§13). Its header
        // version is the casing source's `@version`.
        let out = out.unwrap_or_else(|| fixtures_dir().join("casing.jucd"));
        let bundle = Bundle {
            unicode_version: casing_version,
            cldr_version: String::new(),
            description: String::new(),
            sections: vec![Section::Property(property.clone())],
        };
        let bytes = save_bundle(&bundle);
        // Self-check: open → load reproduces the property table.
        let (_colls, loaded) =
            load_bundle(&open_bundle(&bytes).expect("open_bundle")).expect("load");
        assert_eq!(
            loaded.as_ref(),
            Some(&property),
            "casing-only: loaded property table differs from the compiled one"
        );
        std::fs::write(&out, &bytes).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
        println!(
            "wrote {} ({} bytes): property/casing only ({} simple + {} special mappings)",
            out.display(),
            bytes.len(),
            property.simple.len(),
            property.special.len()
        );
        return ExitCode::SUCCESS;
    }

    let out = out.unwrap_or_else(|| fixtures_dir().join("unicode.jucd"));

    // Select root + tailorings for the preset, opening each from its committed `.coll`.
    let keep_cjk = matches!(preset, Preset::Everything);
    let root_entry = REGISTRY
        .iter()
        .find(|e| e.root)
        .expect("registry has a root collation");
    let root = open_fixture(root_entry.coll);
    let tailorings: Vec<Collation> = REGISTRY
        .iter()
        .filter(|e| !e.root && (keep_cjk || !e.cjk))
        .map(|e| open_fixture(e.coll))
        .collect();
    let refs: Vec<&Collation> = tailorings.iter().collect();

    // Assemble: a shared root + sparse per-locale deltas (build_bundle diffs each tailoring against
    // the root, §5.1). Empty description keeps the loaded collations' introspection identical to the
    // compiled tables (gen emits an empty description).
    let bundle = build_bundle(&root, &refs, Some(property), "");
    let bytes = save_bundle(&bundle);

    // Self-check the merge identity (§5.1): open → load → merge reproduces each full `.coll` table
    // byte-for-byte, so a host that loads this bundle gets exactly the committed tables.
    let (loaded, _property) =
        load_bundle(&open_bundle(&bytes).expect("open_bundle")).expect("load");
    for full in std::iter::once(&root).chain(tailorings.iter()) {
        let got = loaded
            .iter()
            .find(|c| c.name == full.name)
            .unwrap_or_else(|| panic!("loaded bundle missing {}", full.name));
        assert_eq!(
            serialize_table(got),
            serialize_table(full),
            "JUCD merge identity broken for {}",
            full.name
        );
    }

    std::fs::write(&out, &bytes).unwrap_or_else(|e| panic!("write {}: {e}", out.display()));
    let names: Vec<&str> = std::iter::once(root.name.as_str())
        .chain(tailorings.iter().map(|c| c.name.as_str()))
        .collect();
    println!(
        "wrote {} ({} bytes): root {} + tailorings [{}]",
        out.display(),
        bytes.len(),
        root.name,
        names[1..].join(", ")
    );
    ExitCode::SUCCESS
}
