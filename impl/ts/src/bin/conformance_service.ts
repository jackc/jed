// The corpus through a page client (spec/pageservice/README.md §7): in `service` mode another core's
// process actor owns the database while each query record runs here as a page client of it, so rows,
// errors, plans, and costs are checked across the page service and across cores.
//
// The harness is synchronous, so the actor's pipes are driven from a worker thread: the harness posts
// a command line and blocks in Atomics.wait (BlockingChannel) until the worker relays the reply line.

import { join } from "node:path";
import { Worker } from "node:worker_threads";

import { BlockingChannel } from "../pageservice/blocking.ts";

// The relay worker (plain JavaScript, evaluated): spawn the actor, write each command line to its
// stdin, and answer with its next stdout line (null once it has exited).
const RELAY = `
const { workerData } = require("node:worker_threads");
const { spawn } = require("node:child_process");
const { flag, port, argv } = workerData;
const child = spawn(argv[0], argv.slice(1), { stdio: ["pipe", "pipe", "inherit"] });
let buffered = "";
const lines = [];
let waiting = true;
let exited = false;
function answer(message) {
  port.postMessage(message);
  Atomics.store(flag, 0, 1);
  Atomics.notify(flag, 0);
}
function flush() {
  if (!waiting) return;
  if (lines.length > 0) {
    waiting = false;
    answer({ line: lines.shift() });
  } else if (exited) {
    waiting = false;
    answer({ line: null });
  }
}
child.stdout.setEncoding("utf8");
child.stdout.on("data", (data) => {
  buffered += data;
  for (let i = buffered.indexOf("\\n"); i >= 0; i = buffered.indexOf("\\n")) {
    lines.push(buffered.slice(0, i));
    buffered = buffered.slice(i + 1);
  }
  flush();
});
child.on("exit", () => { exited = true; flush(); });
child.on("error", () => { exited = true; flush(); });
child.stdin.on("error", () => {});
port.on("message", (message) => {
  if (message.op === "line") {
    child.stdin.write(message.line + "\\n");
    waiting = true;
    flush();
  } else {
    port.close();
  }
});
`;

export class OwnerActor {
  private readonly channel = new BlockingChannel();
  private readonly core: string;

  // start launches core's process actor (rust | go | node) with `owner-idle <root>` and waits for
  // READY.
  constructor(core: string, root: string) {
    this.core = core;
    let argv: string[];
    switch (core) {
      case "rust":
        argv = [join(root, "impl/rust/target/release/process_actor")];
        break;
      case "go":
        argv = [join(root, "impl/go/process_actor")];
        break;
      case "node":
        argv = [process.execPath, join(root, "impl/ts/src/bin/process_actor.ts")];
        break;
      default:
        throw new Error(`JED_SERVICE_OWNER must be rust, go, or node; got "${core}"`);
    }
    argv.push("owner-idle", root);
    const worker = new Worker(RELAY, {
      eval: true,
      workerData: { flag: this.channel.flag, port: this.channel.remote, argv },
      transferList: [this.channel.remote],
    });
    worker.unref();
    const ready = this.channel.wait() as { line: string | null };
    if (ready.line !== "READY") {
      throw new Error(`${core} owner actor did not start: ${JSON.stringify(ready.line)}`);
    }
  }

  private command(name: string, argument = ""): void {
    const line = argument === "" ? name : `${name}\t${argument}`;
    const reply = (this.channel.request({ op: "line", line }) as { line: string | null }).line;
    if (reply === null) throw new Error(`${this.core} owner actor exited`);
    if (!reply.startsWith("OK")) {
      const [status, code, detail] = reply.split("\t");
      const message = Buffer.from(detail ?? "", "hex").toString("utf8");
      throw new Error(`owner actor ${name} failed: ${status} ${code} ${message}`);
    }
  }

  open(path: string): void {
    this.command("OWNER_OPEN", Buffer.from(path, "utf8").toString("hex"));
  }

  close(): void {
    this.command("OWNER_CLOSE");
  }

  stop(): void {
    try {
      this.command("CLOSE");
    } finally {
      this.channel.post({ op: "stop" });
      this.channel.close();
    }
  }
}
