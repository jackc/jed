// Election at open under the test-only `locking = service` (spec/design/page-service.md §4.3,
// spec/pageservice/README.md §6.1): the opener that takes presence EX owns the database and serves it;
// every other opener connects to the owner's endpoint as a read-only page client (phase 2). The OS
// lock is the only authority — the owner record only tells a client where to connect and words its
// timeout error. Node-only.

import { lockUntil, timeoutValue } from "../coordinator.ts";
import { engineError } from "../errors.ts";
import type { LockFiles, LockHost } from "../lockhost.ts";
import { PageClient } from "./client.ts";
import { currentUid, readOwnerRecord } from "./endpoint.ts";

// Election is the outcome: the owner keeps its bundle (presence EX held, arrival released); a client
// holds no bundle lock and keeps only the process-local open-path claim, released when it closes.
export type Election = { owner: LockFiles; client: null } | { owner: null; client: PageClient };

// electService runs the election for the database at path (already canonical). It retries until the
// open timeout: presence may free up, and a new owner may not have bound its endpoint yet.
export function electService(host: LockHost, path: string, timeoutMs?: number): Election {
  if (!host.claimPath(path)) {
    throw engineError(
      "object_in_use",
      `database is already open in this process: ${path} (share one Database handle)`,
    );
  }
  let files: LockFiles | null = null;
  let client: PageClient | null = null;
  let arrival = false;
  try {
    files = host.openBundle(path);
    const deadline = host.nowMicros() + timeoutValue(timeoutMs) * 1000;
    lockUntil(host, files.arrival, false, deadline, path, false);
    arrival = true;
    host.validateMarker(path);
    let reason = "";
    for (;;) {
      if (host.tryLock(files.presence, true)) {
        host.unlock(files.arrival);
        arrival = false;
        client?.close();
        const owner = files;
        files = null;
        return { owner, client: null };
      }
      const record = readOwnerRecord(path);
      if (record === null) reason = "";
      else if (record.uid !== currentUid()) reason = "owned by another user's process";
      else if (!record.serving) reason = "owned by a non-serving process";
      else {
        reason = "";
        client ??= new PageClient(path);
        const result = client.connect(record.endpoint);
        if (result.ok) {
          // A client holds no bundle lock while connected (page-service.md §4.2).
          host.unlock(files.arrival);
          arrival = false;
          host.closeFiles(files);
          files = null;
          client.release = () => host.releasePath(path);
          const connected = client;
          client = null;
          return { owner: null, client: connected };
        }
        if (!result.retry) throw result.error;
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
    client?.close();
    if (files !== null) {
      if (arrival) host.unlock(files.arrival);
      host.closeFiles(files);
    }
    host.releasePath(path);
    throw error;
  }
}
