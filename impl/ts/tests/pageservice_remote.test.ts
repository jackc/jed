// The stream transport's client (spec/pageservice/README.md §5.3) on what the real-process corpus
// cannot observe: read-ahead's round trips and the wording of a failed open. The transport here is a
// node one-liner relaying its stdio to a TS owner's endpoint (the relay half of `jed pages`), so the
// test needs no Rust serving end. The owner serves on its service worker thread, so the client's
// blocking waits on this thread do not stall it.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

import { canonicalOpenPath } from "../src/coordinator.ts";
import type { EngineError } from "../src/errors.ts";
import { readOwnerRecord } from "../src/pageservice/endpoint.ts";
import {
  createDatabase,
  type Database,
  intValue,
  openRemoteDatabase,
  pageServiceRoleForTesting,
  readPagesSentForTesting,
  type Value,
} from "../src/tooling.ts";

const scratch = mkdtempSync(join(tmpdir(), "jed-pageservice-remote-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

// RELAY copies stdin to the unix socket named by argv[1] and the socket to stdout.
const RELAY = `
const s = require("node:net").createConnection(process.argv[1]);
s.on("error", (e) => { process.stderr.write("relay: " + e.message + "\\n"); process.exit(1); });
process.stdin.pipe(s);
s.pipe(process.stdout);
s.on("close", () => process.exit(0));
process.stdin.on("end", () => s.end());
`;

let fixtures = 0;

// remoteFixture creates a served database of 2000 rows of 200 bytes and opens a stream client to it
// through the relay.
function remoteFixture(): { owner: Database; remote: Database; close: () => void } {
  const path = join(scratch, `remote-${++fixtures}.jed`);
  const owner = createDatabase({ path, pageSize: 4096, skipFsync: true });
  owner.run("CREATE TABLE t (id i64 PRIMARY KEY, v text)");
  owner.run("INSERT INTO t SELECT g, repeat('x', 200) FROM generate_series(1, 2000) g");
  const record = readOwnerRecord(canonicalOpenPath(path));
  if (!record?.serving) throw new Error("the owner wrote no serving owner record");
  const remote = openRemoteDatabase(
    [process.execPath, "-e", RELAY, record.endpoint],
    `remote:${path}`,
    { fileLockTimeoutMs: 5000 },
  );
  assert.equal(pageServiceRoleForTesting(remote), "client");
  return {
    owner,
    remote,
    close: () => {
      remote.close();
      owner.close();
    },
  };
}

// drain runs sql and returns its first row and its cost.
function drain(db: Database, sql: string): { first: Value[] | undefined; cost: bigint } {
  const rows = db.query(sql);
  let first: Value[] | undefined;
  for (const row of rows) first ??= row;
  return { first, cost: rows.cost };
}

test("remote: scans read ahead without changing cost", () => {
  const { owner, remote, close } = remoteFixture();
  try {
    const full = "SELECT sum(length(v))::i64 FROM t";
    const first = "SELECT id FROM t ORDER BY id LIMIT 1";
    const fullCost = drain(owner, full).cost;
    const firstCost = drain(owner, first).cost;

    let before = readPagesSentForTesting();
    assert.equal(drain(remote, first).cost, firstCost);
    assert.equal(readPagesSentForTesting() - before, 1, "LIMIT 1 fetches one leaf");

    before = readPagesSentForTesting();
    const scan = drain(remote, full);
    assert.equal(scan.cost, fullCost);
    assert.deepEqual(scan.first, [intValue(2000n * 200n)]);
    const leaves = Math.floor((2000 * 200) / 4096);
    const requests = readPagesSentForTesting() - before;
    assert.ok(requests * 4 < leaves, `${requests} read_pages requests for about ${leaves} leaves`);
  } finally {
    close();
  }
});

test("remote: a failing transport reports its standard error", () => {
  assert.throws(
    () =>
      openRemoteDatabase(
        ["sh", "-c", "echo 'ssh: connect to host nowhere: Connection refused' >&2; exit 255"],
        "remote:nowhere",
        { fileLockTimeoutMs: 300 },
      ),
    (error: EngineError) =>
      error.code() === "08006" && error.message.includes("Connection refused"),
  );
});

test("remote: a transport that cannot run fails the open naming it", () => {
  assert.throws(
    () =>
      openRemoteDatabase(["/nonexistent/jed-transport"], "remote:missing", {
        fileLockTimeoutMs: 300,
      }),
    (error: EngineError) =>
      error.code() === "08006" &&
      error.message.includes("could not run /nonexistent/jed-transport"),
  );
});
