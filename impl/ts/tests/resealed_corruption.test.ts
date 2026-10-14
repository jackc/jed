// Resealed structural corruption (storage.md §7): the per-page CRC proves a page was written whole,
// not that it is the page its referrer expected or that its directories are self-consistent. A page
// copied from another table, or a CRC-valid page whose in-page lengths and offsets disagree with each
// other, reaches the decoders. They must fail closed with an EngineError — never throw a TypeError /
// RangeError, overflow the stack, exhaust memory, or hang. checksum.test.ts cannot reach these
// paths: its byte corruptions all break a CRC first. Mirrors impl/go/resealed_corruption_test.go.
//
// Each case mutates one body page of a golden fixture, recomputes that page's CRC, then opens the
// image and scans and deletes every table. Wrong rows are possible (a whole-page transposition is
// undetectable), so the oracle is only "EngineError or success". The cases run in a worker
// (resealed_worker.ts) so a hang is detected from its progress cell; JED_RESEALED_MUTATIONS raises
// the number of mutations per body page for an exploration run.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Worker } from "node:worker_threads";

const HANG_MS = 10_000;

function mutationsPerPage(): number {
  const n = Number(process.env.JED_RESEALED_MUTATIONS ?? "");
  return Number.isInteger(n) && n > 0 ? n : 6;
}

test("resealed page corruption fails closed", async (t) => {
  const progress = new SharedArrayBuffer(16);
  const cell = new Int32Array(progress);
  const worker = new Worker(new URL("./resealed_worker.ts", import.meta.url), {
    workerData: { per: mutationsPerPage(), progress },
  });
  const failures: string[] = [];
  let files: string[] = [];
  const summary = await new Promise<string>((resolve, reject) => {
    let lastCase = -1;
    let stalledSince = Date.now();
    const timer = setInterval(() => {
      const now = Atomics.load(cell, 3);
      if (now !== lastCase) {
        lastCase = now;
        stalledSince = Date.now();
      } else if (Date.now() - stalledSince > HANG_MS) {
        clearInterval(timer);
        const where = `fixture #${Atomics.load(cell, 0)} page ${Atomics.load(cell, 1)} mutation ${Atomics.load(cell, 2)}`;
        void worker.terminate();
        reject(
          new Error(
            `a resealed case did not return within ${HANG_MS} ms: ${where}\n${failures.join("\n")}`,
          ),
        );
      }
    }, 500);
    worker.on(
      "message",
      (msg: {
        kind: string;
        text?: string;
        cases?: number;
        failedClosed?: number;
        skipped?: number;
        files?: string[];
      }) => {
        if (msg.kind === "failure") failures.push(msg.text!);
        else {
          clearInterval(timer);
          files = msg.files!;
          resolve(
            `${msg.cases} resealed cases, ${msg.failedClosed} failed closed, ${msg.skipped} fixtures skipped`,
          );
        }
      },
    );
    worker.on("error", (e) => {
      clearInterval(timer);
      reject(e);
    });
    worker.on("exit", (code) => {
      clearInterval(timer);
      if (code !== 0) reject(new Error(`resealed worker exited with code ${code}`));
    });
  });
  await worker.terminate();
  t.diagnostic(summary);
  assert.ok(files.length > 0, "no fixtures found");
  assert.deepEqual(failures, [], `${failures.length} non-engine failures (${summary})`);
});
