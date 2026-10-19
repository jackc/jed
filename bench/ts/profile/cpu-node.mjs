// Benchmark-only sampling of the current isolate. Import this module inside a worker to
// profile that worker; connectToMainThread() would measure the wrong execution context.
import { Session } from "node:inspector";
import { isMainThread, threadId } from "node:worker_threads";

const states = new WeakMap();
let active;

function post(session, method, params = {}) {
  return new Promise((resolve, reject) => {
    session.post(method, params, (error, result) => {
      if (error) reject(error);
      else resolve(result);
    });
  });
}

async function disconnect(state) {
  try {
    await post(state.session, "Profiler.disable");
  } catch {
    // Preserve an earlier start/stop error if the inspector session has already failed.
  } finally {
    state.session.disconnect();
    if (active === state) active = undefined;
  }
}

/** Start CPU sampling on this main thread or worker. The returned handle stays in this isolate. */
export async function startCpuProfile({ samplingIntervalUs = 1000 } = {}) {
  if (!Number.isSafeInteger(samplingIntervalUs) || samplingIntervalUs <= 0) {
    throw new RangeError("samplingIntervalUs must be a positive safe integer");
  }
  if (active !== undefined) throw new Error("a benchmark CPU profile is already active in this isolate");
  const session = new Session();
  const state = { session, stop: undefined };
  active = state;
  try {
    session.connect();
    await post(session, "Profiler.enable");
    await post(session, "Profiler.setSamplingInterval", { interval: samplingIntervalUs });
    await post(session, "Profiler.start");
    const handle = Object.freeze({ samplingIntervalUs, threadId, isMainThread });
    states.set(handle, state);
    return handle;
  } catch (error) {
    await disconnect(state);
    throw error;
  }
}

/** Stop once and return the raw CDP Profile. The caller decides where/how to save it. */
export function stopCpuProfile(handle) {
  const state = states.get(handle);
  if (state === undefined) return Promise.reject(new TypeError("unknown CPU profile handle"));
  if (state.stop === undefined) {
    state.stop = (async () => {
      try {
        const { profile } = await post(state.session, "Profiler.stop");
        if (!profile || !Array.isArray(profile.nodes)) throw new Error("inspector returned no CPU profile");
        return profile;
      } finally {
        await disconnect(state);
      }
    })();
  }
  return state.stop;
}
