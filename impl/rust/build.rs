// Build script (stdlib only — not a dependency, CLAUDE.md §14): define CARGO_TARGET_TMPDIR for the
// crate compilation. Cargo only sets that variable for `tests/` integration-test targets, but the
// integration tests now compile IN-CRATE (Cargo autotests = false; src/integration_tests.rs), where
// the variable is absent — so the file-backed tests' `env!("CARGO_TARGET_TMPDIR")` would fail to
// compile. We point it at a writable per-build scratch dir and create it.
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("test-tmp");
    std::fs::create_dir_all(&dir).expect("create CARGO_TARGET_TMPDIR scratch dir");
    println!("cargo:rustc-env=CARGO_TARGET_TMPDIR={}", dir.display());
    println!("cargo:rerun-if-changed=build.rs");
}
