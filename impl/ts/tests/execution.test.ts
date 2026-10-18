import assert from "node:assert/strict";
import { test } from "node:test";
import { cancellationCheckpoint, hasCancellation, withCancellation } from "../src/cancel.ts";
import { EngineError } from "../src/errors.ts";
import {
  COOPERATE,
  callSteps,
  ioSteps,
  runAsync,
  runSync,
  sequenceSteps,
  shouldCooperate,
  type SequenceSteps,
  type Steps,
} from "../src/execution.ts";

const schedule = (callback: () => void) => setImmediate(callback);
const isCanceled = (error: unknown) => error instanceof EngineError && error.code() === "57014";

test("sync and async drivers resume one traversal without replay", async () => {
  const calls: string[] = [];
  function* work(): Steps<number> {
    calls.push("start");
    const value = yield* ioSteps(
      () => {
        calls.push("sync");
        return 7;
      },
      async () => {
        assert.equal(hasCancellation(), false);
        calls.push("async");
        return 7;
      },
    );
    calls.push("resume");
    return value + 1;
  }
  assert.equal(runSync(work()), 8);
  assert.equal(await runAsync(work(), { schedule, poll: () => false }), 8);
  assert.deepEqual(calls, ["start", "sync", "resume", "start", "async", "resume"]);
});

test("cancellation after suspended I/O enters finally and awaits cleanup once", async () => {
  let release!: () => void;
  let started!: () => void;
  const ready = new Promise<void>((resolve) => {
    started = resolve;
  });
  const pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  let canceled = false;
  let cleaned = 0;
  let resumed = 0;
  function* work(): Steps<void> {
    try {
      yield* ioSteps(
        () => {},
        async () => {
          started();
          await pending;
        },
      );
      resumed++;
    } finally {
      yield* ioSteps(
        () => {
          cleaned++;
        },
        async () => {
          cleaned++;
        },
      );
    }
  }
  const result = runAsync(work(), { check: () => canceled, poll: () => canceled, schedule });
  await ready;
  assert.equal(hasCancellation(), false);
  canceled = true;
  release();
  await assert.rejects(result, isCanceled);
  assert.equal(resumed, 0);
  assert.equal(cleaned, 1);
  assert.equal(hasCancellation(), false);
});

test("cooperative resident loops admit cancellation without a shared flag", async () => {
  let canceled = false;
  let count = 0;
  let cleaned = 0;
  function* work(): Steps<void> {
    try {
      for (;;) {
        if (shouldCooperate()) yield COOPERATE;
        count++;
      }
    } finally {
      cleaned++;
    }
  }
  await assert.rejects(
    runAsync(work(), {
      check: () => canceled,
      quantum: 4,
      schedule: (resume) =>
        setImmediate(() => {
          canceled = true;
          resume();
        }),
    }),
    isCanceled,
  );
  assert.equal(count, 3);
  assert.equal(cleaned, 1);
});

test("poll cancellation inside a segment and I/O failures unwind resources", async () => {
  let cleaned = 0;
  let canceled = false;
  function* polled(): Steps<void> {
    try {
      yield COOPERATE;
      canceled = true;
      cancellationCheckpoint();
    } finally {
      yield* ioSteps(
        () => {
          cancellationCheckpoint();
        },
        async () => {},
      );
      cancellationCheckpoint(); // Sticky cancellation must be masked throughout cleanup.
      cleaned++;
    }
  }
  await assert.rejects(runAsync(polled(), { poll: () => canceled, schedule }), isCanceled);
  canceled = false;
  assert.throws(
    () =>
      withCancellation(
        () => canceled,
        () => runSync(polled()),
      ),
    isCanceled,
  );
  function* failed(): Steps<void> {
    try {
      yield* ioSteps(() => {
        throw new Error("read failed");
      });
    } finally {
      cleaned++;
    }
  }
  await assert.rejects(runAsync(failed(), { schedule }), /read failed/);
  assert.equal(cleaned, 3);
});

test("hot async I/O yields tasks and sequence close runs cleanup without pulling another row", async () => {
  let tasks = 0;
  let reads = 0;
  let cleaned = 0;
  function* sequence(): SequenceSteps<number> {
    try {
      for (let i = 0; i < 8; i++) {
        yield* ioSteps(
          () => reads++,
          async () => reads++,
        );
      }
      yield { kind: "item", value: 1 };
      reads++;
      yield { kind: "item", value: 2 };
    } finally {
      cleaned++;
    }
  }
  const iterator = sequenceSteps(sequence());
  assert.deepEqual(
    await runAsync(iterator.nextSteps(), {
      quantum: 2,
      schedule: (resume) =>
        setImmediate(() => {
          tasks++;
          resume();
        }),
    }),
    { done: false, value: 1 },
  );
  assert.equal(tasks, 4);
  runSync(iterator.returnSteps!());
  assert.equal(reads, 8);
  assert.equal(cleaned, 1);
});

test("recursive calls use an iterative driver stack for sync and async execution", async () => {
  function* descend(depth: number): Steps<number> {
    if (depth === 0) return 1;
    return 1 + (yield* callSteps(descend(depth - 1)));
  }
  assert.equal(runSync(descend(10000)), 10001);
  assert.equal(await runAsync(descend(10000), { schedule }), 10001);
});

test("call effects return child errors to parent catch and resume awaited finally blocks", async () => {
  const events: string[] = [];
  function* child(): Steps<number> {
    try {
      yield* ioSteps(() => {
        throw new Error("child failed");
      });
    } finally {
      events.push("child finally");
    }
    return 0;
  }
  function* cleanup(): Steps<void> {
    yield* ioSteps(
      () => {
        events.push("cleanup");
      },
      async () => {
        events.push("cleanup");
      },
    );
  }
  function* parent(): Steps<number> {
    try {
      return yield* callSteps(child());
    } catch (error) {
      assert.match(String(error), /child failed/);
      events.push("parent catch");
      return 7;
    } finally {
      yield* callSteps(cleanup());
    }
  }
  assert.equal(runSync(parent()), 7);
  assert.equal(await runAsync(parent(), { schedule }), 7);
  assert.deepEqual(events, [
    "child finally",
    "parent catch",
    "cleanup",
    "child finally",
    "parent catch",
    "cleanup",
  ]);
});

test("cancellation unwinds a deep call stack and cleanup calls exactly once", async () => {
  let canceled = false;
  let cleaned = 0;
  function* cleanup(): Steps<void> {
    yield* ioSteps(
      () => {},
      async () => {},
    );
    cancellationCheckpoint();
    cleaned++;
  }
  function* descend(depth: number): Steps<void> {
    try {
      if (depth > 0) yield* callSteps(descend(depth - 1));
      else yield COOPERATE;
    } finally {
      yield* callSteps(cleanup());
    }
  }
  await assert.rejects(
    runAsync(descend(512), {
      check: () => canceled,
      poll: () => canceled,
      schedule: (resume) =>
        setImmediate(() => {
          canceled = true;
          resume();
        }),
    }),
    isCanceled,
  );
  assert.equal(cleaned, 513);
});
