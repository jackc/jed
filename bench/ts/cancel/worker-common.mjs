// Benchmark instrumentation only. RuntimeServer's TypeScript private fields are ordinary
// properties: inspect activeJob without changing the semantic core or its scheduling policy.
export function createProbeServer({ RuntimeServer, open, schedule, send }) {
  let sample;
  const now = () => performance.now();
  const server = new RuntimeServer({
    open,
    schedule(callback) {
      const request = server.activeJob?.request;
      if (
        sample?.queryId !== undefined &&
        !sample.marker &&
        (request?.op === "query" || request?.op === "fetch")
      ) {
        sample.marker = { workerAtMs: now(), operation: request.op, requestId: request.id };
        send({ probe: "active", token: sample.token, ...sample.marker });
      }
      schedule(callback);
    },
    send(reply, transfer) {
      if (sample?.requestIds.has(reply.id) && reply.error) {
        sample.errorSentAtMs = now();
        sample.errorCode = reply.error.code;
      }
      send(reply, transfer);
    },
  });
  return (request) => {
    if (request.probe === "arm") {
      sample = { token: request.token, sql: request.sql, requestIds: new Set() };
      send({ probe: "armed", token: request.token });
      return;
    }
    if (request.probe === "finish") {
      const result = sample;
      sample = undefined;
      send({
        probe: "finished",
        token: request.token,
        worker: result && { ...result, requestIds: [...result.requestIds] },
      });
      return;
    }
    if (sample) {
      if (request.op === "query" && request.args.sql === sample.sql) {
        sample.queryId = request.id;
        sample.admittedAtMs = now();
        sample.sharedFlagPresent = request.cancel !== undefined;
      }
      if (sample.queryId !== undefined && (request.op === "query" || request.op === "fetch"))
        sample.requestIds.add(request.id);
      if (request.op === "cancel" && sample.requestIds.has(request.args.request)) {
        sample.cancelReceivedAtMs ??= now();
      }
    }
    server.receive(request);
  };
}
