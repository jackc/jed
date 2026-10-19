import assert from "node:assert/strict";
import { test } from "node:test";
import { cancellationCheckpoint, hasCancellation, withCancellation } from "../src/cancel.ts";
import { EngineError } from "../src/errors.ts";
import { Meter } from "../src/cost.ts";
import { Planning, paramValue } from "../src/plan_params.ts";
import { intValue } from "../src/value.ts";
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
      timeSliceMs: 0,
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

test("nested sync drains preserve planning and the async scheduling budget", async () => {
  let tasks = 0;
  function* inner(): Steps<number> {
    assert.deepEqual(paramValue(0), intValue(1n));
    const planning = Planning.begin();
    planning.bind([intValue(2n)]);
    try {
      for (let i = 0; i < 100; i++) assert.equal(shouldCooperate(), false);
      yield* ioSteps(() => {
        assert.deepEqual(paramValue(0), intValue(2n));
        assert.equal(shouldCooperate(), false);
      });
      return 7;
    } finally {
      planning.end();
    }
  }
  function* outer(): Steps<number> {
    const planning = Planning.begin();
    planning.bind([intValue(1n)]);
    try {
      assert.equal(shouldCooperate(), false);
      assert.equal(runSync(inner()), 7);
      assert.deepEqual(paramValue(0), intValue(1n));
      assert.equal(shouldCooperate(), true);
      yield COOPERATE;
      assert.deepEqual(paramValue(0), intValue(1n));
      assert.equal(shouldCooperate(), false);
      return 8;
    } finally {
      planning.end();
    }
  }
  assert.equal(
    await runAsync(outer(), {
      quantum: 2,
      timeSliceMs: 0,
      schedule: (resume) => {
        tasks++;
        assert.equal(paramValue(0), null);
        schedule(resume);
      },
    }),
    8,
  );
  assert.equal(tasks, 1);
  assert.equal(paramValue(0), null);
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
      timeSliceMs: 0,
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

test("elapsed scheduling reads its clock only at sparse work checkpoints", async () => {
  let clockReads = 0;
  let tasks = 0;
  function* work(): Steps<number> {
    for (let i = 0; i < 15; i++) {
      const due = shouldCooperate();
      assert.equal(due, false);
      if (due) yield COOPERATE;
    }
    return 15;
  }
  assert.equal(
    await runAsync(work(), {
      quantum: 4,
      now: () => {
        clockReads++;
        return 0;
      },
      schedule: (resume) => {
        tasks++;
        schedule(resume);
      },
    }),
    15,
  );
  assert.equal(clockReads, 4, "one initial clock read plus three work checkpoints");
  assert.equal(tasks, 0);
});

test("default elapsed slice yields at one millisecond and restarts after a task", async () => {
  let now = 0;
  let clockReads = 0;
  const yieldedAt: number[] = [];
  function* work(): Steps<number> {
    for (let i = 0; i < 8; i++) {
      now += 0.25;
      if (shouldCooperate()) yield COOPERATE;
    }
    return 8;
  }
  assert.equal(
    await runAsync(work(), {
      quantum: 2,
      now: () => {
        clockReads++;
        return now;
      },
      schedule: (resume) => {
        yieldedAt.push(now);
        setImmediate(() => {
          now = yieldedAt.length * 100;
          resume();
        });
      },
    }),
    8,
  );
  assert.deepEqual(yieldedAt, [1, 101]);
  assert.equal(clockReads, 7, "initial, four checkpoints and two resumed slices");
});

test("scheduling changes neither completed values nor logical meter cost", async () => {
  function* work(meter: Meter): Steps<number> {
    let sum = 0;
    for (let i = 0; i < 20; i++) {
      if (shouldCooperate()) yield COOPERATE;
      meter.charge(3n);
      sum += i;
    }
    return sum;
  }
  const syncMeter = new Meter();
  const syncValue = runSync(work(syncMeter));
  for (const timeSliceMs of [0, 4]) {
    const meter = new Meter();
    assert.equal(
      await runAsync(work(meter), { quantum: 2, timeSliceMs, now: () => 0, schedule }),
      syncValue,
    );
    assert.equal(meter.accrued, syncMeter.accrued);
  }
});

test("hot async I/O preserves its deadline and shares sparse checkpoint accounting", async () => {
  let now = 0;
  let reads = 0;
  let clockReads = 0;
  const yieldedAfter: number[] = [];
  function* work(): Steps<number> {
    for (let i = 0; i < 8; i++) {
      yield* ioSteps(
        () => assert.fail("async provider must be used"),
        async () => {
          assert.equal(hasCancellation(), false);
          now += 0.5;
          reads++;
        },
      );
    }
    return reads;
  }
  assert.equal(
    await runAsync(work(), {
      quantum: 2,
      timeSliceMs: 2,
      now: () => {
        clockReads++;
        return now;
      },
      schedule: (resume) => {
        yieldedAfter.push(reads);
        setImmediate(() => {
          now += 100;
          resume();
        });
      },
    }),
    8,
  );
  assert.deepEqual(yieldedAfter, [4, 8]);
  assert.equal(clockReads, 7);
});

test("elapsed resident slice admits message cancellation and unwinds once", async () => {
  let now = 0;
  let canceled = false;
  let completed = 0;
  let cleaned = 0;
  function* work(): Steps<void> {
    try {
      for (;;) {
        now += 0.25;
        if (shouldCooperate()) yield COOPERATE;
        completed++;
      }
    } finally {
      yield* ioSteps(
        () => assert.fail("async cleanup must be used"),
        async () => {
          assert.equal(hasCancellation(), false);
          cleaned++;
        },
      );
    }
  }
  await assert.rejects(
    runAsync(work(), {
      quantum: 2,
      now: () => now,
      check: () => canceled,
      schedule: (resume) =>
        setImmediate(() => {
          assert.equal(completed, 3);
          canceled = true;
          resume();
        }),
    }),
    isCanceled,
  );
  assert.equal(completed, 3);
  assert.equal(cleaned, 1);
});

test("synchronous I/O fallback still admits cancellation before invoking the device", async () => {
  let canceled = false;
  let cleaned = 0;
  function* work(): Steps<void> {
    try {
      yield* ioSteps(() => assert.fail("canceled device callback must not run"));
    } finally {
      cleaned++;
    }
  }
  await assert.rejects(
    runAsync(work(), {
      timeSliceMs: 1000,
      now: () => 0,
      check: () => canceled,
      schedule: (resume) =>
        setImmediate(() => {
          canceled = true;
          resume();
        }),
    }),
    isCanceled,
  );
  assert.equal(cleaned, 1);
});

test("each async run starts its own elapsed slice and sync drains read no clock", async () => {
  let now = 0;
  let clockReads = 0;
  let tasks = 0;
  function* work(): Steps<number> {
    for (let i = 0; i < 4; i++) {
      now += 0.125;
      if (shouldCooperate()) yield COOPERATE;
    }
    return 4;
  }
  const options = {
    quantum: 2,
    now: () => {
      clockReads++;
      return now;
    },
    schedule: (resume: () => void) => {
      tasks++;
      schedule(resume);
    },
  };
  assert.equal(await runAsync(work(), options), 4);
  now = 100;
  assert.equal(await runAsync(work(), options), 4);
  assert.equal(clockReads, 6);
  assert.equal(tasks, 0);
  assert.equal(runSync(work()), 4);
  assert.equal(clockReads, 6);
});

test("invalid elapsed slices are rejected before starting the continuation", async () => {
  function* work(): Steps<void> {
    yield* ioSteps(() => assert.fail("invalid scheduling options must not enter the generator"));
  }
  for (const timeSliceMs of [-1, Number.NaN, Number.POSITIVE_INFINITY]) {
    await assert.rejects(runAsync(work(), { timeSliceMs }), RangeError);
  }
});
