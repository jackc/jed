// Cancellation control for synchronous core kernels. A worker's shared atomic flag can change
// while JavaScript is running; AbortSignal alone can change only at event-loop boundaries.
// The dynamic scope is isolate-local and synchronous: never retain it across an await.
import { engineError } from "./errors.ts";

type CancellationCheck = () => boolean;
let activeCheck: CancellationCheck | undefined;

export function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) canceled();
}

function canceled(): never {
  throw engineError("query_canceled", "canceling statement due to user request");
}

export function cancellationCheckpoint(): void {
  if (activeCheck?.()) canceled();
}

export function hasCancellation(): boolean {
  return activeCheck !== undefined;
}

// Installs a flag poll for all meters, including scratch meters created by scalar kernels.
// Restoring in finally prevents cancellation leaking into another session or operation.
export function withCancellation<T>(check: CancellationCheck | undefined, operation: () => T): T {
  const previous = activeCheck;
  activeCheck = check;
  try {
    cancellationCheckpoint();
    return operation();
  } finally {
    activeCheck = previous;
  }
}

// Commit code calls its last safe checkpoint before entering this scope. Once storage or
// publication begins, complete that transition and report its actual outcome, even on abort.
export function withoutCancellation<T>(operation: () => T): T {
  const previous = activeCheck;
  activeCheck = undefined;
  try {
    return operation();
  } finally {
    activeCheck = previous;
  }
}
