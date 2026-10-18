// End-to-end experiment driver. Intentionally uses the public database/session cursor path;
// only selecting the experimental evaluator and collecting program metrics use internal hooks.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createDatabase, openDatabase } from "../tooling.ts";
import { expressionVmStats, resetExpressionVmStats, withExpressionVm } from "../expression_vm.ts";
import { render } from "../value.ts";

const mode = process.env.JED_EXPR_VM_MODE ?? "tree";
if (mode !== "tree" && mode !== "vm") throw new Error("JED_EXPR_VM_MODE must be tree or vm");
const filter = process.env.JED_EXPR_VM_FILTER ?? "";
const override = process.env.JED_EXPR_VM_ITERATIONS;
if (override !== undefined && (!/^\d+$/.test(override) || Number(override) < 1))
  throw new Error("JED_EXPR_VM_ITERATIONS must be positive");
const storage = process.env.JED_EXPR_VM_STORAGE ?? "memory";
if (storage !== "memory" && storage !== "disk") throw new Error("invalid JED_EXPR_VM_STORAGE");
const directory = storage === "disk" ? mkdtempSync(join(tmpdir(), "jed-expression-vm-")) : null;
const path = directory === null ? undefined : join(directory, "data.jed");
let db = createDatabase(path === undefined ? {} : { path, locking: "exclusive" });
let session = db.session();
session.execute("CREATE TABLE t(id i32 PRIMARY KEY, a i32, b i32, p boolean, q boolean)");
session.execute("BEGIN");
for (let base = 1; base <= 20_000; base += 250) {
  const rows: string[] = [];
  for (let id = base; id < base + 250; id++) {
    const a = id % 17 === 0 ? "NULL" : String(id % 97);
    const b = id % 19 === 0 ? "NULL" : String((id % 13) + 1);
    const p = id % 23 === 0 ? "NULL" : String(id % 2 === 0);
    const q = id % 29 === 0 ? "NULL" : String(id % 3 === 0);
    rows.push(`(${id},${a},${b},${p},${q})`);
  }
  session.execute(`INSERT INTO t VALUES ${rows.join(",")}`);
}
session.execute("COMMIT");
if (path !== undefined) {
  session.close();
  db.close();
  db = openDatabase(path, { locking: "exclusive" });
  session = db.session();
}
const workloads = readFileSync(
  process.env.JED_EXPR_VM_INPUTS === undefined
    ? new URL("../../../../spec/experiments/expression-vm/workloads.tsv", import.meta.url)
    : join(process.env.JED_EXPR_VM_INPUTS, "workloads.tsv"),
  "utf8",
)
  .split("\n")
  .filter((line) => line.length > 0 && !line.startsWith("#"));
const encoder = new TextEncoder();
withExpressionVm(mode === "vm", () => {
  for (const line of workloads) {
    const [name, execution, iterationsText, sql] = line.split("\t");
    if (filter !== "" && !name.includes(filter)) continue;
    const iterations = Number(override ?? iterationsText);
    session.setMaxCost(name === "metered_scan" ? 1_000_000_000n : 0n);
    resetExpressionVmStats(true);
    const prepareStart = process.hrtime.bigint();
    const prepared = execution === "prepared" ? session.prepareStatement(sql) : null;
    const prepareNs = process.hrtime.bigint() - prepareStart;
    let checksum = 0n;
    let cost = 0n;
    let consumed = 0;
    function run(check: boolean): void {
      const rows = prepared === null ? session.query(sql) : session.queryPrepared(prepared);
      let hash = 14695981039346656037n;
      const byte = (value: number): void => {
        hash = BigInt.asUintN(64, (hash ^ BigInt(value)) * 1099511628211n);
      };
      try {
        for (const row of rows) {
          for (const cell of row) {
            consumed++;
            if (check) {
              for (const value of encoder.encode(render(cell))) byte(value);
              byte(0);
            }
          }
          consumed++;
          if (check) byte(10);
        }
        cost = rows.cost;
      } finally {
        rows.close();
      }
      if (check) checksum = hash;
    }
    const firstStart = process.hrtime.bigint();
    run(false);
    const firstNs = process.hrtime.bigint() - firstStart;
    const warmup = name.startsWith("point_") ? Math.min(100, iterations) : 3;
    for (let i = 0; i < warmup; i++) run(false);
    run(true);
    const expectedChecksum = checksum;
    const expectedCost = cost;
    const start = process.hrtime.bigint();
    for (let i = 0; i < iterations; i++) run(false);
    const elapsed = process.hrtime.bigint() - start;
    run(true);
    assert.equal(checksum, expectedChecksum);
    assert.equal(cost, expectedCost);
    console.log(
      JSON.stringify({
        core: "ts",
        mode,
        storage,
        case: name,
        execution,
        iterations,
        consumed,
        elapsed_ns: elapsed.toString(),
        checksum: checksum.toString(16).padStart(16, "0"),
        cost: cost.toString(),
        prepare_ns: prepareNs.toString(),
        first_execution_ns: firstNs.toString(),
        ...expressionVmStats,
      }),
    );
  }
});
session.close();
db.close();
if (directory !== null) rmSync(directory, { recursive: true, force: true });
