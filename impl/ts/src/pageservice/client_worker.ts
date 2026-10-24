// The page client's socket thread (spec/design/page-service.md §14): one worker per process owns every
// page-service connection the process opens (client.ts starts it once and reuses it), each tagged by a
// local connection number, and relays raw frames for the engine thread, which blocks in Atomics.wait
// for each answer. The engine thread is single-threaded and waits for one answer at a time, so one
// answer channel serves every connection. The worker only splits each byte stream into frames by
// their length prefix; decoding happens on the engine thread, which knows the connection's page size.
//
// Two things reach the engine thread without a request (spec/pageservice/README.md §6.5): owner
// notices, which the worker consumes itself and records in the connection's status buffer (a
// SharedArrayBuffer the engine thread reads with atomic loads), and the connection's end, which it
// records there too (waking an engine thread waiting for it) and announces on the connection's event
// port, so the engine thread re-elects at its next idle event-loop turn. Node-only.

import { spawn } from "node:child_process";
import { createConnection } from "node:net";
import { type MessagePort, workerData } from "node:worker_threads";

import { signalAnswer } from "./blocking.ts";
import {
  PS_CODE_NOTICE,
  PS_MAX_FRAME_LENGTH,
  PS_NOTICE_YIELD_BEGUN,
  PS_NOTICE_YIELD_CANCELLED,
} from "./codec.ts";

import { LOST_AT_OFFSET, NOTICE_CONN_OFFSET, STATUS_LOST, STATUS_PAUSED } from "./client_status.ts";

// The engine thread's requests, each for connection `conn`: connect to an endpoint, send a frame
// (reply: whether it waits for the next frame; otherwise it waits until the frame is handed to the
// kernel, so a request with no reply, such as end_read, is on the wire before the engine thread moves
// on), wait for the connection's next frame without sending (the engine thread skipping a frame that
// answers another request), drop the connection (no answer), or forget it entirely (no answer).
export type ClientRequest =
  | {
      op: "connect";
      conn: number;
      endpoint: string;
      // command, when present, is the stream transport's command (spec/pageservice/README.md §5.3):
      // the connection is its stdin and stdout instead of a socket to endpoint.
      command?: string[];
      // status and events are sent with a connection's first connect (events in the transfer list).
      status?: SharedArrayBuffer;
      events?: MessagePort;
    }
  | { op: "send"; conn: number; frame: Uint8Array; reply: boolean }
  | { op: "next"; conn: number }
  | { op: "disconnect"; conn: number }
  | { op: "forget"; conn: number };

// The worker's answers: connected (or a reply-less frame written), a reply frame, or the connection
// is gone (unsent: it was already gone before the frame could be written; failure: a stream
// transport's standard error tail, or why it could not run, for a failed open's message).
export type ClientAnswer =
  | { ok: true }
  | { frame: Uint8Array }
  | { lost: string; unsent?: boolean; failure?: string };

// STDERR_TAIL is how much of a stream transport's standard error a failed open reports.
const STDERR_TAIL = 2048;

// Transport is one connection's byte pipe: a socket, or a stream transport's stdin and stdout.
type Transport = {
  write(frame: Uint8Array, written?: () => void): void;
  destroy(): void;
};

const { flag, port } = workerData as { flag: Int32Array; port: MessagePort };

// Connection is one page-service connection's socket state.
class Connection {
  socket: Transport | null = null;
  // failure is the stream transport's standard error tail (or why it could not run).
  failure = "";
  // lost is why the connection ended (null while it is up).
  lost: string | null = "not connected";
  // waiting is true while the engine thread waits for this connection's frame (or its connect);
  // acking while it waits for a reply-less frame to be written.
  waiting = false;
  acking = false;
  buffered = new Uint8Array(0);
  readonly frames: Uint8Array[] = [];
  status: Int32Array | null = null;
  noticeConn: BigInt64Array | null = null;
  lostAt: Float64Array | null = null;
  events: MessagePort | null = null;

  attach(status: SharedArrayBuffer, events: MessagePort | undefined): void {
    this.status = new Int32Array(status, 0, 2);
    this.noticeConn = new BigInt64Array(status, NOTICE_CONN_OFFSET, 1);
    this.lostAt = new Float64Array(status, LOST_AT_OFFSET, 1);
    if (events !== undefined) this.events = events;
  }

  // markLost records the connection's end in the status buffer, wakes a waiting engine thread, and
  // announces it on the event port (once per connect).
  private markLost(): void {
    const status = this.status;
    if (status === null || Atomics.load(status, STATUS_LOST) === 1) return;
    this.lostAt![0] = Date.now();
    Atomics.store(status, STATUS_LOST, 1);
    Atomics.notify(status, STATUS_LOST);
    this.events?.postMessage("lost");
  }

  // notice acts on an owner notice frame (README §3.7): record a yield begun (with the requester's
  // connection id) or cancelled; ignore any other kind.
  private notice(frame: Uint8Array): void {
    if (frame.length < 18 || this.status === null) return;
    const kind = frame[9]!;
    if (kind === PS_NOTICE_YIELD_BEGUN) {
      const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength);
      this.noticeConn![0] = view.getBigInt64(10);
      Atomics.store(this.status, STATUS_PAUSED, 1);
    } else if (kind === PS_NOTICE_YIELD_CANCELLED) {
      Atomics.store(this.status, STATUS_PAUSED, 0);
    }
  }

  answer(a: ClientAnswer): void {
    this.waiting = false;
    this.acking = false;
    signalAnswer(port, flag, a);
  }

  connectionLost(reason: string): void {
    if (this.lost === null) this.lost = reason;
    this.socket?.destroy();
    this.socket = null;
    this.markLost();
    if ((this.waiting && this.frames.length === 0) || this.acking) {
      this.answer(this.lostAnswer());
    }
  }

  // receive splits the stream into frames: each is passed to a waiting engine thread or queued.
  receive(chunk: Buffer): void {
    const joined = new Uint8Array(this.buffered.length + chunk.length);
    joined.set(this.buffered);
    joined.set(chunk, this.buffered.length);
    let start = 0;
    while (joined.length - start >= 4) {
      const length =
        ((joined[start]! << 24) |
          (joined[start + 1]! << 16) |
          (joined[start + 2]! << 8) |
          joined[start + 3]!) >>>
        0;
      if (length < 5 || length > PS_MAX_FRAME_LENGTH) {
        this.connectionLost(`malformed frame length ${length}`);
        return;
      }
      if (joined.length - start < 4 + length) break;
      const frame = joined.slice(start, start + 4 + length);
      start += 4 + length;
      const requestId =
        ((frame[5]! << 24) | (frame[6]! << 16) | (frame[7]! << 8) | frame[8]!) >>> 0;
      if (requestId === 0 && frame[4] === PS_CODE_NOTICE) this.notice(frame);
      else this.frames.push(frame);
    }
    this.buffered = joined.slice(start);
    if (this.waiting && this.frames.length > 0) this.answer({ frame: this.frames.shift()! });
  }

  // lostAnswer is the answer for a lost connection, carrying a stream transport's failure.
  lostAnswer(unsent = false): ClientAnswer {
    const answer: ClientAnswer = { lost: this.lost ?? "not connected" };
    if (unsent) answer.unsent = true;
    if (this.failure.trim() !== "") answer.failure = this.failure.trim();
    return answer;
  }

  connect(endpoint: string, command?: string[]): void {
    this.socket?.destroy();
    this.lost = null;
    this.failure = "";
    this.buffered = new Uint8Array(0);
    this.frames.length = 0;
    this.waiting = true;
    if (this.status !== null) {
      Atomics.store(this.status, STATUS_LOST, 0);
      Atomics.store(this.status, STATUS_PAUSED, 0);
    }
    if (command !== undefined) {
      this.spawnTransport(command);
      return;
    }
    const s = createConnection(endpoint);
    const transport: Transport = {
      write: (frame, written) => {
        s.write(frame, written);
      },
      destroy: () => s.destroy(),
    };
    this.socket = transport;
    s.on("connect", () => {
      if (this.socket === transport && this.waiting) this.answer({ ok: true });
    });
    s.on("data", (chunk: Buffer) => {
      if (this.socket === transport) this.receive(chunk);
    });
    s.on("error", (error) => {
      if (this.socket === transport) this.connectionLost(error.message);
    });
    s.on("close", () => {
      if (this.socket === transport) this.connectionLost("the owner closed the connection");
    });
  }

  // spawnTransport runs a stream transport's command (spec/pageservice/README.md §5.3): its stdin and
  // stdout carry the connection, and the tail of its standard error is kept for a failed open's
  // message. Dropping the connection closes its stdin, so a well-behaved transport exits on its own;
  // one that does not is killed.
  private spawnTransport(command: string[]): void {
    const child = spawn(command[0]!, command.slice(1), { stdio: ["pipe", "pipe", "pipe"] });
    let killTimer: ReturnType<typeof setTimeout> | null = null;
    const transport: Transport = {
      write: (frame, written) => {
        child.stdin.write(frame, written === undefined ? undefined : () => written());
      },
      destroy: () => {
        child.stdin.destroy();
        child.stdout.destroy();
        if (killTimer === null && child.exitCode === null && child.signalCode === null) {
          killTimer = setTimeout(() => child.kill("SIGKILL"), 2000);
          killTimer.unref();
        }
      },
    };
    this.socket = transport;
    child.on("spawn", () => {
      if (this.socket === transport && this.waiting) this.answer({ ok: true });
    });
    child.on("error", (error) => {
      if (this.socket !== transport) return;
      if (child.pid === undefined) this.failure = `could not run ${command[0]}: ${error.message}`;
      this.connectionLost(error.message);
    });
    child.stdout.on("data", (chunk: Buffer) => {
      if (this.socket === transport) this.receive(chunk);
    });
    child.stderr.on("data", (chunk: Buffer) => {
      if (this.socket !== transport) return;
      this.failure += chunk.toString("utf8");
      if (this.failure.length > STDERR_TAIL) this.failure = this.failure.slice(-STDERR_TAIL);
    });
    // Write errors (EPIPE once the transport exits) end the connection like its close does.
    child.stdin.on("error", (error) => {
      if (this.socket === transport) this.connectionLost(error.message);
    });
    child.on("close", () => {
      if (killTimer !== null) clearTimeout(killTimer);
      if (this.socket === transport) this.connectionLost("the transport exited");
    });
  }

  send(frame: Uint8Array, reply: boolean): void {
    const socket = this.socket;
    if (this.lost !== null || socket === null) {
      this.answer(this.lostAnswer(true));
      return;
    }
    if (reply) {
      this.waiting = true;
      socket.write(frame);
      if (this.frames.length > 0) this.answer({ frame: this.frames.shift()! });
    } else {
      this.acking = true;
      socket.write(frame, () => {
        if (this.acking) this.answer({ ok: true });
      });
    }
  }

  // next waits for the next frame (or the connection's loss) without sending.
  next(): void {
    if (this.frames.length > 0) {
      this.answer({ frame: this.frames.shift()! });
      return;
    }
    if (this.lost !== null || this.socket === null) {
      this.answer(this.lostAnswer());
      return;
    }
    this.waiting = true;
  }

  disconnect(): void {
    this.lost = "disconnected";
    this.socket?.destroy();
    this.socket = null;
    this.markLost();
  }
}

const connections = new Map<number, Connection>();

port.on("message", (request: ClientRequest) => {
  let c = connections.get(request.conn);
  switch (request.op) {
    case "connect":
      if (c === undefined) {
        c = new Connection();
        connections.set(request.conn, c);
      }
      if (request.status !== undefined) c.attach(request.status, request.events);
      c.connect(request.endpoint, request.command);
      return;
    case "send":
      if (c === undefined) signalAnswer(port, flag, { lost: "not connected", unsent: true });
      else c.send(request.frame, request.reply);
      return;
    case "next":
      if (c === undefined) signalAnswer(port, flag, { lost: "not connected" });
      else c.next();
      return;
    case "disconnect":
      c?.disconnect();
      return;
    case "forget":
      c?.disconnect();
      c?.events?.close();
      connections.delete(request.conn);
      return;
  }
});
