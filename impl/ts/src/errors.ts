// Structured error codes (CLAUDE.md §5, §10). A SqlState's code is the canonical 5-char
// SQLSTATE from spec/errors/registry.toml. The SqlState union + the code mapping are
// generated (the codegen "middle path", CLAUDE.md §5 — see sqlstate.ts /
// spec/design/codegen.md); this file is the hand-written EngineError scaffolding that
// consumes them. Errors are thrown as EngineError (the TS idiom); the harness reads
// `.code()` to match `statement error <code>`.

import { type SqlState, sqlStateCode } from "./sqlstate.ts";

// Re-export so existing `./errors.ts` consumers (and lib.ts) keep their import paths.
export { type SqlState, sqlStateCode };

// EngineError is an engine error: a SQLSTATE plus an informational (never-matched)
// message. The message text embeds the code so it also matches as a plain regex under
// a stock sqllogictest runner (spec/design/conformance.md §2).
export class EngineError extends Error {
  state: SqlState;

  constructor(state: SqlState, message: string) {
    super(`${sqlStateCode(state)}: ${message}`);
    this.name = "EngineError";
    this.state = state;
  }

  // code returns the error's SQLSTATE string.
  code(): string {
    return sqlStateCode(this.state);
  }
}

// engineError builds an EngineError (mirrors Go's NewError / Rust's EngineError::new).
export function engineError(state: SqlState, message: string): EngineError {
  return new EngineError(state, message);
}
