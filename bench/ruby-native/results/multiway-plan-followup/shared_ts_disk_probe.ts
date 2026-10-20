// Untimed diagnosis of Node page-service startup under an address-space guard.
// Use the same default auto locking as the shared disk conformance harness.
import { appendFileSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createDatabase, openDatabase, queryOutcome } from "../../../../impl/ts/src/tooling.ts";

const report = process.argv[2]!;
const directory = mkdtempSync(join(tmpdir(), "multiway-ts-disk-"));
const path = join(directory, "fixture.jed");
const observe = (phase: string) => {
  appendFileSync(report, JSON.stringify({ phase, pid: process.pid, at: new Date().toISOString(),
    memory: process.memoryUsage(), status: readFileSync("/proc/self/status", "utf8") }) + "\n");
};
observe("before-create");
let database = createDatabase({ path, skipFsync: true, locking: "auto" });
observe("after-create");
try {
  for (let index = 0; index < 3; index++) {
    database.close();
    observe(`closed-${index}`);
    database = openDatabase(path, { skipFsync: true, locking: "auto" });
    observe(`opened-${index}`);
    const session = database.session();
    try {
      const result = queryOutcome(session, "SELECT 1");
      if (result.kind !== "query" || result.rows.length !== 1) throw new Error("unexpected scalar result");
    } finally { session.close(); }
  }
} finally {
  database.close();
  rmSync(directory, { recursive: true, force: true });
}
observe("finished");
