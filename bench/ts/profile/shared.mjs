// Benchmark-only adapters retain production RuntimeConnection/RuntimeServer and host scheduling.
// Diagnostic records are retrieved after the timed drain; ordinary replies carry no telemetry.
export const LIMITS = { maxBatchRows: 512, maxBatchBytes: 262144, maxResultBytes: 2097152 };

export function profileWorker({ RuntimeServer, open, schedule, send, stages, cpu }) {
  let enabled = false;
  let requests = new Map();
  let rpcs = [];
  let cpuHandle;
  const server = new RuntimeServer({
    open,
    schedule,
    send(reply, transfer) {
      if (enabled) {
        const request = requests.get(reply.id);
        if (request) {
          rpcs.push({ id: reply.id, op: request.op, ms: performance.now() - request.at,
            rows: reply.result?.rows?.length ?? 0,
            packetBytes: reply.result?.rows?.data?.byteLength ?? 0 });
          requests.delete(reply.id);
        }
      }
      send(reply, transfer);
    },
  });
  return async (request) => {
    if (request.profile) {
      let result;
      try {
        switch (request.profile) {
          case "start":
            enabled = request.enabled;
            requests = new Map();
            rpcs = [];
            stages.resetStages(enabled);
            break;
          case "stop":
            result = { stages: stages.snapshotStages(), rpcs, pending: requests.size };
            enabled = false;
            stages.resetStages(false);
            break;
          case "cpu.start": cpuHandle = await cpu.startCpuProfile(); break;
          case "cpu.stop": result = await cpu.stopCpuProfile(cpuHandle); cpuHandle = undefined; break;
          default: throw new Error(`Unknown profile command ${request.profile}`);
        }
        send({ profile: request.profile, token: request.token, result });
      } catch (error) {
        send({ profile: request.profile, token: request.token, error: String(error.stack ?? error) });
      }
      return;
    }
    if (enabled) requests.set(request.id, { op: request.op, at: performance.now() });
    server.receive(request);
  };
}

export function profileClient(transport, core, sharedCancellation) {
  let enabled = false;
  let receive;
  let token = 0;
  let requests = new Map();
  let rpcs = [];
  const controls = new Map();
  transport.onMessage((reply) => {
    if (reply.profile) {
      const waiter = controls.get(reply.token);
      controls.delete(reply.token);
      if (reply.error) waiter?.reject(new Error(reply.error));
      else waiter?.resolve(reply.result);
      return;
    }
    if (enabled) {
      const request = requests.get(reply.id);
      if (request) {
        rpcs.push({ id: reply.id, op: request.op, ms: performance.now() - request.at });
        requests.delete(reply.id);
      }
    }
    receive(reply);
  });
  const control = (profile, args = {}) => new Promise((resolve, reject) => {
    const id = ++token;
    controls.set(id, { resolve, reject });
    transport.postMessage({ profile, token: id, ...args });
  });
  const connection = new core.RuntimeConnection({
    postMessage(request) {
      if (enabled) requests.set(request.id, { op: request.op, at: performance.now() });
      transport.postMessage(request);
    },
    onMessage(callback) { receive = callback; },
    onFailure: (callback) => transport.onFailure(callback),
    terminate: () => transport.terminate(),
  }, LIMITS, sharedCancellation);
  return {
    connection, control,
    async start(collect) {
      enabled = collect;
      requests = new Map();
      rpcs = [];
      core.stages.resetStages(collect);
      await control("start", { enabled: collect });
    },
    async stop() {
      enabled = false;
      const mainStages = core.stages.snapshotStages();
      core.stages.resetStages(false);
      const worker = await control("stop");
      if (requests.size || worker.pending) throw new Error("Unfinished profiling RPC");
      return { mainStages, mainRpcs: rpcs, worker };
    },
  };
}

export async function createSuite(core, endpoint, heavy, settings) {
  const config = heavy.heavyConfigFrom(settings);
  const cases = heavy.heavyCases(config);
  const directDb = core.buildInMemory(8192);
  const direct = directDb.session();
  await endpoint.connection.call("open", { limits: LIMITS });
  const workerDb = new core.AsyncDatabase(endpoint.connection);
  const worker = await workerDb.session();
  const prepared = new Map();
  const equal = (actual, expected) => {
    if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error("Result/cost mismatch");
  };
  function metadata(rows, count) {
    return { rows: count, columnNames: [...rows.columnNames], columnTypes: [...rows.columnTypes],
      rowsAffected: rows.rowsAffected, cost: rows.cost.toString() };
  }
  function directDrain(sql, consumer) {
    const start = performance.now();
    const rows = direct.query(sql);
    const opened = performance.now();
    const iterator = rows[Symbol.iterator]();
    const first = iterator.next();
    const firstAt = performance.now();
    const fingerprint = consumer === "fingerprint" ? new heavy.Fingerprint() : undefined;
    let count = 0;
    let result, drained;
    try {
      for (let step = first; !step.done; step = iterator.next()) {
        count++;
        if (fingerprint) fingerprint.row(step.value);
      }
      result = fingerprint ? fingerprint.result(rows) : metadata(rows, count);
      drained = performance.now();
    } finally { rows.close(); }
    const end = performance.now();
    return { totalMs: end - start, queryMs: opened - start, firstPullMs: firstAt - opened,
      restMs: drained - firstAt, closeMs: end - drained, result };
  }
  async function workerDrain(sql, consumer) {
    const start = performance.now();
    const rows = await worker.query(sql);
    const opened = performance.now();
    const first = await rows.next();
    const firstAt = performance.now();
    const fingerprint = consumer === "fingerprint" ? new heavy.Fingerprint() : undefined;
    let count = 0;
    let result, drained;
    try {
      for (let step = first; !step.done; step = await rows.next()) {
        count++;
        if (fingerprint) fingerprint.row(step.value);
      }
      result = fingerprint ? fingerprint.result(rows) : metadata(rows, count);
      drained = performance.now();
    } finally { await rows.close(); }
    const end = performance.now();
    return { totalMs: end - start, queryMs: opened - start, firstPullMs: firstAt - opened,
      restMs: drained - firstAt, closeMs: end - drained, result };
  }
  return {
    config, cases, endpoint,
    async warm(id, warmups = config.warmups) {
      const workload = cases.find((c) => c.id === id);
      if (!workload) throw new Error(`Unknown case ${id}`);
      if (!prepared.has(id)) {
        for (const sql of workload.setup) { direct.execute(sql); await worker.execute(sql); }
        const expected = heavy.runDirectQuery(direct, workload.sql);
        equal(await heavy.runWorkerQuery(worker, workload.sql), expected);
        if (expected.rows !== workload.expectedRows) throw new Error("Unexpected result rows");
        prepared.set(id, { workload, expected });
      }
      for (let i = 0; i < warmups; i++) {
        equal(directDrain(workload.sql, "fingerprint").result, prepared.get(id).expected);
        equal((await workerDrain(workload.sql, "fingerprint")).result, prepared.get(id).expected);
      }
      return prepared.get(id).expected;
    },
    async run(id, boundary, consumer = "fingerprint", collect = false) {
      const { workload, expected } = prepared.get(id);
      await endpoint.start(collect);
      const sample = boundary === "direct" ? directDrain(workload.sql, consumer)
        : await workerDrain(workload.sql, consumer);
      const diagnostic = await endpoint.stop();
      if (consumer === "fingerprint") equal(sample.result, expected);
      else { const { fingerprint, ...rest } = expected; equal(sample.result, rest); }
      return { boundary, consumer, ...sample, diagnostic };
    },
    async close() {
      direct.close(); directDb.close(); await worker.close(); await workerDb.close();
    },
  };
}
