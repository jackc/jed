// Real-browser end-to-end test for the Browser/OPFS host (spec/design/hosts.md §5). Drives headless
// Chromium against the Vite-served demo (browser/main.ts), exercising the FULL stack the node parity
// test cannot: a real FileSystemSyncAccessHandle, in a real dedicated Worker, over the async RPC client.
// Gated — run with `npm run test:browser` after `npx playwright install chromium`; NOT in the node unit
// suite or `rake ci` (which the OPFS host leaves untouched — it adds no SQL semantics, hosts.md §5).
//
// Each test gets a fresh browser context, so OPFS starts empty; a unique db name per test avoids the
// exclusive-handle clash even if state lingered.

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
