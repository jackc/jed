// Host/transport failures are distinct from SQL errors and never acquire a SQLSTATE.
import { EngineError, sqlStateCode, type SqlState } from "../errors.ts";

export type TransportErrorCode =
  | "CLOSED"
  | "BUSY"
  | "QUEUE_FULL"
  | "RESULT_TOO_LARGE"
  | "WORKER_LIMIT"
  | "WORKER_FAILED"
  | "PROTOCOL"
  | "UNSUPPORTED";

export class TransportError extends Error {
  readonly code: TransportErrorCode;
  readonly outcome?: "unknown" | "not-started";

  constructor(code: TransportErrorCode, message: string, outcome?: "unknown" | "not-started") {
    super(message);
    this.name = "TransportError";
    this.code = code;
    this.outcome = outcome;
  }
}

type ErrorFields = Pick<
  EngineError,
  | "constraintName"
  | "tableName"
  | "databaseName"
  | "columnName"
  | "dataTypeName"
  | "detail"
  | "position"
  | "hint"
>;

export type WireError =
  | ({ kind: "engine"; state: SqlState; code: string; message: string } & ErrorFields)
  | {
      kind: "transport";
      code: TransportErrorCode;
      message: string;
      outcome?: "unknown" | "not-started";
    };

const STRING_FIELDS = [
  "constraintName",
  "tableName",
  "databaseName",
  "columnName",
  "dataTypeName",
  "detail",
  "hint",
] as const;

// A failed operation must still fit the reserved control lane. Bound diagnostics independently
// from result budgets; SQLSTATE and diagnostic identities survive even when text is truncated.
const MAX_ERROR_CHARS = 16_384;

export function encodeError(error: unknown): WireError {
  let remaining = MAX_ERROR_CHARS;
  const bounded = (text: string): string => {
    const result = text.slice(0, remaining);
    remaining -= result.length;
    return result;
  };
  if (error instanceof EngineError) {
    const out: WireError = {
      kind: "engine",
      state: error.state,
      code: error.code(),
      message: "",
    };
    // Preserve bounded SQL identifier fields before spending the control envelope on prose.
    for (const field of [
      "constraintName",
      "tableName",
      "databaseName",
      "columnName",
      "dataTypeName",
    ] as const) {
      if (error[field] !== undefined) out[field] = bounded(error[field]!);
    }
    out.message = bounded(error.message.slice(0, MAX_ERROR_CHARS / 2));
    if (error.detail !== undefined) out.detail = bounded(error.detail);
    if (error.hint !== undefined) out.hint = bounded(error.hint);
    if (error.position !== undefined) out.position = error.position;
    return out;
  }
  if (error instanceof TransportError) {
    return {
      kind: "transport",
      code: error.code,
      message: bounded(error.message),
      outcome: error.outcome,
    };
  }
  return {
    kind: "transport",
    code: "WORKER_FAILED",
    message: bounded(error instanceof Error ? error.message : "execution worker failed"),
    outcome: "unknown",
  };
}

export function decodeError(wire: WireError): EngineError | TransportError {
  if (wire.kind === "transport") return new TransportError(wire.code, wire.message, wire.outcome);
  if (wire.kind !== "engine" || sqlStateCode(wire.state) !== wire.code) {
    return new TransportError("PROTOCOL", "invalid worker error response");
  }
  // The constructor prefixes SQLSTATE; retain the original message verbatim, exactly once.
  const error = new EngineError(wire.state, "");
  error.message = wire.message;
  for (const field of STRING_FIELDS) {
    if (wire[field] !== undefined) error[field] = wire[field];
  }
  if (wire.position !== undefined) error.position = wire.position;
  return error;
}
