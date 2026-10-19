// Snapshot-only counters. Each JS realm owns its own module state; no cross-thread clocks or
// per-event arrays are retained. Timing sums are milliseconds; add() may record other units.
let enabled = false;
let stages = new Map();

export function resetStages(measure = true) {
  enabled = measure;
  stages = new Map();
}

export function snapshotStages() {
  return Object.fromEntries(
    [...stages].map(([name, value]) => [name, { sum: value.sum, count: value.count }]),
  );
}

export function add(name, value, count = 1) {
  if (!enabled) return;
  const stage = stages.get(name);
  if (stage === undefined) stages.set(name, { sum: value, count });
  else {
    stage.sum += value;
    stage.count += count;
  }
}

export function begin(_name) {
  return enabled ? performance.now() : -1;
}

export function end(name, start) {
  if (start >= 0 && enabled) add(name, performance.now() - start);
}

// Deliberately synchronous: timing a returned Promise would measure invocation only. Awaited
// intervals use begin/end around the existing await, without adding another Promise boundary.
export function timed(name, operation) {
  if (!enabled) return operation();
  const start = begin(name);
  try {
    return operation();
  } finally {
    end(name, start);
  }
}
