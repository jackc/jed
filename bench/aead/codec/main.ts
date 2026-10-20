// Page driver for the integrated codec experiment (README.md). It runs one process run: every
// variant, in an order rotated by the run index, over a fresh OPFS file in the engine worker.
// Latency samples are taken with the main thread idle; responsiveness observations run separately
// while the main thread pings the worker every few milliseconds and watches its own frames.
import { VARIANTS, type VariantName } from "./codecs.ts";
import type { Config } from "./worker.ts";

const PING_INTERVAL_MS = 2; // setInterval; Chromium clamps repeated intervals to about 4 ms
const OBSERVED = ["single-burst", "tx64", "large", "open", "scan-cold", "scan-warm"] as const;

type Settings = Record<string, string | undefined>;

function integer(settings: Settings, key: string, fallback: number): number {
  const raw = settings[key];
  if (raw === undefined || raw === "") return fallback;
  const n = Number(raw);
  if (!Number.isSafeInteger(n) || n < 0) throw new Error(`invalid ${key}`);
  return n;
}

function configFrom(settings: Settings): Config {
  const p = "JED_AEAD_CODEC_";
  return {
    singleWarmups: integer(settings, `${p}SINGLE_WARMUPS`, 20),
    singleSamples: integer(settings, `${p}SINGLE`, 200),
    tx64Warmups: integer(settings, `${p}TX64_WARMUPS`, 5),
    tx64Samples: integer(settings, `${p}TX64`, 40),
    largeWarmups: integer(settings, `${p}LARGE_WARMUPS`, 1),
    largeSamples: integer(settings, `${p}LARGE`, 6),
    largeRows: integer(settings, `${p}LARGE_ROWS`, 16000),
    scanRows: integer(settings, `${p}SCAN_ROWS`, 16000),
    coldOpens: integer(settings, `${p}OPENS`, 8),
    warmScansPerOpen: integer(settings, `${p}WARM_SCANS`, 2),
    burstCommits: integer(settings, `${p}BURST`, 50),
  };
}

function rotated<T>(items: T[], by: number): T[] {
  const k = by % items.length;
  return [...items.slice(k), ...items.slice(0, k)];
}

let nextId = 0;
const pending = new Map<number, { resolve: (v: any) => void; reject: (e: Error) => void }>();

function call(worker: Worker, cmd: string, args: unknown = {}, transfer: Transferable[] = []) {
  const id = nextId++;
  return new Promise<any>((resolve, reject) => {
    pending.set(id, { resolve, reject });
    worker.postMessage({ id, cmd, args }, transfer);
  });
}

// observe runs one worker operation while pinging it and sampling animation frames.
async function observe(worker: Worker, port: MessagePort, name: string, config: Config) {
  const pings: number[] = [];
  const answeredAt: number[] = [];
  const sent = new Map<number, number>();
  let seq = 0;
  let started = 0;
  port.onmessage = (event) => {
    const now = performance.now();
    const at = sent.get(event.data);
    if (at !== undefined) {
      pings.push(now - at);
      answeredAt.push(now - started);
    }
    sent.delete(event.data);
  };
  const frames: number[] = [];
  let last: number | undefined;
  let frame = 0;
  const onFrame = (now: number) => {
    if (last !== undefined) frames.push(now - last);
    last = now;
    frame = requestAnimationFrame(onFrame);
  };
  frame = requestAnimationFrame(onFrame);
  await new Promise((r) => setTimeout(r, 40));
  const interval = setInterval(() => {
    sent.set(seq, performance.now());
    port.postMessage(seq++);
  }, PING_INTERVAL_MS);
  started = performance.now();
  const sample = await call(worker, "observed", { name, config });
  const elapsedMs = performance.now() - started;
  clearInterval(interval);
  // Pings still queued behind the operation are answered once it ends; wait for them.
  const deadline = performance.now() + 2000;
  while (sent.size > 0 && performance.now() < deadline) await new Promise((r) => setTimeout(r, 5));
  cancelAnimationFrame(frame);
  return {
    name,
    workerMs: sample.ms,
    encoded: sample.encoded,
    decoded: sample.decoded,
    extra: sample.extra,
    mainElapsedMs: elapsedMs,
    pingsSent: seq,
    pingsLost: sent.size,
    pingRttMs: pings,
    // Arrival of each echo relative to sending the command; earlier than mainElapsedMs means the
    // worker admitted that ping while the operation was still running.
    pingAnsweredAtMs: answeredAt,
    pingsAdmittedDuringOp: answeredAt.filter((t) => t >= 0 && t < elapsedMs).length,
    frameGapsMs: frames,
  };
}

async function runCodecBenchmark(settings: Settings) {
  const config = configFrom(settings);
  const runIndex = integer(settings, "JED_AEAD_CODEC_RUN_INDEX", 0);
  const repetitions = integer(settings, "JED_AEAD_CODEC_RESP_REPS", 2);
  const selected = (settings.JED_AEAD_CODEC_VARIANTS ?? "")
    .split(",")
    .filter((v) => v !== "") as VariantName[];
  for (const v of selected) if (!VARIANTS.includes(v)) throw new Error(`unknown variant ${v}`);
  const variants = rotated(selected.length > 0 ? selected : VARIANTS, runIndex);

  const worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
  worker.onmessage = ({ data }) => {
    const p = pending.get(data.id);
    pending.delete(data.id);
    if (data.error) p?.reject(new Error(data.error));
    else p?.resolve(data.result);
  };
  const channel = new MessageChannel();
  try {
    const environment = await call(worker, "init", { port: channel.port2 }, [channel.port2]);
    const idle = await observeIdle(channel.port1);
    // Engine-free probe of how this browser schedules awaited WebCrypto in a worker.
    const probe = [];
    for (let r = 0; r < 3; r++) probe.push(await observe(worker, channel.port1, "webcrypto-chain", config));
    const results = [];
    for (const variant of variants) {
      document.getElementById("status")!.textContent = `run ${runIndex}: ${variant}`;
      console.log(`JED_CODEC_PROGRESS run ${runIndex} ${variant}`);
      const setup = await call(worker, "setup", { variant, config });
      const latency = await call(worker, "latency", { config });
      const observations = [];
      for (let r = 0; r < repetitions; r++) {
        for (const name of OBSERVED) {
          if (name === "open") await call(worker, "close");
          observations.push(await observe(worker, channel.port1, name, config));
        }
      }
      const finish = await call(worker, "finish");
      results.push({ variant, setup, latency, observations, finish });
    }
    return {
      runIndex,
      variantOrder: variants,
      config,
      pingIntervalMs: PING_INTERVAL_MS,
      environment: {
        ...environment,
        userAgent: navigator.userAgent,
        mainCrossOriginIsolated: crossOriginIsolated,
        visibilityState: document.visibilityState,
      },
      idle,
      webCryptoProbe: probe,
      results,
    };
  } finally {
    worker.terminate();
  }
}

// The ping round trip with an idle worker: the floor of the responsiveness measurement.
async function observeIdle(port: MessagePort) {
  const pings: number[] = [];
  const sent = new Map<number, number>();
  let seq = 0;
  port.onmessage = (event) => {
    const at = sent.get(event.data);
    if (at !== undefined) pings.push(performance.now() - at);
    sent.delete(event.data);
  };
  const interval = setInterval(() => {
    sent.set(seq, performance.now());
    port.postMessage(seq++);
  }, PING_INTERVAL_MS);
  await new Promise((r) => setTimeout(r, 500));
  clearInterval(interval);
  await new Promise((r) => setTimeout(r, 50));
  return { pingRttMs: pings };
}

declare global {
  interface Window {
    runCodecBenchmark: typeof runCodecBenchmark;
  }
}
window.runCodecBenchmark = runCodecBenchmark;
