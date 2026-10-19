import { observeTimers } from "../src/async-heavy.ts";

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
export async function observeBrowser<T>(work: () => T | Promise<T>) {
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
