import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = process.env.JED_CODEC_REPO ?? path.resolve(fileURLToPath(new URL(".", import.meta.url)), "../../../../..");
const archived = process.env.JED_CODEC_BASELINE ?? "/tmp/jed-fused-baseline";
const snapshots = process.env.JED_CODEC_SNAPSHOTS ?? "/tmp/jed-fused-codec";
const out = process.env.JED_CODEC_OUTPUT ?? "/tmp/jed-fused-codec-probe";
const rowsCount = Number(process.env.JED_CODEC_ROWS ?? 200000);
const maxBatchRows = Number(process.env.JED_CODEC_BATCH_ROWS ?? 512);
const maxBatchBytes = Number(process.env.JED_CODEC_BATCH_BYTES ?? 262144);
const warmups = 3;
const samples = 7;
for (const value of [rowsCount, maxBatchRows, maxBatchBytes]) {
  assert.ok(Number.isSafeInteger(value) && value > 0);
}
await mkdir(out, { recursive: true });
await writeFile(`${out}/package.json`, '{"type":"module"}\n');
const sourceFiles = {
  baseline: "baseline.ts",
  fusedDecoder: "decoder-only.ts",
  fusedCombined: "combined.ts",
};
const sources = {};
const sourceHashes = {};
const expectedHashes = new Map((await readFile(`${snapshots}/SHA256SUMS`, "utf8"))
  .trim().split("\n").map((line) => {
    const [hash, filename] = line.split(/\s+/);
    return [filename, hash];
  }));
for (const [name, filename] of Object.entries(sourceFiles)) {
  sources[name] = await readFile(`${snapshots}/${filename}`, "utf8");
  sourceHashes[name] = createHash("sha256").update(sources[name]).digest("hex");
  assert.equal(sourceHashes[name], expectedHashes.get(filename), `frozen source changed: ${name}`);
}
assert.deepEqual(
  sources.fusedDecoder.slice(sources.fusedDecoder.indexOf("class PackedReader")),
  sources.fusedCombined.slice(sources.fusedCombined.indexOf("class PackedReader")),
  "combined and decoder-only readers must be identical",
);
const dependencyHashes = {};
for (const dependency of ["decimal.ts", "async/wire.ts", "async/errors.ts"]) {
  const current = await readFile(path.join(root, "impl/ts/src", dependency), "utf8");
  assert.equal(current, await readFile(path.join(archived, "impl/ts/src", dependency), "utf8"),
    `dependency changed: ${dependency}`);
  dependencyHashes[dependency] = createHash("sha256").update(current).digest("hex");
}
// Both variants use the same dependency instances so separate module copies of Decimal or
// WireBudget cannot introduce an unrelated polymorphic call-site difference.
const modules = {};
for (const [name, source] of Object.entries(sources)) {
  const mapped = source.replace(/from "(\.[^"]+)"/g, (_, relative) =>
    `from "${pathToFileURL(path.resolve(root, "impl/ts/src/async", relative)).href}"`);
  await writeFile(`${out}/${name}.ts`, mapped);
  modules[name] = await import(pathToFileURL(`${out}/${name}.ts`).href);
}
const { Decimal } = await import(pathToFileURL(`${root}/impl/ts/src/decimal.ts`).href);
const { WireBudget, measureValues } = await import(pathToFileURL(`${root}/impl/ts/src/async/wire.ts`).href);
const sourceRows = new Array(rowsCount);
const sourceRowBytes = new Array(rowsCount);
const sourceBytes = Uint8Array.of(0, 1, 254, 255);
for (let i = 0; i < rowsCount; i++) {
  const n = BigInt(i + 1);
  const row = [
    { kind: "int", int: n },
    { kind: "decimal", dec: Decimal.fromBigInt(n) },
    { kind: "bytea", bytes: sourceBytes },
    { kind: "array", dims: [2], lbounds: [1], elements: [{ kind: "int", int: n }, { kind: "int", int: n + 1n }] },
    { kind: "null" },
  ];
  sourceRows[i] = row;
  sourceRowBytes[i] = measureValues(row);
}
function encode(codec, collect = false) {
  const packets = collect ? [] : undefined;
  let at = 0, batches = 0, totalBytes = 0;
  while (at < sourceRows.length) {
    const writer = new codec.PackedRowsWriter(maxBatchBytes);
    let count = 0;
    while (at < sourceRows.length && count < maxBatchRows && writer.append(sourceRows[at])) {
      at++;
      count++;
    }
    assert.ok(count > 0);
    const packet = writer.finish();
    totalBytes += packet.data.byteLength;
    batches++;
    if (collect) packets.push(packet);
  }
  return { rows: at, batches, totalBytes, packets };
}
const packed = encode(modules.baseline, true);
const packets = packed.packets;
function decode(codec) {
  let count = 0, checksum = 0, logicalBytes = 0;
  for (const packet of packets) {
    const budget = new WireBudget(maxBatchBytes);
    budget.reserve(128);
    const decoded = codec.decodePackedRows(packet, budget);
    count += decoded.rows.length;
    logicalBytes += decoded.bytes;
    // Consume both batch boundaries without retaining the full decoded query result.
    checksum += Number(decoded.rows[0][0].int) + Number(decoded.rows.at(-1)[0].int);
  }
  return { rows: count, checksum, logicalBytes };
}
const expectedEncode = { ...packed, packets: undefined };
const expectedDecode = decode(modules.baseline);
assert.equal(expectedDecode.logicalBytes, sourceRowBytes.reduce((a, b) => a + b, 0));
// Validate every encoded byte, every decoded value and every per-row logical charge before
// warmup. The assertions and data construction are excluded from all timings below.
for (const [name, codec] of Object.entries(modules)) {
  const encoded = encode(codec, true);
  assert.deepEqual({ ...encoded, packets: undefined }, expectedEncode, `${name} encoded totals`);
  let offset = 0;
  for (let i = 0; i < packets.length; i++) {
    const packet = encoded.packets[i];
    assert.equal(packet.version, packets[i].version);
    assert.equal(packet.length, packets[i].length);
    assert.deepEqual(new Uint8Array(packet.data), new Uint8Array(packets[i].data), `${name} packet ${i}`);
    const budget = new WireBudget(maxBatchBytes);
    budget.reserve(128);
    const decoded = codec.decodePackedRows(packet, budget);
    const end = offset + packet.length;
    assert.deepEqual(decoded.rows, sourceRows.slice(offset, end), `${name} values ${i}`);
    assert.deepEqual(decoded.rowBytes, sourceRowBytes.slice(offset, end), `${name} row accounting ${i}`);
    assert.equal(decoded.bytes, decoded.rowBytes.reduce((a, b) => a + b, 0));
    assert.equal(budget.used, 128 + decoded.bytes);
    offset = end;
  }
  assert.equal(offset, rowsCount);
  assert.deepEqual(decode(codec), expectedDecode, `${name} decoded totals`);
}
const variants = {
  encode: ["baseline", "fusedCombined"],
  decode: ["baseline", "fusedDecoder"],
};
const result = {
  metadata: {
    node: process.version, v8: process.versions.v8,
    snapshotDirectory: snapshots, dependencyDirectory: `${root}/impl/ts/src`,
    sourceFiles, sourceHashes, dependencyHashes,
    rows: rowsCount, maxBatchRows, maxBatchBytes, warmups, samples,
    actualBatches: packets.length, totalPhysicalBytes: packed.totalBytes,
    logicalBytes: expectedDecode.logicalBytes,
    methodology: "No SQL or worker messaging. Prebuilt representative typed-output rows. Exact full-result packet bytes, values, per-row accounting and budget equality checked before warmup. Shared byte-identical dependencies. Variant order rotates every round, alternating AB/BA for each pair. Encoders discard completed packets; decoders retain one batch. Natural GC remains inside samples; explicit full GC once before warmups only. Encoding and decoding measured separately; timings are not end-to-end throughput or browser responsiveness.",
  },
  raw: { encode: {}, decode: {} },
  order: [],
  summary: {},
};
global.gc?.();
for (const phase of ["encode", "decode"]) {
  const names = variants[phase];
  for (const name of names) result.raw[phase][name] = [];
  for (let round = -warmups; round < samples; round++) {
    const rotation = (round + warmups) % names.length;
    const order = [...names.slice(rotation), ...names.slice(0, rotation)];
    result.order.push({ phase, round, order });
    for (const name of order) {
      const started = performance.now();
      const value = phase === "encode" ? encode(modules[name]) : decode(modules[name]);
      const ms = performance.now() - started;
      assert.deepEqual(value, phase === "encode" ? expectedEncode : expectedDecode);
      if (round >= 0) result.raw[phase][name].push(ms);
      process.stderr.write(`${phase} ${round < 0 ? "warmup" : "sample"} ${round < 0 ? round + warmups + 1 : round + 1} ${name}: ${ms.toFixed(2)} ms\n`);
    }
  }
  result.summary[phase] = Object.fromEntries(Object.entries(result.raw[phase]).map(([name, timings]) => {
    const ordered = [...timings].sort((a, b) => a - b);
    return [name, { min: ordered[0], p50: ordered[3], max: ordered.at(-1) }];
  }));
}
await writeFile(`${out}/results.json`, `${JSON.stringify(result, null, 2)}\n`);
process.stdout.write(`${JSON.stringify(result.summary, null, 2)}\n`);
