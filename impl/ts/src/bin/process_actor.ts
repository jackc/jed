// Test-only line actor for the shared real-process corpus (concurrency-testing.md §10).

import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createInterface } from "node:readline";
import process from "node:process";

import {
  createDatabase,
  type Database,
  EngineError,
  type Environment,
  EnvironmentBuilder,
  attachFile,
  intValue,
  openDatabase,
  render,
  type Session,
} from "../tooling.ts";

const [action, path, timeoutText] = process.argv.slice(2);
if (action === undefined || path === undefined) {
  throw new Error("usage: process_actor.ts create|open PATH [timeout_ms] | owner-idle REPO_ROOT");
}
if (action === "owner-idle") {
  // The corpus-through-a-client driver (spec/pageservice/README.md §7): open and close databases as
  // their page-service owner on command.
  await ownerIdle(path);
  process.exit(0);
}
const fileLockTimeoutMs = timeoutText === undefined ? 5000 : Number(timeoutText);
// create-service / open-service use the test-only page-service mode (spec/design/page-service.md §16).
const locking =
  action === "open-exclusive"
    ? "exclusive"
    : action === "create-service" || action === "open-service"
      ? "service"
      : "shared";

let database: Database;
try {
  database =
    action === "create" || action === "create-service"
      ? createDatabase({ path, locking, fileLockTimeoutMs })
      : openDatabase(path, { locking, fileLockTimeoutMs });
} catch (error) {
  replyError(error);
  process.exit(1);
}

console.log("READY");
let reader: Session | null = null;
let writer: Session | null = null;

for await (const line of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
  const tab = line.indexOf("\t");
  const command = tab < 0 ? line : line.slice(0, tab);
  const argument = tab < 0 ? "" : line.slice(tab + 1);
  try {
    let value = "";
    switch (command) {
      case "EXEC":
        database.execute(sql(argument));
        break;
      case "ATTACH": {
        const [name, readOnly, attachmentPath] = argument.split("\t", 3);
        database.attach(name!, attachFile(attachmentPath!), readOnly === "1");
        break;
      }
      case "QUERY_I64":
        value = queryI64(database, sql(argument));
        break;
      case "READ_OPEN":
        reader = database.readSession();
        break;
      case "READ_QUERY_I64":
        value = queryI64(reader!, sql(argument));
        break;
      case "READ_CLOSE":
        reader?.close();
        reader = null;
        break;
      case "WRITE_OPEN":
        writer = database.session();
        writer.lockTimeoutMs = Number(argument || 0);
        writer.begin(true);
        break;
      case "WRITE_EXEC":
        writer!.execute(sql(argument));
        break;
      case "WRITE_COMMIT":
        writer!.commit();
        break;
      case "WRITE_ROLLBACK":
        writer!.rollback();
        break;
      case "COMPACT":
        database.compact(argument || "main");
        break;
      case "MAX_STORAGE_BYTES":
        database.setMaxStorageBytes("main", BigInt(argument));
        break;
      case "STORAGE_BYTES":
        value = database.storageBytes("main").toString();
        break;
      case "TIME_LOOKUPS":
      case "TIME_SCAN":
      case "TIME_COMMITS":
        // The page-service benchmark's timed loops (spec/design/benchmarks.md §12).
        value = timed(database, command, argument);
        break;
      case "TXID":
        value = database.txid.toString();
        break;
      case "PAGE_COUNT":
        value = database.pageCount.toString();
        break;
      case "CLOSE":
        reader?.close();
        writer?.close();
        database.close();
        replyOK("");
        process.exit(0);
        break;
      default:
        throw new Error(`unknown actor command ${command}`);
    }
    replyOK(value);
  } catch (error) {
    replyError(error);
  }
}

function queryI64(handle: { query(sql: string): Iterable<unknown[]> }, query: string): string {
  const rows = handle.query(query);
  const output: string[] = [];
  try {
    for (const row of rows) output.push(row.map((value) => render(value as never)).join(":"));
  } finally {
    if ("close" in rows) (rows as { close(): void }).close();
  }
  return output.join(",");
}

// timed runs one of the page-service benchmark's loops over bench_ps (id i64 PRIMARY KEY, v i64) and
// replies "<elapsed ns>:<checksum>". argument is "<iterations>[\t<rows>]"; the probe id of iteration k
// is (k*7919) mod rows + 1. The checksum is the sum of the values read (TIME_LOOKUPS), the last scan's
// sum (TIME_SCAN), or 0 (TIME_COMMITS).
function timed(db: Database, command: string, argument: string): string {
  const fields = argument.split("\t");
  const n = BigInt(fields[0] || "0");
  const rows = fields.length > 1 ? BigInt(fields[1]!) : 1n;
  let checksum = 0n;
  const firstInt = (query: Iterable<unknown[]> & { close(): void }): bigint => {
    try {
      for (const row of query) return (row[0] as { int: bigint }).int;
      throw new Error("timed query returned no row");
    } finally {
      query.close();
    }
  };
  const start = process.hrtime.bigint();
  switch (command) {
    case "TIME_LOOKUPS": {
      const stmt = db.prepareStatement("SELECT v FROM bench_ps WHERE id = $1");
      for (let k = 0n; k < n; k++) {
        checksum += firstInt(db.queryPrepared(stmt, [intValue(((k * 7919n) % rows) + 1n)]));
      }
      break;
    }
    case "TIME_SCAN":
      for (let k = 0n; k < n; k++)
        checksum = firstInt(db.query("SELECT sum(v)::i64 FROM bench_ps"));
      break;
    case "TIME_COMMITS": {
      const stmt = db.prepareStatement("UPDATE bench_ps SET v = v + 1 WHERE id = $1");
      for (let k = 0n; k < n; k++) {
        db.executePrepared(stmt, [intValue(((k * 7919n) % rows) + 1n)]);
      }
      break;
    }
  }
  return `${process.hrtime.bigint() - start}:${checksum}`;
}

function sql(hex: string): string {
  return Buffer.from(hex, "hex").toString("utf8");
}

function replyOK(value: string): void {
  console.log(`OK\t${value}`);
}

function replyError(error: unknown): void {
  if (error instanceof EngineError) {
    console.log(`ERR\t${error.code()}\t${Buffer.from(error.message).toString("hex")}`);
  } else {
    const message = error instanceof Error ? error.message : String(error);
    console.log(`ERR\tXXXXX\t${Buffer.from(message).toString("hex")}`);
  }
}

// ownerIdle serves OWNER_OPEN <hex path> / OWNER_CLOSE / CLOSE: it owns one database at a time under the
// test-only page-service mode, so another core's conformance harness can run queries as its client.
// root is the repository root, whose spec bundles the owner loads.
async function ownerIdle(root: string): Promise<void> {
  const environment = ownerEnvironment(root);
  console.log("READY");
  let owned: Database | null = null;
  for await (const line of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
    const tab = line.indexOf("\t");
    const command = tab < 0 ? line : line.slice(0, tab);
    const argument = tab < 0 ? "" : line.slice(tab + 1);
    try {
      switch (command) {
        case "OWNER_OPEN":
          owned = openDatabase(sql(argument), {
            locking: "service",
            skipFsync: true,
            fileLockTimeoutMs: 5000,
            environment,
          });
          break;
        case "OWNER_CLOSE":
          owned?.close();
          owned = null;
          break;
        case "CLOSE":
          owned?.close();
          replyOK("");
          return;
        default:
          throw new Error(`unknown owner-idle command ${command}`);
      }
      replyOK("");
    } catch (error) {
      replyError(error);
    }
  }
}

// ownerEnvironment loads the spec's Unicode and time-zone bundles: like any opener, an owner must
// resolve the collations a file's catalog references (XX002 otherwise), and the corpus files use these.
function ownerEnvironment(root: string): Environment {
  return new EnvironmentBuilder()
    .loadUnicodeData(readFileSync(join(root, "spec", "collation", "fixtures", "unicode.jucd")))
    .loadTimeZoneData(readFileSync(join(root, "spec", "tz", "fixtures", "tzdata.jtz")))
    .build();
}
