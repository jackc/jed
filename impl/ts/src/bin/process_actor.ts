// Test-only line actor for the shared real-process corpus (concurrency-testing.md §10).

import { createInterface } from "node:readline";
import process from "node:process";

import {
  createDatabase,
  type Database,
  EngineError,
  attachFile,
  openDatabase,
  render,
  type Session,
} from "../tooling.ts";

const [action, path, timeoutText] = process.argv.slice(2);
if (action === undefined || path === undefined) {
  throw new Error("usage: process_actor.ts create|open PATH [timeout_ms]");
}
const fileLockTimeoutMs = timeoutText === undefined ? 5000 : Number(timeoutText);

let database: Database;
try {
  database =
    action === "create"
      ? createDatabase({ path, locking: "shared", fileLockTimeoutMs })
      : openDatabase(path, { locking: "shared", fileLockTimeoutMs });
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
