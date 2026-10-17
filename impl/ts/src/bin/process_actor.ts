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
  crashBeforeNextPageServiceAckForTesting,
  hangUpNextPageServiceAckForTesting,
  intValue,
  openDatabase,
  pageServiceAttachmentRoleForTesting,
  pageServiceRoleForTesting,
  render,
  type Session,
} from "../tooling.ts";

const [action, path, ...rest] = process.argv.slice(2);
if (action === undefined || path === undefined) {
  throw new Error(
    "usage: process_actor.ts create|open PATH [timeout_ms [idle_timeout_ms]] [key=value...] | owner-idle REPO_ROOT",
  );
}
// Positional arguments after the path: the open timeout, then the page-service owner's
// idle-in-transaction timeout (spec/design/page-service.md §11.1). key=value arguments set the
// handoff options (spec/pageservice/README.md §6.5): ownership=prefer|yield|never, yield_grace,
// shutdown_grace, idle_session (milliseconds), max_connections.
const positional: string[] = [];
const options = new Map<string, string>();
for (const arg of rest) {
  const eq = arg.indexOf("=");
  if (eq < 0) positional.push(arg);
  else options.set(arg.slice(0, eq), arg.slice(eq + 1));
}
const [timeoutText, idleText] = positional;
const millis = (key: string): number | undefined =>
  options.has(key) ? Number(options.get(key)) : undefined;
const ownershipText = options.get("ownership");
const ownership: "prefer" | "yield" | "never" =
  ownershipText === "yield" || ownershipText === "never" ? ownershipText : "prefer";
const handoffOptions = {
  ownership,
  yieldGraceMs: millis("yield_grace"),
  shutdownGraceMs: millis("shutdown_grace"),
  idleSessionTimeoutMs: millis("idle_session"),
  maxConnections: millis("max_connections"),
};
if (action === "owner-idle") {
  // The corpus-through-a-client driver (spec/pageservice/README.md §7): open and close databases as
  // their page-service owner on command.
  await ownerIdle(path);
  process.exit(0);
}
const fileLockTimeoutMs = timeoutText === undefined ? 5000 : Number(timeoutText);
// The page-service owner's idle-in-transaction timeout (spec/design/page-service.md §11.1).
const idleInTransactionTimeoutMs = idleText === undefined ? undefined : Number(idleText);
// create / open / open-read-only use the default locking (the page service); open-exclusive is a
// non-serving owner.
const locking = action === "open-exclusive" ? "exclusive" : "auto";
const readOnly = action === "open-read-only";

let database: Database;
try {
  database =
    action === "create"
      ? createDatabase({
          path,
          locking,
          fileLockTimeoutMs,
          idleInTransactionTimeoutMs,
          ...handoffOptions,
        })
      : openDatabase(path, {
          locking,
          fileLockTimeoutMs,
          idleInTransactionTimeoutMs,
          readOnly,
          ...handoffOptions,
        });
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
      case "DETACH":
        database.detach(argument);
        break;
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
      case "HANGUP_NEXT_ACK":
        // The commit-resolution test hook (spec/pageservice/README.md §6.4).
        hangUpNextPageServiceAckForTesting(database);
        break;
      case "CRASH_NEXT_ACK":
        // End the process instead of acknowledging the next client commit (README §6.5).
        crashBeforeNextPageServiceAckForTesting(database);
        break;
      case "ROLE":
        // The page-service role test hook (spec/pageservice/README.md §6.5): main's, or an
        // attachment's when the step names one.
        value =
          argument === ""
            ? pageServiceRoleForTesting(database)
            : pageServiceAttachmentRoleForTesting(database, argument);
        break;
      case "TXID":
        value = database.txid.toString();
        break;
      case "PAGE_COUNT":
        value = database.pageCount.toString();
        break;
      case "CLOSE": {
        // Acknowledged at once; the close itself may take a while (an owner drains its clients first,
        // README §6.5), and the exit status reports its outcome.
        replyOK("");
        let status = 0;
        try {
          reader?.close();
          writer?.close();
          database.close();
        } catch (error) {
          console.error(error);
          status = 1;
        }
        process.exit(status);
        break;
      }
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
// page service, so another core's conformance harness can run queries as its client.
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
            locking: "auto",
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
