import { buildInMemory } from "../../../impl/ts/src/shared.ts";
import { DEFAULT_PAGE_SIZE } from "../../../impl/ts/src/executor.ts";
import { createDatabase, type AsyncDatabase } from "../../../impl/ts/src/browser/async.ts";
import { observeBrowser } from "./observe.ts";
import { runtimeLimits } from "../../../impl/ts/src/async/protocol.ts";
import { heavyConfigFrom, heavyRuntimeFrom, runHeavySuite } from "../src/async-heavy.ts";

async function runHeavyBenchmark(settings: Record<string, string | undefined>) {
  const config = heavyConfigFrom(settings);
  const limits = runtimeLimits(heavyRuntimeFrom(settings));
  const directStarted = performance.now();
  const directDatabase = buildInMemory(DEFAULT_PAGE_SIZE);
  const directStartupMs = performance.now() - directStarted;
  const direct = directDatabase.session();
  let workerDatabase: AsyncDatabase | undefined;
  try {
    const workerStarted = performance.now();
    workerDatabase = await createDatabase({ runtime: limits });
    const workerStartupMs = performance.now() - workerStarted;
    const worker = await workerDatabase.session();
    const idleWork = async () => {
      await new Promise((resolve) => setTimeout(resolve, 250));
      return null;
    };
    const idleBefore = await observeBrowser(idleWork);
    const result = await runHeavySuite({
      direct,
      worker,
      config,
      observe: observeBrowser,
      onProgress(message) {
        document.getElementById("status")!.textContent = message;
        console.log(`JED_HEAVY_PROGRESS ${message}`);
      },
    });
    const idleAfter = await observeBrowser(idleWork);
    document.getElementById("status")!.textContent = "Benchmark complete.";
    return {
      browserEnvironment: {
        userAgent: navigator.userAgent,
        crossOriginIsolated,
        sharedArrayBuffer: typeof SharedArrayBuffer !== "undefined",
        cancellationSignalProvided: false,
        hardwareConcurrency: navigator.hardwareConcurrency,
        visibilityState: document.visibilityState,
        pageSize: DEFAULT_PAGE_SIZE,
        runtimeLimits: limits,
        viewport: { width: innerWidth, height: innerHeight, devicePixelRatio },
      },
      startup: { directMs: directStartupMs, workerMs: workerStartupMs },
      idle: { before: idleBefore, after: idleAfter },
      suite: result,
    };
  } finally {
    direct.close();
    directDatabase.close();
    await workerDatabase?.close();
  }
}

declare global {
  interface Window {
    runHeavyBenchmark: typeof runHeavyBenchmark;
  }
}
window.runHeavyBenchmark = runHeavyBenchmark;
