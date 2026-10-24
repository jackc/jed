// Real-browser end-to-end test for the Browser/OPFS host (spec/design/hosts.md §5). Drives headless
// Chromium against the Vite-served demo (browser/main.ts), exercising the FULL stack the node parity
// test cannot: a real FileSystemSyncAccessHandle, in a real dedicated Worker, over the async RPC client.
// Runs in `mise run ci` as the `e2e:ts` step (or `npm run test:browser` here), after `mise run
// dev:browsers` installs Chromium; NOT in the node unit suite. The OPFS host adds no SQL semantics, so
// the conformance corpus is untouched (hosts.md §5).
//
// Each test gets a fresh browser context, so OPFS starts empty; a unique db name per test avoids the
// exclusive-handle clash even if state lingered.

import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import { expect, test } from "@playwright/test";

const EXPECTED = [
  ["1", "one"],
  ["2", "two"],
  ["3", "three"],
];

test("writes a database to OPFS and reads it back through a fresh handle", async ({ page }) => {
  await page.goto("/");
  const name = "rt1.jed";
  const written = await page.evaluate((n) => window.jed.writeScenario(n), name);
  const read = await page.evaluate((n) => window.jed.readScenario(n), name);
  expect(written).toEqual(EXPECTED);
  expect(read).toEqual(EXPECTED); // durable across handle close + reopen on real OPFS
});

test("committed data survives a full page reload (real OPFS durability)", async ({ page }) => {
  await page.goto("/");
  const name = "rt2.jed";
  const written = await page.evaluate((n) => window.jed.writeScenario(n), name);
  expect(written).toEqual(EXPECTED);

  // A full reload tears down the page (and the worker); reopening the file must still see the rows —
  // the strongest durability check for the OPFS host.
  await page.reload();
  const read = await page.evaluate((n) => window.jed.readScenario(n), name);
  expect(read).toEqual(EXPECTED);
});

test("opening an absent database raises the structured 58P01 across the worker boundary", async ({
  page,
}) => {
  await page.goto("/");
  const code = await page.evaluate(() => window.jed.errorScenario("does-not-exist.jed"));
  expect(code).toBe("58P01"); // undefined_file — the structured-error contract survives postMessage
});

test("commit, recovery and maintenance await an asynchronous page codec over real OPFS", async ({
  page,
}) => {
  await page.goto("/");
  // The demo owns a worker during its initial OPFS write/read; let it release its handle first.
  await expect(page.locator("#out")).toContainText("read back");
  const result = await page.evaluate(() => window.jed.asyncCodecScenario());
  expect(result).toEqual({
    rows: 30,
    sameRows: true,
    sameReread: true,
    sameBytes: true,
    encoded: true,
    decoded: true,
  });
});

test("an encrypted database seals its pages through WebCrypto, compacts, rekeys and changes its credential", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.locator("#out")).toContainText("read back");
  const result = await page.evaluate((n) => window.jed.encryptedScenario(n), "enc1.jed");
  expect(result).toEqual({
    written: EXPECTED,
    read: EXPECTED,
    missing: "28P02",
    wrong: "28P01",
    compact: "<no error>",
    rekey: "<no error>",
    oldKey: "28P01",
    rotatedKey: "28P01",
    afterChange: EXPECTED,
    journal: null,
  });
});

test("compaction shrinks an OPFS database through its journal (hosts.md §5.1)", async ({
  page,
}) => {
  await page.goto("/");
  await expect(page.locator("#out")).toContainText("read back");
  const result = await page.evaluate((n) => window.jed.compactScenario(n), "compact1.jed");
  expect(result).toEqual({ shrank: true, journal: null, rows: 3 });
});

test("WebCrypto writes the Node host's bytes, and its per-page cost", async ({ page }) => {
  test.setTimeout(120_000);
  // The Node side: the seeded workload's file under node:crypto, and the goldens' rows.
  const script = fileURLToPath(new URL("./encryption_expected.ts", import.meta.url));
  const request = JSON.parse(
    execFileSync(process.execPath, [script], { maxBuffer: 64 << 20 }).toString(),
  );
  await page.goto("/");
  await expect(page.locator("#out")).toContainText("read back");
  const result = (await page.evaluate((r) => window.jed.encryptionScenario(r), request)) as {
    error?: string;
    goldens: Record<string, boolean>;
    sameBytes: boolean;
    timing: Record<"plain" | "sealed", { write: number; scan: number; pages: number }>;
  };
  expect(result.error).toBeUndefined();
  expect(Object.values(result.goldens)).toEqual([true, true, true, true]);
  expect(result.sameBytes).toBe(true);
  // The per-page cost (encryption.md §9): the encrypted run's extra time over the plaintext one,
  // per page of the encrypted file — reported, not asserted (wall-clock).
  const { plain, sealed } = result.timing;
  const perPage = (a: number, b: number) => (((a - b) * 1000) / sealed.pages).toFixed(1);
  console.log(
    `browser encryption cost: ${sealed.pages} pages; cold scan ${sealed.scan.toFixed(1)} ms vs ` +
      `${plain.scan.toFixed(1)} ms plaintext (${perPage(sealed.scan, plain.scan)} µs/page); bulk ` +
      `write ${sealed.write.toFixed(1)} ms vs ${plain.write.toFixed(1)} ms ` +
      `(${perPage(sealed.write, plain.write)} µs/page)`,
  );
});
