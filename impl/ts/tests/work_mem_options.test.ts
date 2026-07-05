// OpenOptions.workMem === 0 means "the default budget" (256 MiB), NOT "unlimited" — the zero value must
// stay a safe finite budget so a bare open does not silently disable spill-to-disk. Unbounded /
// never-spill is reachable only at runtime via setWorkMem(0). This pins the options→session boundary
// that once diverged across cores (Go remapped 0→default; Rust/TS passed 0 through as unlimited).
// Host-API config surface + a deliberate cross-core alignment the corpus cannot express → a per-core
// unit test (CLAUDE.md §10). Mirrors impl/go/workmem_options_test.go and
// impl/rust/tests/work_mem_options.rs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { DEFAULT_WORK_MEM } from "../src/spill.ts";
import { close, create, open } from "../src/tooling.ts";

function seed(path: string): void {
  close(create(path, {})); // writes the initial durable image, then closes
}

test("OpenOptions.workMem = 0 is the default budget, not unlimited", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-workmem-"));
  try {
    const path = join(dir, "wm.jed");
    seed(path);

    // unset ⇒ default
    let db = open(path, {});
    assert.equal(db.session.workMem, DEFAULT_WORK_MEM);
    close(db);

    // explicit 0 ⇒ default (NOT unlimited) — the regression guard
    db = open(path, { workMem: 0 });
    assert.equal(db.session.workMem, DEFAULT_WORK_MEM);
    close(db);

    // explicit budget passes through
    db = open(path, { workMem: 1 << 20 });
    assert.equal(db.session.workMem, 1 << 20);
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("setWorkMem(0) reaches the unlimited (never-spill) budget at runtime", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-workmem-"));
  try {
    const path = join(dir, "wm.jed");
    seed(path);
    // options 0 ⇒ default; runtime 0 ⇒ unlimited — the unbounded budget stays reachable via the setter.
    const db = open(path, {});
    assert.equal(db.session.workMem, DEFAULT_WORK_MEM);
    db.setWorkMem(0);
    assert.equal(db.session.workMem, 0);
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
