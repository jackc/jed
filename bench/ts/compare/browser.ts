// Vite supplies one static adapter import for the selected source snapshot. This keeps the
// original master's synchronous core independent of the branch's worker/runtime modules.
import { createComparisonDatabase, comparisonRuntimeLimits } from "@compare-adapter";
import { observeBrowser } from "../browser/observe.ts";
import { comparisonConfigFrom, directDriver, runComparison, workerDriver } from "./suite.ts";

declare const __COMPARE_MODE__: "direct" | "worker";

async function runComparisonBenchmark(settings: Record<string, string | undefined>) {
  const config = comparisonConfigFrom(settings);
  const started = performance.now();
  const database = await createComparisonDatabase();
  const databaseMs = performance.now() - started;
  const sessionStarted = performance.now();
  const session = await database.session();
  const sessionMs = performance.now() - sessionStarted;
  const driver = __COMPARE_MODE__ === "direct" ? directDriver(session) : workerDriver(session);
  const idleWork = () => new Promise<void>((resolve) => setTimeout(resolve, 250));
  try {
    const idleBefore = await observeBrowser(idleWork);
    const suite = await runComparison({
      driver,
      config,
      observe: observeBrowser,
      onProgress(message) {
        document.getElementById("status")!.textContent = message;
        console.log(`JED_COMPARE_PROGRESS ${message}`);
      },
    });
    const idleAfter = await observeBrowser(idleWork);
    document.getElementById("status")!.textContent = "Benchmark complete.";
    return {
      mode: __COMPARE_MODE__,
      browserEnvironment: {
        userAgent: navigator.userAgent,
        crossOriginIsolated,
        sharedArrayBuffer: typeof SharedArrayBuffer !== "undefined",
        measuredQueriesPassAbortSignal: false,
        hardwareConcurrency: navigator.hardwareConcurrency,
        visibilityState: document.visibilityState,
        pageSize: 8192,
        runtimeLimits: comparisonRuntimeLimits,
        storage: "memory",
        viewport: { width: innerWidth, height: innerHeight, devicePixelRatio },
      },
      startup: { databaseMs, sessionMs },
      idle: { before: idleBefore, after: idleAfter },
      suite,
    };
  } finally {
    await session.close();
    await database.close();
  }
}

declare global {
  interface Window {
    runComparisonBenchmark: typeof runComparisonBenchmark;
  }
}
window.runComparisonBenchmark = runComparisonBenchmark;
