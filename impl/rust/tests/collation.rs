//! Cross-core collation contract: the Rust compiler + executor + artifact codec must reproduce the
//! byte-exact vectors in spec/collation/vectors/{compiler,sortkey}.toml (CLAUDE.md §8;
//! spec/collation/README.md §2/§3/§4). These vectors are the shared contract that guarantees Rust,
//! Go, and TS emit identical compiled tables, sort keys, and `.coll` artifacts. Mirrors
//! impl/go/collation_test.go and impl/ts/tests/collation.test.ts.

use jed::collation::{
    Collation, compile_collation, open_collation, save_collation, serialize_table, sort_key,
    vendored_collation,
};
use std::path::Path;
use std::sync::Arc;

fn spec(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Concatenate the def_files (newline-joined) the way the generator does.
fn definition(files: &[toml::Value]) -> String {
    files
        .iter()
        .map(|f| spec(f.as_str().unwrap()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn compiler_matches_the_pinned_vectors() {
    let v: toml::Value = toml::from_str(&spec("collation/vectors/compiler.toml")).unwrap();
    let vectors = v["compiler"].as_array().expect("[[compiler]] array");
    assert!(!vectors.is_empty(), "no compiler vectors");
    for vec in vectors {
        let name = vec["name"].as_str().unwrap();
        let coll_name = vec["coll_name"].as_str().unwrap();
        let def = definition(vec["def_files"].as_array().unwrap());

        let coll = compile_collation(coll_name, &def).unwrap();
        assert_eq!(
            hex(&serialize_table(&coll)),
            vec["table_hex"].as_str().unwrap(),
            "{name}: table"
        );

        let artifact = save_collation(&coll);
        let artifact_hex = vec["artifact_hex"].as_str().unwrap();
        assert_eq!(hex(&artifact), artifact_hex, "{name}: artifact");

        // round-trip: open the artifact, re-save, get identical bytes; the reopened collation
        // equals the compiled one.
        let reopened = open_collation(&artifact).unwrap();
        assert_eq!(reopened, coll, "{name}: open == compiled");
        assert_eq!(
            hex(&save_collation(&reopened)),
            artifact_hex,
            "{name}: open→save round-trip"
        );
    }
}

#[test]
fn sortkey_matches_vectors_and_is_strictly_ascending() {
    let v: toml::Value = toml::from_str(&spec("collation/vectors/sortkey.toml")).unwrap();
    let vectors = v["sortkey"].as_array().expect("[[sortkey]] array");
    assert!(!vectors.is_empty(), "no sortkey vectors");

    let mut last_coll = String::new();
    let mut coll: Option<Arc<Collation>> = None;
    let mut prev_key: Option<Vec<u8>> = None;

    for vec in vectors {
        let coll_name = vec["coll_name"].as_str().unwrap();
        let s = vec["string"].as_str().unwrap();
        let expected = vec["sortkey_hex"].as_str().unwrap();

        if coll_name != last_coll {
            // The real version-pinned collations (`unicode`, `es`) are resolved from the embedded
            // `.coll` — the production read path — rather than recompiling their ~2.3 MB source. The
            // small dev fixtures (not vendored) are compiled from their definition files.
            coll = Some(vendored_collation(coll_name).unwrap_or_else(|| {
                let def = definition(vec["def_files"].as_array().unwrap());
                Arc::new(compile_collation(coll_name, &def).unwrap())
            }));
            last_coll = coll_name.to_string();
            prev_key = None;
        }
        let key = sort_key(coll.as_ref().unwrap(), s).unwrap();
        assert_eq!(hex(&key), expected, "{coll_name} {s:?}: sort key");

        if let Some(prev) = &prev_key {
            assert!(
                prev < &key,
                "{coll_name}: {s:?} must sort strictly after the previous entry"
            );
        }
        prev_key = Some(key);
    }
}

#[test]
fn open_rejects_a_tampered_artifact() {
    let coll = compile_collation("dev-root", &spec("collation/fixtures/dev-root.allkeys")).unwrap();
    let mut artifact = save_collation(&coll);
    // flip a byte inside the compressed table region (past the fixed-size header/strings).
    let n = artifact.len();
    artifact[n - 1] ^= 0xFF;
    let err = open_collation(&artifact).unwrap_err();
    assert_eq!(
        err.code(),
        "XX001",
        "tampered artifact must be data_corrupted"
    );
}
