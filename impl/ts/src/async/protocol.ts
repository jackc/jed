// Worker transport v2. This is a host contract, separate from SQL cost and file bytes.
import type { WireError } from "./errors.ts";
import type { PackedRows } from "./packed.ts";

export const RUNTIME_VERSION = 2;

export type RuntimeLimits = {
  maxQueuedRequests: number;
  maxQueuedBytes: number;
  maxBatchRows: number;
  maxBatchBytes: number;
  maxResultBytes: number;
  maxSessions: number;
  maxStatements: number;
  maxCursors: number;
  maxWorkers: number;
};

export const DEFAULT_RUNTIME_LIMITS: Readonly<RuntimeLimits> = Object.freeze({
  maxQueuedRequests: 128,
  maxQueuedBytes: 8 * 1024 * 1024,
  maxBatchRows: 128,
  maxBatchBytes: 256 * 1024,
  maxResultBytes: 2 * 1024 * 1024,
  maxSessions: 64,
  maxStatements: 256,
  maxCursors: 64,
  maxWorkers: 32,
});

export function runtimeLimits(options: Partial<RuntimeLimits> = {}): RuntimeLimits {
  const limits = { ...DEFAULT_RUNTIME_LIMITS, ...options };
  for (const [key, value] of Object.entries(limits)) {
    if (!Number.isSafeInteger(value) || value <= 0) {
      throw new RangeError(`${key} must be a positive safe integer`);
    }
  }
  if (limits.maxResultBytes < limits.maxBatchBytes) {
    throw new RangeError("maxResultBytes must admit at least one maxBatchBytes batch");
  }
  return limits;
}

export type RuntimeRequest = {
  version: typeof RUNTIME_VERSION;
  id: number;
  op: string;
  session?: number;
  transaction?: number;
  args: Record<string, unknown>;
  cancel?: SharedArrayBuffer;
  aborted?: boolean;
};

export type RuntimeReply =
  | { version: typeof RUNTIME_VERSION; id: number; ok: true; result: unknown }
  | { version: typeof RUNTIME_VERSION; id: number; ok: false; error: WireError };

export type QueryReply = {
  cursor: number;
  columnNames: string[];
  columnTypes: string[];
  rowsAffected: number | null;
  cost: bigint;
};

export type FetchReply = { rows: PackedRows; done: boolean; cost: bigint };

export interface RuntimeTransport {
  postMessage(request: RuntimeRequest): void;
  onMessage(handler: (reply: RuntimeReply) => void): void;
  onFailure(handler: (error: unknown) => void): void;
  terminate(): void | Promise<void>;
}

// Control admission is independent of the ordinary queue so a full queue cannot keep an
// owner from releasing its transaction or a caller from releasing a cursor.
export function isControlOperation(op: string): boolean {
  return (
    op === "cancel" ||
    op === "close" ||
    op === "cursor.close" ||
    op === "session.close" ||
    op === "statement.close" ||
    op === "rollback" ||
    op === "commit"
  );
}
