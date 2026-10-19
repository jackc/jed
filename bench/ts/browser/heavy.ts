import { buildInMemory } from "../../../impl/ts/src/shared.ts";
import { DEFAULT_PAGE_SIZE } from "../../../impl/ts/src/executor.ts";
import { createDatabase, type AsyncDatabase } from "../../../impl/ts/src/browser/async.ts";
import { runtimeLimits } from "../../../impl/ts/src/async/protocol.ts";
import { heavyConfigFrom, observeTimers, runHeavySuite } from "../src/async-heavy.ts";

type FrameSample = { atMs: number; gapMs: number };
type LongTask = { startTimeMs: number; durationMs: number; name: string };

function pause(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function gapSummary(gaps: number[]) {
  const sorted = [...gaps].sort((a, b) => a - b);
  const percentile = (fraction: number) =>
    sorted.length === 0 ? null : sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)];
  return { p50Ms: percentile(0.5), p95Ms: percentile(0.95), maxMs: sorted.at(-1) ?? null };
}

// Observe rendering and browser long tasks around the shared timer probe. Latency samples run
// separately, so monitoring allocations and the delayed observer flush do not inflate their times.
async function observeBrowser<T>(work: () => T | Promise<T>) {
  const frames: FrameSample[] = [];
  const longTasks: LongTask[] = [];
  let lastFrame: number | undefined;
  let frame = 0;
  const onFrame = () => {
    const now = performance.now();
    if (lastFrame !== undefined) frames.push({ atMs: now, gapMs: now - lastFrame });
    lastFrame = now;
    frame = requestAnimationFrame(onFrame);
  };
  frame = requestAnimationFrame(onFrame);
  const longTasksSupported =
    typeof PerformanceObserver !== "undefined" &&
    PerformanceObserver.supportedEntryTypes.includes("longtask");
  const collect = (entries: PerformanceEntry[]) => {
    for (const entry of entries) {
      longTasks.push({
        startTimeMs: entry.startTime,
        durationMs: entry.duration,
        name: entry.name,
      });
    }
  };
  const observer = longTasksSupported
    ? new PerformanceObserver((list) => collect(list.getEntries()))
    : undefined;
  observer?.observe({ type: "longtask" });
  try {
    // Establish a preceding frame before the measured operation so even a synchronous block's
    // first delayed animation callback contributes its full gap.
    await pause(40);
    const observation = await observeTimers(work);
    // Chromium reports a long task asynchronously after that task finishes. An immediate
    // disconnect would lose the very synchronous stall this comparison needs to measure.
    await pause(80);
    if (observer) collect(observer.takeRecords());
    const start = observation.timer.workStartedAtMs;
    const end = observation.timer.workEndedAtMs;
    const overlappingFrames = frames.filter(
      (sample) => sample.atMs >= start && sample.atMs - sample.gapMs < end,
    );
    const overlappingTasks = longTasks.filter(
      (task) => task.startTimeMs < end && task.startTimeMs + task.durationMs > start,
    );
    return {
      ...observation,
      extra: {
        visibilityState: document.visibilityState,
        animationFrames: {
          ticksDuringWork: frames.filter((sample) => sample.atMs >= start && sample.atMs < end)
            .length,
          callbackTimesMs: overlappingFrames.map((sample) => sample.atMs),
          gapsMs: overlappingFrames.map((sample) => sample.gapMs),
          firstCallbackAfterWorkMs: frames.find((sample) => sample.atMs >= end)?.atMs ?? null,
          summary: gapSummary(overlappingFrames.map((sample) => sample.gapMs)),
        },
        longTasks: {
          supported: longTasksSupported,
          entries: overlappingTasks,
          count: overlappingTasks.length,
          totalDurationMs: overlappingTasks.reduce((sum, task) => sum + task.durationMs, 0),
          maxDurationMs: Math.max(0, ...overlappingTasks.map((task) => task.durationMs)),
          durationScope:
            "Complete overlapping tasks; durations are not clipped to the SQL interval.",
          additionalObserverDrainMs: 80,
        },
      },
    };
  } finally {
    cancelAnimationFrame(frame);
    observer?.disconnect();
  }
}

async function runHeavyBenchmark(settings: Record<string, string | undefined>) {
  const config = heavyConfigFrom(settings);
  const directStarted = performance.now();
  const directDatabase = buildInMemory(DEFAULT_PAGE_SIZE);
  const directStartupMs = performance.now() - directStarted;
  const direct = directDatabase.session();
  let workerDatabase: AsyncDatabase | undefined;
  try {
    const workerStarted = performance.now();
    workerDatabase = await createDatabase();
    const workerStartupMs = performance.now() - workerStarted;
    const worker = await workerDatabase.session();
    const idleWork = async () => {
      await pause(250);
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
        runtimeLimits: runtimeLimits(),
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
