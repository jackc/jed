// A synchronous request/reply channel to a worker thread (spec/design/page-service.md §14): the engine
// thread posts a message and blocks in Atomics.wait until the worker answers, then takes the answer
// with receiveMessageOnPort. Node permits Atomics.wait on the main thread; there is no busy-spin.
// Node-only (worker_threads); the browser bundle never reaches it.

import {
  MessageChannel,
  type MessagePort,
  receiveMessageOnPort,
  type Transferable,
  Worker,
} from "node:worker_threads";

export class BlockingChannel {
  // flag is set to 1 by the worker after it posts an answer (signalAnswer), waking the waiter.
  readonly flag = new Int32Array(new SharedArrayBuffer(4));
  // port is this thread's end; remote goes to the worker in its transfer list.
  readonly port: MessagePort;
  readonly remote: MessagePort;

  constructor() {
    const { port1, port2 } = new MessageChannel();
    this.port = port1;
    this.remote = port2;
    this.port.unref();
  }

  // post sends a message that gets no answer.
  post(message: unknown): void {
    this.port.postMessage(message);
  }

  // request sends a message (moving the transfer list's objects) and waits for the worker's answer.
  request(message: unknown, transfer: Transferable[] = []): unknown {
    this.port.postMessage(message, transfer);
    return this.wait();
  }

  // wait blocks until the worker's next answer arrives and returns it.
  wait(): unknown {
    for (;;) {
      Atomics.store(this.flag, 0, 0);
      const got = receiveMessageOnPort(this.port);
      if (got !== undefined) return got.message;
      Atomics.wait(this.flag, 0, 0);
    }
  }

  close(): void {
    this.port.close();
  }
}

// signalAnswer is the worker's half: post the answer, then wake the waiting thread.
export function signalAnswer(port: MessagePort, flag: Int32Array, answer: unknown): void {
  port.postMessage(answer);
  Atomics.store(flag, 0, 1);
  Atomics.notify(flag, 0);
}

// IDLE_STOP_MS is how long a process worker with no users lingers before it is stopped: long enough
// that a close followed by a reopen reuses it, short enough that no thread outlives its handles.
const IDLE_STOP_MS = 1000;

// ProcessWorker is one long-lived worker thread shared by every handle of the process that needs it
// (the page client's socket worker, the owner's service worker), so an open costs a message, not a
// thread start. It is started on first use and unref'd, so it never keeps the process alive; once its
// last user releases it, an unref'd timer stops it after IDLE_STOP_MS.
export class ProcessWorker {
  private readonly url: URL;
  private worker: Worker | null = null;
  private channel: BlockingChannel | null = null;
  private users = 0;
  private idle: ReturnType<typeof setTimeout> | null = null;

  constructor(url: URL) {
    this.url = url;
  }

  // acquire returns the channel to the worker, starting it if needed; pair it with release.
  acquire(): BlockingChannel {
    this.users++;
    if (this.idle !== null) {
      clearTimeout(this.idle);
      this.idle = null;
    }
    if (this.channel === null) {
      const channel = new BlockingChannel();
      const worker = new Worker(this.url, {
        workerData: { flag: channel.flag, port: channel.remote },
        transferList: [channel.remote],
      });
      worker.unref();
      this.worker = worker;
      this.channel = channel;
    }
    return this.channel;
  }

  release(): void {
    this.users--;
    if (this.users > 0 || this.worker === null) return;
    this.idle = setTimeout(() => this.stop(), IDLE_STOP_MS);
    this.idle.unref();
  }

  private stop(): void {
    this.idle = null;
    if (this.users > 0 || this.worker === null) return;
    void this.worker.terminate();
    this.channel?.close();
    this.worker = null;
    this.channel = null;
  }
}
