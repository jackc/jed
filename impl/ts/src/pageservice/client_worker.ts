// The page client's socket thread (spec/design/page-service.md §14): one worker per process owns every
// page-service connection the process opens (client.ts starts it once and reuses it), each tagged by a
// local connection number, and relays raw frames for the engine thread, which blocks in Atomics.wait
// for each answer. The engine thread is single-threaded and waits for one answer at a time, so one
// answer channel serves every connection. The worker only splits each byte stream into frames by
// their length prefix; decoding happens on the engine thread, which knows the connection's page size.
// Node-only.

import { createConnection, type Socket } from "node:net";
import { type MessagePort, workerData } from "node:worker_threads";

import { signalAnswer } from "./blocking.ts";
import { PS_MAX_FRAME_LENGTH } from "./codec.ts";

// The engine thread's requests, each for connection `conn`: connect to an endpoint, send a frame
// (reply: whether it waits for the next frame; otherwise it waits until the frame is handed to the
// kernel, so a request with no reply, such as end_read, is on the wire before the engine thread moves
// on), wait for the connection's next frame without sending (the engine thread skipping a frame that
// answers another request), drop the connection (no answer), or forget it entirely (no answer).
export type ClientRequest =
  | { op: "connect"; conn: number; endpoint: string }
  | { op: "send"; conn: number; frame: Uint8Array; reply: boolean }
  | { op: "next"; conn: number }
  | { op: "disconnect"; conn: number }
  | { op: "forget"; conn: number };

// The worker's answers: connected (or a reply-less frame written), a reply frame, or the connection
// is gone (unsent: it was already gone before the frame could be written).
export type ClientAnswer =
  | { ok: true }
  | { frame: Uint8Array }
  | { lost: string; unsent?: boolean };

const { flag, port } = workerData as { flag: Int32Array; port: MessagePort };

// Connection is one page-service connection's socket state.
class Connection {
  socket: Socket | null = null;
  // lost is why the connection ended (null while it is up).
  lost: string | null = "not connected";
  // waiting is true while the engine thread waits for this connection's frame (or its connect);
  // acking while it waits for a reply-less frame to be written.
  waiting = false;
  acking = false;
  buffered = new Uint8Array(0);
  readonly frames: Uint8Array[] = [];

  answer(a: ClientAnswer): void {
    this.waiting = false;
    this.acking = false;
    signalAnswer(port, flag, a);
  }

  connectionLost(reason: string): void {
    if (this.lost === null) this.lost = reason;
    this.socket?.destroy();
    this.socket = null;
    if ((this.waiting && this.frames.length === 0) || this.acking) {
      this.answer({ lost: this.lost });
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
      this.frames.push(joined.slice(start, start + 4 + length));
      start += 4 + length;
    }
    this.buffered = joined.slice(start);
    if (this.waiting && this.frames.length > 0) this.answer({ frame: this.frames.shift()! });
  }

  connect(endpoint: string): void {
    this.socket?.destroy();
    this.lost = null;
    this.buffered = new Uint8Array(0);
    this.frames.length = 0;
    this.waiting = true;
    const s = createConnection(endpoint);
    this.socket = s;
    s.on("connect", () => {
      if (this.socket === s && this.waiting) this.answer({ ok: true });
    });
    s.on("data", (chunk: Buffer) => {
      if (this.socket === s) this.receive(chunk);
    });
    s.on("error", (error) => {
      if (this.socket === s) this.connectionLost(error.message);
    });
    s.on("close", () => {
      if (this.socket === s) this.connectionLost("the owner closed the connection");
    });
  }

  send(frame: Uint8Array, reply: boolean): void {
    const socket = this.socket;
    if (this.lost !== null || socket === null) {
      this.answer({ lost: this.lost ?? "not connected", unsent: true });
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
      this.answer({ lost: this.lost ?? "not connected" });
      return;
    }
    this.waiting = true;
  }

  disconnect(): void {
    this.lost = "disconnected";
    this.socket?.destroy();
    this.socket = null;
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
      c.connect(request.endpoint);
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
      connections.delete(request.conn);
      return;
  }
});
