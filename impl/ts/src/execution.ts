// Explicit continuation effects shared by the synchronous tooling API and worker execution.
// A driver resumes the SAME generator after I/O; no statement, row, or expression is replayed.
import { currentCancellationCheck, withCancellationScope } from "./cancel.ts";
import { engineError } from "./errors.ts";
import { currentPlanningFrames, withPlanningScope, type PlanningFrames } from "./plan_params.ts";

export type ExecutionEffect =
  | { kind: "io"; sync: () => unknown; async?: () => Promise<unknown> }
  | { kind: "cooperate" }
  | { kind: "call"; steps: Steps<unknown> };
export type Steps<T> = Generator<ExecutionEffect, T, unknown>;
export interface StepIterator<T> {
  nextSteps(): Steps<IteratorResult<T>>;
  returnSteps?(): Steps<IteratorResult<T>>;
}
export type SequenceSteps<T> = Generator<
  ExecutionEffect | { kind: "item"; value: T },
  void,
  unknown
>;

// A row traversal yields tagged items beside effects. Its iterator keeps the traversal suspended
// between rows, forwarding failures into it so nested resource finally blocks unwind once.
export function sequenceSteps<T>(sequence: SequenceSteps<T>): StepIterator<T> {
  function* resume(closing: boolean): Steps<IteratorResult<T>> {
    let step = closing ? sequence.return() : sequence.next();
    while (!step.done) {
      if (step.value.kind === "item") {
        if (!closing) return { done: false, value: step.value.value };
        step = sequence.next();
        continue;
      }
      let input: unknown;
      try {
        input = yield step.value;
      } catch (error) {
        step = sequence.throw(error);
        continue;
      }
      step = sequence.next(input);
    }
    return { done: true, value: undefined };
  }
  return { nextSteps: () => resume(false), returnSteps: () => resume(true) };
}

export function* syncStepIterator<T>(iterator: StepIterator<T>): Generator<T> {
  try {
    for (;;) {
      const step = runSync(iterator.nextSteps());
      if (step.done) return;
      yield step.value;
    }
  } finally {
    if (iterator.returnSteps !== undefined) runSync(iterator.returnSteps());
  }
}
export const COOPERATE: ExecutionEffect = Object.freeze({ kind: "cooperate" });

type ExecutionContext = {
  cooperative: boolean;
  remaining: number;
  quantum: number;
  timeSliceMs: number;
  now: () => number;
  deadline: number;
  planning: PlanningFrames;
};
let activeContext: ExecutionContext | undefined;
const DEFAULT_QUANTUM = 256;
const DEFAULT_TIME_SLICE_MS = 1;

function monotonicNow(): number {
  return performance.now();
}

function cooperationDue(context: ExecutionContext, units = 1): boolean {
  context.remaining -= units;
  if (context.remaining > 0) return false;
  context.remaining = context.quantum;
  return context.now() >= context.deadline;
}

function resetSlice(context: ExecutionContext): void {
  context.remaining = context.quantum;
  context.deadline = context.now() + context.timeSliceMs;
}

// The counter and clock are physical scheduling state, never SQL cost or the SQL host clock.
// Read the monotonic clock only at sparse work checkpoints; cheap resident work can pass several
// checkpoints without creating an effect, promise or task turn.
export function shouldCooperate(units = 1): boolean {
  const context = activeContext;
  if (context === undefined || !context.cooperative) return false;
  return cooperationDue(context, units);
}

export function* ioSteps<T>(sync: () => T, async?: () => Promise<T>): Steps<T> {
  return (yield { kind: "io", sync, async }) as T;
}

// Delegate recursive query execution through the driver stack, rather than accumulating a JS
// yield* call stack. Returning and throwing are delivered to the same suspended parent segment.
export function* callSteps<T>(steps: Steps<T>): Steps<T> {
  return (yield { kind: "call", steps }) as T;
}

function scoped<T>(
  context: ExecutionContext,
  check: (() => boolean) | undefined,
  operation: () => T,
): T {
  const previous = activeContext;
  activeContext = context;
  try {
    return withPlanningScope(context.planning, () => withCancellationScope(check, operation));
  } finally {
    activeContext = previous;
  }
}

export function runSync<T>(steps: Steps<T>): T {
  // A synchronous drain never leaves this stack, so its planning and cancellation scopes do not
  // need reinstalling for every segment. Suppress cooperative work in a nested synchronous call,
  // restoring the caller's scheduling context only when the whole drain returns.
  const previous = activeContext;
  activeContext = undefined;
  try {
    return drainSync(steps);
  } finally {
    activeContext = previous;
  }
}

function drainSync<T>(steps: Steps<T>): T {
  const check = currentCancellationCheck();
  if (check === undefined) return advanceSync(steps);
  const cancellation = { delivered: false };
  return withCancellationScope(
    () => {
      const requested = check();
      if (requested) cancellation.delivered = true;
      return requested;
    },
    () => advanceSync(steps, cancellation),
  );
}

function advanceSync<T>(steps: Steps<T>, cancellation?: { delivered: boolean }): T {
  // Most row pulls finish in their first segment. Allocate the recursive-query stack only for an
  // actual call effect; an ordinary resident row needs neither a stack array nor scope closures.
  let stack: Steps<unknown>[] | undefined;
  let current: Steps<unknown> = steps;
  let input: unknown;
  let failure: unknown;
  let failed = false;
  for (;;) {
    let step: IteratorResult<ExecutionEffect, unknown>;
    try {
      step =
        failed || cancellation?.delivered
          ? withCancellationScope(undefined, () =>
              failed ? current.throw(failure) : current.next(input),
            )
          : current.next(input);
    } catch (error) {
      const parent = stack?.pop();
      if (parent === undefined) throw error;
      current = parent;
      failure = error;
      failed = true;
      continue;
    }
    if (step.done) {
      const parent = stack?.pop();
      if (parent === undefined) return step.value as T;
      current = parent;
      input = step.value;
      failed = false;
      continue;
    }
    failed = false;
    input = undefined;
    if (step.value.kind === "call") {
      if (stack === undefined) stack = [];
      stack.push(current);
      current = step.value.steps;
      continue;
    }
    if (step.value.kind === "cooperate") continue;
    try {
      // A sticky cancellation must not interrupt resource cleanup a second time.
      input = cancellation?.delivered
        ? withCancellationScope(undefined, step.value.sync)
        : step.value.sync();
    } catch (error) {
      failure = error;
      failed = true;
    }
  }
}

export type AsyncExecutionOptions = {
  // Checked before each continuation segment and after awaited I/O/task yields.
  check?: () => boolean;
  // Atomic polling while a synchronous segment runs. A message-only cancellation changes only
  // between segments, so it need not disable the core's unmetered fast paths inside a segment.
  poll?: () => boolean;
  schedule?: (callback: () => void) => void;
  quantum?: number;
  // Elapsed scheduling budget, checked every quantum units. Zero forces every checkpoint to yield.
  timeSliceMs?: number;
  // Monotonic scheduling clock; injectable for deterministic driver tests, never exposed to SQL.
  now?: () => number;
};

export async function runAsync<T>(
  steps: Steps<T>,
  options: AsyncExecutionOptions = {},
): Promise<T> {
  const quantum = options.quantum ?? DEFAULT_QUANTUM;
  if (!Number.isSafeInteger(quantum) || quantum <= 0)
    throw new RangeError("execution quantum must be positive");
  const timeSliceMs = options.timeSliceMs ?? DEFAULT_TIME_SLICE_MS;
  if (!Number.isFinite(timeSliceMs) || timeSliceMs < 0)
    throw new RangeError("execution time slice must be finite and non-negative");
  const now = options.now ?? monotonicNow;
  const context = {
    cooperative: true,
    remaining: quantum,
    quantum,
    timeSliceMs,
    now,
    deadline: now() + timeSliceMs,
    planning: currentPlanningFrames().slice(),
  };
  const poll = options.poll ?? currentCancellationCheck();
  const check = options.check ?? poll;
  const schedule =
    options.schedule ??
    ((callback: () => void) => {
      setTimeout(callback, 0);
    });
  const stack: Steps<unknown>[] = [steps];
  let needsCheck = true;
  let input: unknown;
  let failure: unknown;
  let failed = false;
  let canceled = false;
  const segmentPoll =
    poll === undefined
      ? undefined
      : () => {
          const requested = poll();
          // A meter may throw inside a segment whose finally then yields an I/O effect. Remember that
          // delivery now, before resuming cleanup, so a sticky flag cannot interrupt that finally twice.
          if (requested) canceled = true;
          return requested;
        };
  for (;;) {
    if (needsCheck && !failed && !canceled && check?.()) {
      failure = engineError("query_canceled", "canceling statement due to user request");
      failed = true;
      canceled = true;
    }
    needsCheck = false;
    // Throw cancellation INTO the suspended continuation so every finally runs. Once delivered,
    // do not interrupt its cleanup again, including cleanup that itself needs an I/O effect.
    const current = stack[stack.length - 1]!;
    let step: IteratorResult<ExecutionEffect, unknown>;
    try {
      step = scoped(context, canceled || failed ? undefined : segmentPoll, () =>
        failed ? current.throw(failure) : current.next(input),
      );
    } catch (error) {
      stack.pop();
      if (stack.length === 0) throw error;
      failure = error;
      failed = true;
      continue;
    }
    if (step.done) {
      stack.pop();
      if (stack.length === 0) return step.value as T;
      input = step.value;
      failed = false;
      continue;
    }
    failed = false;
    input = undefined;
    if (step.value.kind === "call") {
      stack.push(step.value.steps);
      continue;
    }
    needsCheck = true;
    try {
      if (step.value.kind === "cooperate") {
        await new Promise<void>((resolve) => schedule(resolve));
        resetSlice(context);
      } else {
        const effect = step.value;
        // Even a synchronous host gets a real suspension point at page I/O on the async path.
        // This admits cancel messages before the next segment and keeps the continuation intact.
        if (effect.async !== undefined) {
          input = await effect.async();
          // Preserve the deadline across awaits: an already-resolved provider must not reset
          // its slice forever and starve control messages. Actual I/O is always awaited promptly;
          // its wait may conservatively count toward the next sparse scheduling checkpoint.
          if (cooperationDue(context)) {
            await new Promise<void>((resolve) => schedule(resolve));
            resetSlice(context);
          }
        } else {
          await new Promise<void>((resolve) => schedule(resolve));
          resetSlice(context);
          if (!canceled && check?.()) {
            failure = engineError("query_canceled", "canceling statement due to user request");
            failed = true;
            canceled = true;
          } else input = scoped(context, canceled ? undefined : segmentPoll, effect.sync);
        }
      }
    } catch (error) {
      failure = error;
      failed = true;
    }
  }
}
