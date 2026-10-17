// Election under the page service, `locking = "auto"` (spec/design/page-service.md §4.3,
// spec/pageservice/README.md §6.1, §6.5): the opener that takes presence EX owns the database and
// serves it; every other opener connects to the owner's endpoint as a page client (read-only exactly
// when the opener asked for a read-only handle). A client whose connection ends runs the same election
// again (re-election). The OS lock is the only authority — the owner record only tells a client where
// to connect and words its timeout error. Node-only.

import { lockUntil, timeoutValue } from "../coordinator.ts";
import type { Environment } from "../environment.ts";
import { EngineError, engineError } from "../errors.ts";
import type { Engine } from "../executor.ts";
import type { LockFiles, LockHost } from "../lockhost.ts";
import { type ClientOpenOptions, openPageClient, PageClient } from "./client.ts";
import { currentUid } from "./endpoint.ts";

// Election is the outcome: the owner keeps its bundle (presence EX held, arrival released); a client
// holds no bundle lock and arrives opened (its engine over the connection).
export type Election =
  | { owner: LockFiles; client: null; engine: null }
  | { owner: null; client: PageClient; engine: Engine };

// ElectOptions shape one election: never skips presence tries (ownership = never), pauseUntilMs skips
// them until then (the handoff pause), and reelect marks a re-election, which keeps the handle's
// process-local path claim (it neither claims nor releases it). open and environment open a client's
// engine.
export type ElectOptions = {
  never: boolean;
  pauseUntilMs: number;
  reelect: boolean;
  open: ClientOpenOptions;
  environment: Environment;
};

function isCode(error: unknown, code: string): boolean {
  return error instanceof EngineError && error.code() === code;
}

// electService runs the election for the database at path (already canonical). It retries until the
// open timeout: presence may free up, a new owner may not have bound its endpoint yet, and a draining
// owner closes the connection of a client that opened during the drain.
export function electService(
  host: LockHost,
  path: string,
  timeoutMs: number | undefined,
  readOnly: boolean,
  eo: ElectOptions,
): Election {
  if (!eo.reelect && !host.claimPath(path)) {
    throw engineError(
      "object_in_use",
      `database is already open in this process: ${path} (share one Database handle)`,
    );
  }
  let files: LockFiles | null = null;
  let arrival = false;
  try {
    files = host.openBundle(path);
    const deadline = host.nowMicros() + timeoutValue(timeoutMs) * 1000;
    lockUntil(host, files.arrival, false, deadline, path, false);
    arrival = true;
    host.validateMarker(path);
    // The handoff pause (Date.now() scale) is waited out on the lock host's clock, as every other
    // wait of the election is (the simulator's in a simulation).
    const pauseEnd = host.nowMicros() + Math.max(0, eo.pauseUntilMs - Date.now()) * 1000;
    let reason = "";
    for (;;) {
      if (!eo.never && host.nowMicros() >= pauseEnd && host.tryLock(files.presence, true)) {
        host.unlock(files.arrival);
        arrival = false;
        const owner = files;
        files = null;
        return { owner, client: null, engine: null };
      }
      const record = host.readOwnerRecord(path);
      if (record === null) reason = "";
      else if (record.uid !== currentUid()) reason = "owned by another user's process";
      else if (!record.serving) reason = "owned by a non-serving process";
      else {
        reason = "";
        const client = new PageClient(path, host.pageNet?.link(host.pid()));
        const result = client.connect(record.endpoint, readOnly);
        if (result.ok) {
          let engine: Engine | null = null;
          try {
            engine = openPageClient(client, eo.open, eo.environment);
          } catch (error) {
            client.close();
            if (!isCode(error, "08006")) throw error;
          }
          if (engine !== null) {
            // A client holds no bundle lock while connected (page-service.md §4.2).
            host.unlock(files.arrival);
            arrival = false;
            host.closeFiles(files);
            files = null;
            return { owner: null, client, engine };
          }
        } else {
          client.close();
          // A full owner (53300) fails an open at once; a re-electing client keeps trying.
          if (!result.retry || (isCode(result.error, "53300") && !eo.reelect)) {
            throw result.error;
          }
        }
      }
      if (host.nowMicros() >= deadline) {
        throw engineError(
          "object_in_use",
          `database file is in use: ${path}${reason === "" ? "" : ` (${reason})`}`,
        );
      }
      host.pause();
    }
  } catch (error) {
    if (files !== null) {
      if (arrival) host.unlock(files.arrival);
      host.closeFiles(files);
    }
    if (!eo.reelect) host.releasePath(path);
    throw error;
  }
}
