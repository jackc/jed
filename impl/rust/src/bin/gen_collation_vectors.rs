//! Regeneration tool for the collation artifacts + cross-core byte vectors, produced from
//! the Rust core and cross-confirmed by Go/TS (CLAUDE.md §8). UCA sort keys are not safely
//! hand-authored (spec/collation/README.md §6), so the vectors are generated here and the case lists
//! below are their source of truth. Run after changing a fixture/source or a byte format:
//!   cargo run --release --bin gen_collation_vectors   — then re-run all three cores' suites.
//!
//! Two collation families (spec/design/collation.md §9/§14):
//!   * DEV fixtures (`dev-root`, `dev-nordic`) — tiny hand-authored definitions that exercise the
//!     compiler + executor (expansion, a tailoring, an astral code point) cheaply. They drive the
//!     `compiler.toml` vectors (the cross-core compiler contract, small enough to inline as hex) and
//!     their own sort-key vectors. NOT part of the production bundle.
//!   * The production `JUCD` bundle (`unicode.jucd`) — the DUCET root `unicode` once + the `es`
//!     tailoring as a sparse delta (README §5), the bytes a host LOADS via `db.LoadUnicodeData`
//!     (collation.md §4/§9, slice 3c). The per-collation `.coll` artifacts (`unicode.coll`/`es.coll`)
//!     are kept as the builder's intermediate + the golden unit (§4.1); the bundle is built from the
//!     same compiled tables and self-checks the load-time merge identity here. The tables are ~0.5 MB,
//!     far too large to inline in `compiler.toml`, so the bundle is pinned instead by (a) the cores
//!     reading it identically and (b) the sort-key vectors below (the executor contract, computed from
//!     the compiled table, which the loaded bundle must reproduce byte-for-byte — README §5.1).

use jed::collation::{
    Collation, build_bundle, compile_collation, load_bundle, open_bundle, save_bundle,
    save_collation, serialize_table, sort_key,
};
use std::path::Path;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn compile(def_files: &[&str], name: &str) -> Collation {
    let def: String = def_files
        .iter()
        .map(|f| spec(f))
        .collect::<Vec<_>>()
        .join("\n");
    compile_collation(name, &def).unwrap()
}

fn files_toml(files: &[&str]) -> String {
    files
        .iter()
        .map(|f| format!("\"{f}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn esc(s: &str) -> String {
    // TOML basic-string escaping for the values we use (only embedded NUL would need it; none do).
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn out_path(rel: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel)
}

fn main() {
    // (label, def_files, coll_name) — the small hand-authored compiler/executor fixtures.
    let dev: &[(&str, &[&str], &str)] = &[
        (
            "dev-root",
            &["collation/fixtures/dev-root.allkeys"],
            "dev-root",
        ),
        (
            "dev-nordic",
            &[
                "collation/fixtures/dev-root.allkeys",
                "collation/fixtures/dev-nordic.ldml",
            ],
            "dev-nordic",
        ),
    ];
    // (def_files, coll_name) — the real version-pinned production set (packed into unicode.jucd).
    let production: &[(&[&str], &str)] = &[
        (&["collation/17.0.0/root.allkeys"], "unicode"),
        (
            &["collation/17.0.0/root.allkeys", "collation/17.0.0/es.ldml"],
            "es",
        ),
    ];

    // --- write the per-collation .coll intermediates (§4.1) + the production JUCD bundle the cores
    //     LOAD via db.LoadUnicodeData (spec/design/collation.md §4/§9, slice 3c) ---
    // The `.coll` artifacts stay the golden unit / builder intermediate; the bundle ships the DUCET
    // root `unicode` once + the `es` tailoring as a sparse delta merged at load (README §5.1). No
    // property/casing section yet (slice 3e). An empty description keeps the loaded collations'
    // introspection identical to the compiled tables (compile_collation emits an empty description).
    let mut compiled: Vec<Collation> = Vec::new();
    for (files, name) in production {
        let coll = compile(files, name);
        let artifact = save_collation(&coll);
        std::fs::write(
            out_path(&format!("collation/fixtures/{name}.coll")),
            &artifact,
        )
        .unwrap();
        compiled.push(coll);
    }
    {
        let root = compiled
            .iter()
            .find(|c| c.name == "unicode")
            .expect("unicode root");
        let tailorings: Vec<&Collation> = compiled.iter().filter(|c| c.name != "unicode").collect();
        let bundle = build_bundle(root, &tailorings, None, "");
        let bytes = save_bundle(&bundle);
        // self-check: open → load → merge reproduces each full `.coll` table byte-identically
        // (README §5.1), so a stale bundle is caught at generation, not only by the cores' vectors.
        let (colls, _property) = load_bundle(&open_bundle(&bytes).unwrap()).unwrap();
        for full in &compiled {
            let loaded = colls.iter().find(|c| c.name == full.name).unwrap();
            assert_eq!(
                serialize_table(loaded),
                serialize_table(full),
                "JUCD merge identity broken for {}",
                full.name
            );
        }
        std::fs::write(out_path("collation/fixtures/unicode.jucd"), &bytes).unwrap();
    }

    // --- compiler.toml — DEV fixtures only (small enough to pin as full hex) ---
    let mut out = String::new();
    out.push_str(
        "# Collation compiler vectors — (definition fixtures) → (compiled table §2 / .coll\n",
    );
    out.push_str(
        "# artifact §3) bytes. GENERATED by impl/rust/src/bin/gen_collation_vectors.rs;\n",
    );
    out.push_str(
        "# cross-confirmed byte-for-byte by all three cores (CLAUDE.md §8). Do not hand-edit.\n",
    );
    out.push_str(
        "# Format: spec/collation/README.md §2/§3. def_files are concatenated (newline-joined)\n",
    );
    out.push_str(
        "# then compiled under coll_name. Only the small DEV fixtures are pinned here; the real\n",
    );
    out.push_str(
        "# production tables (unicode/es) are ~0.5 MB and are pinned by the cores loading the JUCD\n",
    );
    out.push_str("# bundle + the sort-key vectors instead (spec/design/collation.md §9/§10).\n\n");
    out.push_str("schema_version = 1\n");
    for (label, files, name) in dev {
        let coll = compile(files, name);
        let table = serialize_table(&coll);
        let artifact = save_collation(&coll);
        out.push_str("\n[[compiler]]\n");
        out.push_str(&format!("name = \"{label}\"\n"));
        out.push_str(&format!("coll_name = \"{name}\"\n"));
        out.push_str(&format!("def_files = [{}]\n", files_toml(files)));
        out.push_str(&format!("table_hex = \"{}\"\n", hex(&table)));
        out.push_str(&format!("artifact_hex = \"{}\"\n", hex(&artifact)));
    }
    std::fs::write(out_path("collation/vectors/compiler.toml"), &out).unwrap();

    // --- sortkey.toml — DEV fixtures + the real production collations ---
    // Strings chosen per collation; the harness also asserts they appear in ascending sort-key order,
    // so they are emitted (below) in true collation order. The real-collation cases double as the
    // executor cross-core contract over the version-pinned table.
    let dev_cases: &[(&str, &[&str])] = &[
        (
            "dev-root",
            &[
                " ", "a", "A", "ä", "Ä", "b", "B", "z", "Z", "😀", "aa", "ab", "az", "a😀",
            ],
        ),
        (
            "dev-nordic",
            &[" ", "a", "A", "b", "B", "z", "Z", "ä", "Ä", "😀"],
        ),
    ];
    // unicode (root): letters order by DUCET primary (ä near a, ñ near n), astral sorts low.
    // es: ñ is a distinct PRIMARY letter after n (n < N < nz < ñ < Ñ < ña < o) — the Spanish contrast.
    let real_cases: &[(&[&str], &str, &[&str])] = &[
        (
            &["collation/17.0.0/root.allkeys"],
            "unicode",
            &[
                "a", "A", "ä", "b", "z", "Z", "é", "n", "ñ", "o", "😀", "ña", "nz",
            ],
        ),
        (
            &["collation/17.0.0/root.allkeys", "collation/17.0.0/es.ldml"],
            "es",
            &["a", "n", "N", "nz", "ñ", "Ñ", "ña", "o", "z"],
        ),
    ];

    let mut out = String::new();
    out.push_str("# Collation executor (sort-key) vectors — (collation, string) → (sort-key §4)\n");
    out.push_str("# bytes, the primary cross-core contract for the algorithm. GENERATED by\n");
    out.push_str(
        "# impl/rust/src/bin/gen_collation_vectors.rs; cross-confirmed by all three cores\n",
    );
    out.push_str(
        "# (CLAUDE.md §8). Do not hand-edit. Format: spec/collation/README.md §4. Within one\n",
    );
    out.push_str(
        "# collation the entries are in ascending sort order, so the harness also checks the\n",
    );
    out.push_str(
        "# sort keys' memcmp order matches. Includes an astral case (😀, U+1F600) — the TS\n",
    );
    out.push_str(
        "# UTF-16-vs-code-point trap (types.md §11). The `unicode`/`es` cases are over the real\n",
    );
    out.push_str(
        "# version-pinned production table; the harness resolves them via the loaded JUCD bundle.\n\n",
    );
    out.push_str("schema_version = 1\n");

    let mut emit = |coll: &Collation, files: &[&str], name: &str, strings: &[&str]| {
        let mut keyed: Vec<(&str, Vec<u8>)> = strings
            .iter()
            .map(|s| (*s, sort_key(coll, s).unwrap()))
            .collect();
        keyed.sort_by(|a, b| a.1.cmp(&b.1));
        for (s, key) in keyed {
            out.push_str("\n[[sortkey]]\n");
            out.push_str(&format!("coll_name = \"{name}\"\n"));
            out.push_str(&format!("def_files = [{}]\n", files_toml(files)));
            out.push_str(&format!("string = \"{}\"\n", esc(s)));
            out.push_str(&format!("sortkey_hex = \"{}\"\n", hex(&key)));
        }
    };
    for (name, strings) in dev_cases {
        let files = dev.iter().find(|(_, _, n)| n == name).unwrap().1;
        let coll = compile(files, name);
        emit(&coll, files, name, strings);
    }
    for (files, name, strings) in real_cases {
        let coll = compile(files, name);
        emit(&coll, files, name, strings);
    }
    std::fs::write(out_path("collation/vectors/sortkey.toml"), &out).unwrap();

    // --- bundle.toml — the JUCD bundle cross-core contract (spec/collation/README.md §5) ---
    // A small DEV bundle: dev-root as the shared root + dev-nordic as a tailoring delta. Pinned as
    // full hex (small). Each core's harness rebuilds it from def_files (build_bundle: shared root +
    // per-locale deltas), asserts save_bundle == bundle_hex, round-trips open_bundle, and checks the
    // load-time merge reproduces the full tailoring table (§5.1). No property section here — it
    // cannot be rebuilt from def_files; the property codec is unit-tested until casing data lands (3e).
    {
        let root_files: &[&str] = &["collation/fixtures/dev-root.allkeys"];
        let nordic_files: &[&str] = &[
            "collation/fixtures/dev-root.allkeys",
            "collation/fixtures/dev-nordic.ldml",
        ];
        let root = compile(root_files, "dev-root");
        let nordic = compile(nordic_files, "dev-nordic");
        let bundle = build_bundle(&root, &[&nordic], None, "");
        let bytes = save_bundle(&bundle);

        let mut out = String::new();
        out.push_str(
            "# Collation JUCD bundle vectors (spec/collation/README.md §5). GENERATED by\n",
        );
        out.push_str(
            "# impl/rust/src/bin/gen_collation_vectors.rs; cross-confirmed byte-for-byte by all\n",
        );
        out.push_str(
            "# three cores (CLAUDE.md §8). Do not hand-edit. Each core's harness rebuilds the bundle\n",
        );
        out.push_str(
            "# from the def_files (build_bundle: a shared root + per-locale deltas), asserts\n",
        );
        out.push_str(
            "# save_bundle == bundle_hex, round-trips open_bundle, and checks the load-time merge\n",
        );
        out.push_str(
            "# reproduces each full tailoring table (§5.1). Flat layout (the mini TOML readers take\n",
        );
        out.push_str(
            "# only scalars + string arrays): tailoring_def_files joins each tailoring's files with\n",
        );
        out.push_str(
            "# '|'. DEV fixtures only — small enough to pin as full hex. No property section here\n",
        );
        out.push_str(
            "# (it can't be rebuilt from def_files; the property codec is unit-tested until 3e).\n\n",
        );
        out.push_str("schema_version = 1\n\n");
        out.push_str("[[bundle]]\n");
        out.push_str("name = \"dev\"\n");
        out.push_str("description = \"\"\n");
        out.push_str("root_name = \"dev-root\"\n");
        out.push_str(&format!("root_def_files = [{}]\n", files_toml(root_files)));
        out.push_str("tailoring_names = [\"dev-nordic\"]\n");
        out.push_str(&format!(
            "tailoring_def_files = [\"{}\"]\n",
            nordic_files.join("|")
        ));
        out.push_str(&format!("bundle_hex = \"{}\"\n", hex(&bytes)));
        std::fs::write(out_path("collation/vectors/bundle.toml"), &out).unwrap();
    }

    println!(
        "wrote spec/collation/fixtures/{{unicode,es}}.coll + unicode.jucd + vectors/{{compiler,sortkey,bundle}}.toml"
    );
}
