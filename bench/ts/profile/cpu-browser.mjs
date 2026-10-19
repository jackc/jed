// Chromium CPU sampling for a warmed page and its already-created execution worker.
// Playwright exposes CDPSession for Page/Frame, but not Worker. Public nested CDP routing
// reaches the worker without private Playwright objects or another browser connection.
// Raw profiles retain bundled callFrame.url/functionName and zero-based line/column numbers.

function nestedSession(parent, sessionId, timeoutMs) {
  let nextId = 0;
  let closed = false;
  const pending = new Map();

  function fail(error) {
    closed = true;
    for (const call of pending.values()) {
      clearTimeout(call.timer);
      call.reject(error);
    }
    pending.clear();
  }

  function received(event) {
    if (event.sessionId !== sessionId) return;
    let message;
    try {
      message = JSON.parse(event.message);
    } catch (error) {
      fail(error);
      return;
    }
    const call = pending.get(message.id);
    if (call === undefined) return;
    pending.delete(message.id);
    clearTimeout(call.timer);
    if (message.error) {
      call.reject(new Error(`${call.method}: ${message.error.message}`));
    } else {
      call.resolve(message.result);
    }
  }

  function detached(event) {
    if (event.sessionId === sessionId) fail(new Error("CPU profile worker detached"));
  }
  const parentClosed = () => fail(new Error("CPU profile browser session closed"));
  parent.on("Target.receivedMessageFromTarget", received);
  parent.on("Target.detachedFromTarget", detached);
  parent.on("close", parentClosed);

  return {
    send(method, params = {}) {
      if (closed) return Promise.reject(new Error("CPU profile worker session is closed"));
      const id = ++nextId;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          pending.delete(id);
          reject(new Error(`Worker CDP ${method} timed out after ${timeoutMs} ms`));
        }, timeoutMs);
        pending.set(id, { resolve, reject, timer, method });
        parent.send("Target.sendMessageToTarget", {
          sessionId,
          message: JSON.stringify({ id, method, params }),
        }).catch((error) => {
          const call = pending.get(id);
          if (call === undefined) return;
          pending.delete(id);
          clearTimeout(call.timer);
          call.reject(error);
        });
      });
    },
    async detach() {
      const alreadyClosed = closed;
      fail(new Error("CPU profile worker session detached"));
      parent.off("Target.receivedMessageFromTarget", received);
      parent.off("Target.detachedFromTarget", detached);
      parent.off("close", parentClosed);
      if (!alreadyClosed) await parent.send("Target.detachFromTarget", { sessionId });
    },
  };
}

function errors(results) {
  return results.filter((result) => result.status === "rejected").map((result) => result.reason);
}

/**
 * Attach after database creation and warmup, before the query phase to sample.
 * start()/stop() control both target-local profilers; stop() returns two raw CDP Profiles.
 * Separate commands introduce boundary skew; raw target timestamps are not forcibly aligned.
 * Keep the worker alive until stop(); close() only detaches these profiling sessions.
 */
export async function attachBrowserProfilers(browser, page, {
  workerUrlSubstring = "worker",
  samplingIntervalUs = 1000,
  commandTimeoutMs = 30000,
} = {}) {
  if (typeof workerUrlSubstring !== "string") throw new TypeError("workerUrlSubstring must be a string");
  if (!Number.isSafeInteger(samplingIntervalUs) || samplingIntervalUs <= 0) {
    throw new RangeError("samplingIntervalUs must be a positive safe integer");
  }
  if (!Number.isSafeInteger(commandTimeoutMs) || commandTimeoutMs <= 0) {
    throw new RangeError("commandTimeoutMs must be a positive safe integer");
  }

  let main;
  let parent;
  let worker;
  try {
    main = await page.context().newCDPSession(page);
    parent = await browser.newBrowserCDPSession();
    const { targetInfo: mainTarget } = await main.send("Target.getTargetInfo");
    const pageWorkerUrls = new Set(page.workers().map((entry) => entry.url()));
    const { targetInfos } = await parent.send("Target.getTargets", { filter: [{ type: "worker" }] });
    const candidates = targetInfos.filter((target) =>
      target.type === "worker" &&
      target.browserContextId === mainTarget.browserContextId &&
      pageWorkerUrls.has(target.url) && target.url.includes(workerUrlSubstring));
    if (candidates.length !== 1) {
      throw new Error(`Expected one page worker matching ${JSON.stringify(workerUrlSubstring)}, found ${candidates.length}`);
    }
    const workerTarget = candidates[0];
    const { sessionId } = await parent.send("Target.attachToTarget", {
      targetId: workerTarget.targetId,
      flatten: false,
    });
    worker = nestedSession(parent, sessionId, commandTimeoutMs);
    const sessions = [main, worker];
    const enabled = await Promise.allSettled(sessions.map(async (session) => {
      await session.send("Profiler.enable");
      await session.send("Profiler.setSamplingInterval", { interval: samplingIntervalUs });
    }));
    if (errors(enabled).length) throw new AggregateError(errors(enabled), "Could not enable both CPU profilers");

    let state = "idle";
    let operation;
    let closing;
    return {
      metadata: {
        samplingIntervalUs,
        mainTarget,
        workerTarget,
        workerRouting: "Target.attachToTarget(flatten:false)",
        callFrameLocations: "raw bundled URLs and zero-based lines/columns",
      },
      start() {
        if (state !== "idle" || closing) return Promise.reject(new Error(`Cannot start CPU profiles in state ${state}`));
        state = "starting";
        operation = (async () => {
          const started = await Promise.allSettled(sessions.map((session) => session.send("Profiler.start")));
          const failures = errors(started);
          if (failures.length) {
            await Promise.allSettled(sessions.map((session, index) =>
              started[index].status === "fulfilled" ? session.send("Profiler.stop") : Promise.resolve()));
            state = "failed";
            throw new AggregateError(failures, "Could not start both CPU profilers");
          }
          state = "running";
        })();
        return operation;
      },
      stop() {
        if (state !== "running" || closing) return Promise.reject(new Error(`Cannot stop CPU profiles in state ${state}`));
        state = "stopping";
        operation = (async () => {
          const stopped = await Promise.allSettled(sessions.map((session) => session.send("Profiler.stop")));
          const failures = errors(stopped);
          state = failures.length ? "failed" : "idle";
          if (failures.length) throw new AggregateError(failures, "Could not collect both CPU profiles");
          return { main: stopped[0].value.profile, worker: stopped[1].value.profile };
        })();
        return operation;
      },
      close() {
        if (closing) return closing;
        closing = (async () => {
          // Cleanup must also work when a query, profiler command, or target has failed.
          if (operation) await operation.catch(() => {});
          if (state === "running") {
            await Promise.allSettled(sessions.map((session) => session.send("Profiler.stop")));
          }
          state = "closed";
          await Promise.allSettled(sessions.map((session) => session.send("Profiler.disable")));
          await Promise.allSettled([worker.detach(), main.detach()]);
          await parent.detach().catch(() => {});
        })();
        return closing;
      },
    };
  } catch (error) {
    await Promise.allSettled([
      worker?.detach(),
      main?.detach(),
      parent?.detach(),
    ]);
    throw error;
  }
}
