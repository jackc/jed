import { expect, test } from "@playwright/test";

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  // The demo owns a worker during its initial OPFS write/read. Let it close before quota probes.
  await expect(page.locator("#out")).toContainText("read back");
});

test("public custom workers await registration and isolate function state", async ({ page }) => {
  const result = await page.evaluate(() => window.jedHostFunctions.isolatedFunctionsScenario());
  expect(result).toEqual({
    exact: "9007199254741034",
    counters: ["1", "2", "1"],
    strict: null,
  });
});

test("custom workers register host functions when creating and reopening OPFS", async ({
  page,
}) => {
  const result = await page.evaluate(() =>
    window.jedHostFunctions.persistentFunctionsScenario("host-functions.jed"),
  );
  expect(result).toEqual({ persisted: "42", registeredAgain: "83" });
});

test("failed async configuration preserves storage and releases the worker and quota", async ({
  page,
}) => {
  const result = await page.evaluate(() =>
    window.jedHostFunctions.failedConfigurationScenario("host-config-failure.jed"),
  );
  expect(result).toEqual({
    code: "WORKER_FAILED",
    message: "application configuration failed",
    terminations: 1,
    recovered: "48",
  });
});

test("worker factories release failed reservations and transfer exclusive lifetime ownership", async ({
  page,
}) => {
  const result = await page.evaluate(() => window.jedHostFunctions.workerOwnershipScenario());
  expect(result).toEqual({
    preservedError: true,
    reused: "PROTOCOL",
    reusedClosed: "PROTOCOL",
    alive: "1",
    terminations: 1,
  });
});

test("worker factories accept dedicated workers from another same-origin realm", async ({
  page,
}) => {
  const result = await page.evaluate(() => window.jedHostFunctions.crossRealmWorkerScenario());
  expect(result).toEqual({ foreignRealm: true, value: "42" });
});
