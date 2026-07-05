//! `OpenOptions.work_mem == 0` means "the default budget" (256 MiB), NOT "unlimited" — the zero value
//! must stay a safe finite budget so a bare `OpenOptions { work_mem: 0, .. }` does not silently disable
//! spill-to-disk. Unbounded / never-spill is reachable only at runtime via `set_work_mem(0)`. This pins
//! the options→session boundary that once diverged across cores (Go remapped `0`→default; Rust/TS passed
//! `0` through as unlimited). Host-API config surface + a deliberate cross-core alignment the corpus
//! cannot express → a per-core unit test (CLAUDE.md §10). Mirrors impl/go/workmem_options_test.go and
//! impl/ts/tests/work_mem_options.test.ts.

use std::path::PathBuf;

use jed::{CreateOptions, DEFAULT_WORK_MEM, Database, Engine, OpenOptions};

fn seed(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    // `create` writes the initial durable image immediately; the temporary handle drops → file closed.
    Database::create(CreateOptions {
        path: Some(path.clone()),
        ..Default::default()
    })
    .unwrap();
    path
}

#[test]
fn open_options_work_mem_zero_is_default_budget() {
    let path = seed("wm_boundary.jed");

    // unset ⇒ default
    let db = Engine::open_with_options(&path, OpenOptions::default()).unwrap();
    assert_eq!(db.session.work_mem, DEFAULT_WORK_MEM);

    // explicit 0 ⇒ default (NOT unlimited) — the regression guard
    let db = Engine::open_with_options(
        &path,
        OpenOptions {
            work_mem: 0,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(db.session.work_mem, DEFAULT_WORK_MEM);

    // explicit budget passes through
    let db = Engine::open_with_options(
        &path,
        OpenOptions {
            work_mem: 1 << 20,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(db.session.work_mem, 1 << 20);
}

#[test]
fn set_work_mem_zero_is_unlimited() {
    // The unbounded/never-spill budget is still reachable — just at runtime, via the setter, never as a
    // bare-options zero value. options 0 ⇒ default; runtime 0 ⇒ unlimited.
    let path = seed("wm_setter.jed");
    let mut db = Engine::open(&path).unwrap();
    assert_eq!(db.session.work_mem, DEFAULT_WORK_MEM);
    db.session.set_work_mem(0);
    assert_eq!(db.session.work_mem, 0);
}
