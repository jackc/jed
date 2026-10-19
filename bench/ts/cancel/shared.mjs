export const FIXED_LIMITS = {
  maxBatchRows: 512,
  maxBatchBytes: 256 * 1024,
  maxResultBytes: 2 * 1024 * 1024,
};

// Transport instrumentation never forwards probe messages into RuntimeConnection.
export function controlChannel(post) {
  const waiting = new Map();
  let active;
  return {
    receive(message) {
      if (!message?.probe) return false;
      if (message.probe === "active" && active?.token === message.token) {
        active.onMarker(message);
      } else {
        const key = `${message.probe}:${message.token}`;
        waiting.get(key)?.(message);
        waiting.delete(key);
      }
      return true;
    },
    post(request) {
      if (request.op === "cancel" && active) active.cancelPostAtMs ??= performance.now();
      post(request);
    },
    async arm(token, sql, onMarker) {
      active = { token, onMarker };
      await new Promise((resolve) => {
        waiting.set(`armed:${token}`, resolve);
        post({ probe: "arm", token, sql });
      });
    },
    async finish(token) {
      const client = active;
      const response = await new Promise((resolve) => {
        waiting.set(`finished:${token}`, resolve);
        post({ probe: "finish", token });
      });
      active = undefined;
      return { cancelPostAtMs: client?.cancelPostAtMs, worker: response.worker };
    },
  };
}

function summary(values) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted.length
    ? {
        samples: sorted.length,
        min: sorted[0],
        median: (sorted[Math.floor((sorted.length - 1) / 2)] + sorted[Math.floor(sorted.length / 2)]) / 2,
        max: sorted.at(-1),
      }
    : null;
}

export async function runCancellationSuite({
  db,
  control,
  workloads,
  validateQuery,
  settings = {},
}) {
  const integer = (key, fallback, minimum = 0) => {
    const value = Number(settings[key] ?? fallback);
    if (!Number.isSafeInteger(value) || value < minimum) throw new Error(`invalid ${key}`);
    return value;
  };
  const config = {
    fullWarmups: integer("JED_CANCEL_FULL_WARMUPS", 3),
    cancelWarmups: integer("JED_CANCEL_WARMUPS", 2),
    samples: integer("JED_CANCEL_SAMPLES", 7, 1),
    delayAfterMarkerMs: integer("JED_CANCEL_DELAY_MS", 20),
    watchdogMs: integer("JED_CANCEL_TIMEOUT_MS", 30000, 1),
  };
  const session = await db.session();
  const results = [];
  let token = 0;
  try {
    for (const workload of workloads) {
      for (const sql of workload.setup) await session.execute(sql);
      const validated = await validateQuery(session, workload.sql);
      if (validated.rows !== workload.expectedRows)
        throw new Error("unexpected validation row count");
      for (let i = 0; i < config.fullWarmups; i++) {
        const actual = await validateQuery(session, workload.sql);
        if (JSON.stringify(actual) !== JSON.stringify(validated))
          throw new Error("warmup result changed");
      }
      const samples = [];
      for (let index = -config.cancelWarmups; index < config.samples; index++) {
        token++;
        const controller = new AbortController();
        let marker, markerReceivedAtMs, cancelIssuedAtMs, settledAtMs, timer;
        let state = "running",
          code,
          consumedRows = 0,
          rows,
          watchdogFired = false;
        await control.arm(token, workload.sql, (message) => {
          marker = message;
          markerReceivedAtMs = performance.now();
          timer = setTimeout(() => {
            if (state !== "running") return;
            cancelIssuedAtMs = performance.now();
            controller.abort();
          }, config.delayAfterMarkerMs);
        });
        const submittedAtMs = performance.now();
        const watchdog = setTimeout(() => {
          watchdogFired = true;
          controller.abort();
        }, config.watchdogMs);
        try {
          rows = await session.query(workload.sql, [], { signal: controller.signal });
          for await (const _row of rows) consumedRows++;
          settledAtMs = performance.now();
          state =
            cancelIssuedAtMs === undefined ? "completed-before-abort" : "completed-after-abort";
        } catch (error) {
          settledAtMs = performance.now();
          code = typeof error.code === "function" ? error.code() : error.code;
          state = code === "57014" ? "canceled" : "failed";
          if (state === "failed") throw error;
        } finally {
          clearTimeout(watchdog);
          clearTimeout(timer);
          await rows?.close();
        }
        const telemetry = await control.finish(token);
        const followupStartedAtMs = performance.now();
        const followup = await validateQuery(session, "SELECT 1 AS ok");
        const followupFinishedAtMs = performance.now();
        if (followup.rows !== 1) throw new Error("post-cancellation query failed");
        const sample = {
          index,
          state,
          code,
          consumedRows,
          watchdogFired,
          submittedAtMs,
          markerReceivedAtMs,
          cancelIssuedAtMs,
          settledAtMs,
          cancelPostAtMs: telemetry.cancelPostAtMs,
          marker,
          worker: telemetry.worker,
          submitToMarkerMs:
            markerReceivedAtMs === undefined ? null : markerReceivedAtMs - submittedAtMs,
          markerToIssueMs:
            cancelIssuedAtMs === undefined ? null : cancelIssuedAtMs - markerReceivedAtMs,
          issueToSettleMs: cancelIssuedAtMs === undefined ? null : settledAtMs - cancelIssuedAtMs,
          issueToPostMs:
            cancelIssuedAtMs === undefined ? null : telemetry.cancelPostAtMs - cancelIssuedAtMs,
          receivedToErrorSentMs:
            telemetry.worker?.cancelReceivedAtMs === undefined ||
            telemetry.worker?.errorSentAtMs === undefined
              ? null
              : telemetry.worker.errorSentAtMs - telemetry.worker.cancelReceivedAtMs,
          followupMs: followupFinishedAtMs - followupStartedAtMs,
          cleanupVerified: true,
          followup,
        };
        if (watchdogFired) throw new Error(`${workload.id}: cancellation watchdog fired`);
        if (state === "canceled" && (!marker || cancelIssuedAtMs === undefined))
          throw new Error(
            `${workload.id}: canceled without active-execution marker and timed abort`,
          );
        if (index >= 0) samples.push(sample);
      }
      const canceled = samples.filter((sample) => sample.state === "canceled");
      results.push({
        ...workload,
        validated,
        samples,
        canceled: canceled.length,
        issueToSettleMs: summary(canceled.map((sample) => sample.issueToSettleMs)),
        submitToMarkerMs: summary(
          samples.flatMap((sample) =>
            sample.submitToMarkerMs === null ? [] : [sample.submitToMarkerMs],
          ),
        ),
        markerToIssueMs: summary(canceled.map((sample) => sample.markerToIssueMs)),
      });
    }
  } finally {
    await session.close();
  }
  return {
    config,
    limits: FIXED_LIMITS,
    results,
    methodology: [
      "Benchmark-only worker adapters use the production RuntimeServer, RuntimeConnection, semantic core and host scheduling primitive.",
      "One active marker is posted at the first real task yield while a query/fetch continuation is active; no SQL or cost is changed.",
      "Abort is requested after a fixed delay from receiving that marker. No query is aborted before the active marker.",
      "Primary issue-to-settle elapsed time uses only the main-thread clock, recorded at the operation rejection before explicit close and followup.",
      "Worker cancel-receipt to error-send uses only the worker clock; negative values can occur when Node observes the atomic flag before the cancel message.",
      "No cross-clock subtraction or dispatch-latency claim. Submit-to-marker includes request dispatch and the first execution segment, plus marker delivery.",
      "Completed-before-abort samples are reported separately; full-query validation/warmup is outside cancellation timing.",
      "Fixed batch limits. Following every sample, SELECT 1 checks that the same session and worker remain usable.",
      "Instrumentation adds one worker-to-main marker and fixed bookkeeping; cancellation latency does not characterize an uninterruptible scalar/sort kernel's worst case.",
    ],
  };
}
