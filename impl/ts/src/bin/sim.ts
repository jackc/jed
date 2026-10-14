// The TS deterministic-simulation runner's entry point (spec/design/simulation-testing.md §11).
// Test tooling only; run through scripts/sim.rb (`mise run sim:sweep`, `mise run sim:replay`).
//
//   JED_SIM_IN=trace.sim JED_SIM_OUT=out/ node src/bin/sim.ts
//
// JED_SIM_IN is one .sim file, a directory of them, or a file listing trace paths one per line.
// Results append to JED_SIM_RESULTS (default JED_SIM_OUT/ts.results.jsonl). With JED_SIM_IMAGES set,
// every image a recovery opens is written to JED_SIM_OUT/<trace>.ts.step<N>[.<db>].img. Each
// transcript is JED_SIM_OUT/<trace>.ts.jsonl. Exits 1 if any trace failed.

import "../crc32_node.ts";

import {
  appendFileSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  statSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import process from "node:process";

import { resultJson, runTrace } from "../sim/runner.ts";
import { compareUtf8 } from "../sim/util.ts";

function tracePaths(input: string): string[] {
  if (statSync(input).isDirectory()) {
    return readdirSync(input)
      .filter((f) => f.endsWith(".sim"))
      .map((f) => join(input, f))
      .sort(compareUtf8);
  }
  if (input.endsWith(".sim")) return [input];
  return readFileSync(input, "utf8")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");
}

const input = process.env.JED_SIM_IN ?? "";
if (input === "") {
  console.error("JED_SIM_IN not set (run through scripts/sim.rb)");
  process.exit(2);
}
const out = process.env.JED_SIM_OUT || mkdtempSync(join(tmpdir(), "jed-sim-"));
mkdirSync(out, { recursive: true });
const resultsPath = process.env.JED_SIM_RESULTS || join(out, "ts.results.jsonl");
const writeImages = (process.env.JED_SIM_IMAGES ?? "") !== "";

let failed = false;
for (const path of tracePaths(input)) {
  const name = basename(path).replace(/\.sim$/, "");
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch (e) {
    const violation = "read trace: " + (e instanceof Error ? e.message : String(e));
    appendFileSync(
      resultsPath,
      resultJson({
        name,
        steps: 0,
        violation,
        violationStep: -1,
        transcriptHash: "0000000000000000",
        opsHash: "0000000000000000",
      }) + "\n",
    );
    failed = true;
    continue;
  }
  const res = runTrace(name, text, out, writeImages);
  appendFileSync(resultsPath, resultJson(res) + "\n");
  if (res.violation !== "") {
    console.error(`${path}: ${res.violation}`);
    failed = true;
  }
}
process.exit(failed ? 1 : 0);
