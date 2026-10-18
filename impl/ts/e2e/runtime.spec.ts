import { expect, test } from "@playwright/test";

test("typed worker transport restores exact values and recursive containers", async ({ page }) => {
  await page.goto("/");
  const result = await page.evaluate(() => window.jedRuntime.typedScenario());
  expect(result.kinds).toEqual(["int", "decimal", "bytea", "null", "array", "composite", "range"]);
  expect(result.values).toEqual([
    "9007199254740993",
    "12345678901234567890.123456789",
    "\\x007fff",
    "NULL",
    "{1,2}",
    "(1,x)",
    "[1,5)",
  ]);
  expect(result.decimalRestored).toBe(true);
  expect(result.bytesRestored).toBe(true);
});

test("writer waiters yield to commits, early iterators release cursors, and close rolls back OPFS", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(() => window.jedRuntime.lifecycleScenario("lifecycle.jed"));
  expect(result).toEqual({ beforeCommit: "0", early: "1", afterBreak: "3", reopened: "3" });
});

test("queue and result limits preserve structured SQL errors and release failed cursors", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(() => window.jedRuntime.errorScenario());
  expect(result).toEqual({
    queue: "QUEUE_FULL",
    sql: "23505",
    table: "t",
    result: "RESULT_TOO_LARGE",
    recovery: "7",
    closed: "CLOSED",
  });
});

test("worker failure rejects outstanding work with an explicitly unknown outcome", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(() => window.jedRuntime.failureScenario());
  expect(result).toEqual({ code: "WORKER_FAILED", outcome: "unknown" });
});

test("ordinary browsers cancel at batch boundaries and keep the application event loop responsive", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(() => window.jedRuntime.cancellationScenario());
  expect(result).toEqual({
    isolated: false,
    first: true,
    code: "57014",
    admission: "57014",
    count: "20",
    responsive: true,
  });
});

test("ordinary browsers cancel resident scans before their first result batch", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(() => window.jedRuntime.cooperativeCancellationScenario());
  expect(result).toEqual({
    isolated: false,
    code: "57014",
    progressed: true,
    status: "Failed",
    recovered: "7",
  });
});
