// Shared Node/browser CPU-profile analysis. No filesystem access, runtime instrumentation or
// source-map dependency: callers optionally supply a synchronous sourceMap(callFrame) callback.
// CDP lineNumber/columnNumber stay zero-based; display line/column fields are one-based.

export const CPU_BUCKETS = Object.freeze([
  "gc",
  "idle",
  "parse-plan",
  "sort",
  "execution",
  "scheduler",
  "codec-measure",
  "codec-encode",
  "codec-validation",
  "codec-decode",
  "codec-other",
  "client-driver",
  "promise-runtime",
  "consumer",
  "transport",
  "profiler",
  "other",
]);

function hasFile(frame, ending) {
  return frame.url.replaceAll("\\", "/").split(/[?#]/, 1)[0].endsWith(ending);
}

function named(frame, expression) {
  return expression.test(frame.functionName) ||
    (frame.generated !== undefined && expression.test(frame.generated.functionName));
}

/** Best-effort classification of a root-to-leaf stack; exact frames remain in the summary. */
export function classifyCpuStack(stack) {
  const leaf = stack.at(-1);
  if (!leaf) return "other";
  if (named(leaf, /^\(garbage collector\)$|^GC$|GarbageCollect/i)) return "gc";
  if (named(leaf, /^\(idle\)$/)) return "idle";
  if (hasFile(leaf, "/cpu-node.mjs") || leaf.url === "node:inspector") return "profiler";
  if (named(leaf, /^postMessage$/) && stack.some((frame) =>
    hasFile(frame, "/profile/browser-worker.mjs") && named(frame, /^schedule$/)
  )) return "scheduler";
  if (named(leaf, /^(?:postMessage|structuredClone|serialize|deserialize)$/)) return "transport";
  if (
    (leaf.url.startsWith("node:") || leaf.url === "") &&
    named(leaf, /promise|microtask|processTicksAndRejections|runNextTicks/i)
  ) return "promise-runtime";

  const fromLeaf = [...stack].reverse();
  const packed = fromLeaf.filter((frame) => hasFile(frame, "/async/packed.ts"));
  if (packed.length > 0) {
    const reading = packed.some((frame) => named(frame, /^(?:decodePackedRows|PackedReader|row)$/));
    const writing = packed.some((frame) => named(frame, /^(?:PackedRowsWriter|append|finish)$/));
    const wire = fromLeaf.find((frame) => hasFile(frame, "/async/wire.ts"));
    if (reading) {
      // Only the preflight reader calls WireBudget.reserve; other PackedReader functions
      // are shared by validation and construction and cannot be separated by sampled stacks.
      return wire && named(wire, /^(?:reserve|WireBudget)$/) ? "codec-validation" : "codec-decode";
    }
    if (writing) return wire ? "codec-measure" : "codec-encode";
    return "codec-other";
  }

  const wire = fromLeaf.filter((frame) => hasFile(frame, "/async/wire.ts"));
  if (wire.length > 0) {
    const measuring = wire.some((frame) => named(frame, /^measure/));
    if (wire.some((frame) => named(frame, /^decodeValues?$/))) {
      return measuring ? "codec-validation" : "codec-decode";
    }
    if (wire.some((frame) => named(frame, /^encodeValues?$/))) {
      return measuring ? "codec-measure" : "codec-encode";
    }
    // Standalone measureValues can be client collector accounting, not result encoding.
    if (stack.some((frame) => hasFile(frame, "/async/client.ts"))) return "client-driver";
  }

  for (const frame of fromLeaf) {
    if (
      hasFile(frame, "/src/async-heavy.ts") &&
      named(frame, /^(?:Fingerprint|word|value|row|result)$/)
    ) return "consumer";
    if (
      /\/(?:parser|lexer|resolve|optimize)\.ts(?:[?#]|$)/.test(frame.url) ||
      (hasFile(frame, "/src/executor.ts") && named(frame, /^plan(?:Query|Select|Insert|Update|Delete)/))
    ) return "parse-plan";
    // plan_params.withPlanningScope wraps every async resume; its descendants are not
    // necessarily planning. Only actual planner/parser frames form a phase boundary.
    if (named(frame, /^(?:sort|sortRows(?:Collated)?|sortSpool|sortScanCandidates|sortOsaVals|sortAggOrderRows|sortBy|compareSort|compareOrder|cmpRowsByOrder|cmpWindowRows|ArrayPrototypeSort)$/i)) return "sort";
    if (hasFile(frame, "/async/client.ts")) return "client-driver";
    if (
      /\/(?:async\/node|browser\/async)\.ts(?:[?#]|$)/.test(frame.url) &&
      named(frame, /^(?:postMessage|onMessage|receive)$/)
    ) return "transport";
    // A scalar helper under fingerprinting belongs to the consumer, so defer the general
    // engine fallback until every more specific enclosing boundary has been inspected.
  }
  if (stack.some((frame) => /\bimpl\/ts\/src\//.test(frame.url))) return "execution";
  return "other";
}

function originalFrame(frame) {
  return {
    functionName: typeof frame?.functionName === "string" ? frame.functionName : "",
    url: typeof frame?.url === "string" ? frame.url : "",
    scriptId: typeof frame?.scriptId === "string" ? frame.scriptId : "",
    lineNumber: Number.isInteger(frame?.lineNumber) ? frame.lineNumber : -1,
    columnNumber: Number.isInteger(frame?.columnNumber) ? frame.columnNumber : -1,
  };
}

function mappedFrame(callFrame, sourceMap) {
  const generated = originalFrame(callFrame);
  const mapped = sourceMap?.({ ...generated });
  if (mapped != null && (typeof mapped !== "object" || typeof mapped.then === "function")) {
    throw new TypeError("sourceMap must synchronously return a call-frame object, null or undefined");
  }
  const frame = mapped == null ? generated : originalFrame({ ...generated, ...mapped });
  return {
    ...frame,
    line: frame.lineNumber < 0 ? null : frame.lineNumber + 1,
    column: frame.columnNumber < 0 ? null : frame.columnNumber + 1,
    sourceMapped: mapped != null,
    generated,
  };
}

function frameKey(frame) {
  return JSON.stringify([
    frame.url, frame.lineNumber, frame.columnNumber, frame.functionName,
    frame.generated.scriptId, frame.generated.url, frame.generated.lineNumber,
    frame.generated.columnNumber, frame.generated.functionName,
  ]);
}

function limitValue(value, name) {
  if (value !== Infinity && (!Number.isSafeInteger(value) || value < 0)) {
    throw new RangeError(`${name} must be a non-negative integer or Infinity`);
  }
  return value;
}

function percentage(part, total) {
  return total === 0 ? 0 : (part / total) * 100;
}

function timingDeltaSummary(values) {
  if (values.length === 0) return { min: null, p50: null, p95: null, max: null };
  const sorted = [...values].sort((a, b) => a - b);
  return {
    min: sorted[0],
    p50: sorted[Math.floor(sorted.length * 0.5)],
    p95: sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * 0.95))],
    max: sorted.at(-1),
  };
}

/**
 * Summarize a CDP/V8 profile without altering it. sourceMap receives/returns zero-based CDP
 * call frames. classify(stack, defaultBucket) may override the default bucket or return
 * undefined; stack is root-to-leaf. No inference from hitCount replaces missing sample timing.
 */
export function summarizeCpuProfile(profile, {
  sourceMap,
  classify,
  hotspotLimit = 50,
  stackLimit = 50,
} = {}) {
  if (!profile || !Array.isArray(profile.nodes) || !Array.isArray(profile.samples) ||
      !Array.isArray(profile.timeDeltas)) {
    throw new TypeError("CPU profile must include nodes, samples and timeDeltas");
  }
  if (profile.samples.length !== profile.timeDeltas.length) {
    throw new RangeError("CPU samples and timeDeltas must have matching lengths");
  }
  if (!Number.isFinite(profile.startTime) || !Number.isFinite(profile.endTime) ||
      profile.endTime < profile.startTime) {
    throw new RangeError("CPU profile has invalid start/end times");
  }
  if (sourceMap !== undefined && typeof sourceMap !== "function") throw new TypeError("sourceMap must be a function");
  if (classify !== undefined && typeof classify !== "function") throw new TypeError("classify must be a function");
  limitValue(hotspotLimit, "hotspotLimit");
  limitValue(stackLimit, "stackLimit");

  const nodes = new Map();
  const parents = new Map();
  for (const node of profile.nodes) {
    if (!Number.isSafeInteger(node.id) || nodes.has(node.id)) throw new TypeError("CPU profile has duplicate/invalid node IDs");
    nodes.set(node.id, { frame: mappedFrame(node.callFrame, sourceMap), original: node });
  }
  for (const node of profile.nodes) {
    for (const child of node.children ?? []) {
      if (!nodes.has(child) || parents.has(child)) throw new TypeError("CPU profile is not a valid call tree");
      parents.set(child, node.id);
    }
  }

  const weights = new Map();
  let sampledUs = 0;
  for (let i = 0; i < profile.samples.length; i++) {
    const id = profile.samples[i];
    const delta = profile.timeDeltas[i];
    if (!nodes.has(id)) throw new TypeError(`CPU sample refers to missing node ${id}`);
    if (!Number.isFinite(delta) || delta < 0) throw new RangeError(`invalid CPU timeDelta at sample ${i}`);
    const weight = weights.get(id) ?? { us: 0, samples: 0 };
    weight.us += delta;
    weight.samples++;
    weights.set(id, weight);
    sampledUs += delta;
  }

  const buckets = new Map(CPU_BUCKETS.map((name) => [name, { us: 0, samples: 0 }]));
  const functions = new Map();
  const stacks = [];
  for (const [id, weight] of weights) {
    const ids = [];
    const seen = new Set();
    let cursor = id;
    while (cursor !== undefined) {
      if (seen.has(cursor)) throw new TypeError("CPU profile contains a call-tree cycle");
      seen.add(cursor);
      ids.push(cursor);
      cursor = parents.get(cursor);
    }
    ids.reverse();
    const frames = ids.map((nodeId) => nodes.get(nodeId).frame);
    const fallback = classifyCpuStack(frames);
    const bucketName = classify?.(frames, fallback) ?? fallback;
    if (typeof bucketName !== "string" || bucketName.length === 0) throw new TypeError("classify must return a bucket name or undefined");
    const bucket = buckets.get(bucketName) ?? { us: 0, samples: 0 };
    bucket.us += weight.us;
    bucket.samples += weight.samples;
    buckets.set(bucketName, bucket);
    stacks.push({ nodeId: id, bucket: bucketName, selfMs: weight.us / 1000,
      samples: weight.samples, percent: percentage(weight.us, sampledUs), frames });

    // Function inclusive time counts a recursive frame at most once per sampled stack.
    const included = new Set();
    for (let i = 0; i < frames.length; i++) {
      const frame = frames[i];
      const key = frameKey(frame);
      let entry = functions.get(key);
      if (!entry) {
        entry = { frame, selfUs: 0, inclusiveUs: 0, samples: 0, nodeIds: new Set(), buckets: new Map() };
        functions.set(key, entry);
      }
      entry.nodeIds.add(ids[i]);
      if (!included.has(key)) entry.inclusiveUs += weight.us;
      included.add(key);
      if (i === frames.length - 1) {
        entry.selfUs += weight.us;
        entry.samples += weight.samples;
        entry.buckets.set(bucketName, (entry.buckets.get(bucketName) ?? 0) + weight.us);
      }
    }
  }

  const hotspots = [...functions.values()].map((entry) => ({
    frame: entry.frame,
    selfMs: entry.selfUs / 1000,
    inclusiveMs: entry.inclusiveUs / 1000,
    samples: entry.samples,
    percent: percentage(entry.selfUs, sampledUs),
    nodeIds: [...entry.nodeIds].sort((a, b) => a - b),
    selfByBucketMs: Object.fromEntries([...entry.buckets].map(([name, us]) => [name, us / 1000])),
  })).sort((a, b) => b.selfMs - a.selfMs || b.inclusiveMs - a.inclusiveMs ||
    (a.frame.url < b.frame.url ? -1 : a.frame.url > b.frame.url ? 1 : 0) ||
    a.frame.lineNumber - b.frame.lineNumber);
  stacks.sort((a, b) => b.selfMs - a.selfMs || a.nodeId - b.nodeId);
  const idleUs = buckets.get("idle")?.us ?? 0;
  const durationUs = profile.endTime - profile.startTime;
  return {
    profileDurationMs: durationUs / 1000,
    sampledMs: sampledUs / 1000,
    activeSampledMs: (sampledUs - idleUs) / 1000,
    unsampledTailMs: Math.max(0, durationUs - sampledUs) / 1000,
    sampleOverrunMs: Math.max(0, sampledUs - durationUs) / 1000,
    sampleCount: profile.samples.length,
    nodeCount: nodes.size,
    mappedNodeCount: [...nodes.values()].filter((node) => node.frame.sourceMapped).length,
    timeDeltaUs: timingDeltaSummary(profile.timeDeltas),
    buckets: [...buckets].map(([name, value]) => ({
      name, selfMs: value.us / 1000, samples: value.samples,
      percent: percentage(value.us, sampledUs),
    })),
    hotspotCount: hotspots.length,
    hotspots: hotspots.slice(0, hotspotLimit),
    stackCount: stacks.length,
    stacks: stacks.slice(0, stackLimit),
    methodology: [
      "Each sample's timeDelta, in microseconds, is assigned to its sampled leaf; buckets partition sampled self-time without double counting.",
      "These are interval-weighted CPU samples, including idle samples, not exact stage timers or operating-system thread CPU accounting. Unsampled tail and overrun are reported separately.",
      "Inclusive function time includes descendants and is deduplicated for recursion within each stack; inclusive times must not be summed across functions.",
      "Buckets are best-effort source/ancestor classification. codec-measure covers writer preflight; codec-validation isolates identifiable checks. codec-decode includes reader functions shared by validation and construction, which ordinary sampled stacks cannot separate.",
      "Collector budget checks without a packed codec ancestor are not assumed to be encoding. Preserve exact mapped/generated frames and call stacks when interpreting bucket totals.",
      "Browser worker postMessage samples under its schedule callback are local MessageChannel scheduling, not result transport; only that identifiable stack is assigned to scheduler.",
      "Source-map callbacks use zero-based CDP coordinates. Display line/column are one-based; generated coordinates and script IDs are retained.",
    ],
  };
}
