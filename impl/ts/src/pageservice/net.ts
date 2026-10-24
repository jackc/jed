// The page service's transport seam (spec/design/page-service.md §14; spec/design/simulation-testing.md
// §14 S8). In production an owner serves on the process's service worker thread over OS sockets
// (owner_worker.ts) and a client's connection runs on its socket worker thread (client_worker.ts); the
// engine thread reaches each through a link. The deterministic simulator substitutes an in-memory
// network (src/sim/net.ts) on which the owner's connection code (owner_conn.ts) runs on the engine
// thread, so every request is answered before the client's wait returns. Type-only.

import type { Credential } from "../crypto.ts";
import type { Engine } from "../executor.ts";
import type { ClientAnswer } from "./client_worker.ts";
import type { ServiceState } from "./shared_state.ts";

// ClientLink is one page client's connection as its engine thread drives it: the socket worker's
// requests (connect, send a frame, wait for the next answer, skip to the next frame, drop the
// connection), and the connection's end and owner notices as the worker records them.
export interface ClientLink {
  // connect dials endpoint: {ok} or {lost}.
  connect(endpoint: string): ClientAnswer;
  // send writes one frame. With reply set the next wait returns its reply frame; otherwise the next
  // wait returns {ok} once the frame is written, or {lost, unsent} when the connection was already gone.
  send(frame: Uint8Array, reply: boolean): void;
  wait(): ClientAnswer;
  // next waits for the connection's next frame without sending.
  next(): ClientAnswer;
  // disconnect drops the connection; close drops it and releases the link for good.
  disconnect(): void;
  close(): void;
  // lost reports that the connection ended; waitForLoss blocks until it does or deadlineMs
  // (Date.now() scale) passes.
  lost(): boolean;
  waitForLoss(deadlineMs: number): boolean;
  // pausedBy is the connection id a yield-begun notice named (null: none, or cancelled); lostAtMs is
  // when the connection ended (0: not recorded).
  pausedBy(): bigint | null;
  lostAtMs(): number;
  // onLost installs the handler run on the event loop when the connection ends.
  onLost(handler: () => void): void;
}

// StreamTransport is how a remote database is reached (the stream transport, spec/pageservice/README.md
// §5.3): every connection runs command, whose stdin and stdout carry it, and its hello carries
// credential. lastFailure is the tail of the last transport's standard error, or why it could not run,
// for a failed open's message.
export type StreamTransport = {
  readonly command: string[];
  readonly credential: Credential | null;
  lastFailure: string;
};

// OwnerServeArgs is what an owner's service needs to serve one database (owner.ts).
export type OwnerServeArgs = {
  state: ServiceState;
  // storage is the owner's storage engine (a same-thread service writes client commits through its
  // pager, as an owner's own commit does).
  storage: Engine;
  dbPath: string;
  endpoint: string;
  pageSize: number;
  formatVersion: number;
  incarnation: Uint8Array;
  ownerReadOnly: boolean;
  preferYield: boolean;
  noSync: boolean;
  idleTimeoutMs: number;
  idleSessionTimeoutMs: number;
  maxConnections: number;
  // The service's events for the engine thread: a gate handed over for a maintenance commit, an
  // accepted ownership request, and a batch a relaxed client commit started (validated-cow.md §4.1
  // rule 4), which arms the idle flusher. localOldest is the oldest version a local reader pins (null:
  // none).
  onMaintain: () => void;
  onYield: () => void;
  onBatch: () => void;
  localOldest: () => bigint | null;
};

// OwnerLink is the engine thread's handle on the service serving its database.
export interface OwnerLink {
  // stop stops serving: no more connections, every client sees EOF. poison stops serving after a
  // failed commit. notice sends a notice to every connection.
  stop(): void;
  poison(): void;
  notice(kind: number): void;
  // gateWaits is false when the engine thread must not block on a held writer gate (a same-thread
  // service could never release it): the wait fails 55P03 at once.
  readonly gateWaits: boolean;
}

// PageNet is a page-service network: links for clients, and serving for owners.
export interface PageNet {
  link(pid: number): ClientLink;
  // serve binds args.endpoint and serves; a bind failure throws.
  serve(pid: number, args: OwnerServeArgs): OwnerLink;
}
