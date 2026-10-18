// A closed page-service owner releases its storage engine before the event loop turns. Closing a
// MessagePort only takes it off the GC roots at a later turn, so a port whose listener reaches the
// engine kept every engine a synchronous run closed: the TS disk conformance walk, which reopens its
// database before every record, grew to ~9 GB. v8.queryObjects collects garbage and counts what is
// left, without ending the synchronous run.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import v8 from "node:v8";

import { Engine } from "../src/executor.ts";
import { createDatabase, openDatabase } from "../src/lib.ts";

const scratch = mkdtempSync(join(tmpdir(), "jed-pageservice-close-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function liveEngines(): number {
  return v8.queryObjects(Engine, { format: "count" }) as number;
}

test("pageservice close: a closed serving owner keeps no engine alive", () => {
  const path = join(scratch, "close.jed");
  createDatabase({ path, pageSize: 4096, skipFsync: true }).close();
  const before = liveEngines();
  // Each open is the database's serving owner. (A page client needs a second process: one process
  // shares one handle per file. Its close detaches its port the same way.)
  for (let i = 0; i < 10; i++) openDatabase(path, { skipFsync: true }).close();
  assert.ok(liveEngines() <= before, `${liveEngines()} engines alive after 10 closes`);
});
