# Integrated async page codec over real OPFS (2026-10-19)

**Measurement only. No suite, nonce protocol, format, provider or dependency is selected.**
This report informs the AES-256-GCM vs XChaCha20-Poly1305 comparison in
[encryption.md](../../../spec/design/encryption.md) §3.1/§8–§9. It is the first integrated
browser measurement of page crypto through the TS core's asynchronous write side
([ts-runtime.md](../../../spec/design/ts-runtime.md) §11).

Method, variants and limitations: [bench/aead/codec/README.md](../codec/README.md). The generated
tables over all samples ([summary.md](2026-10-19-async-codec/summary.md)) and every raw latency
sample, responsiveness observation (each ping round trip and frame gap), probe and the metadata
([samples.json](2026-10-19-async-codec/samples.json), compacted from the run's `results.json` by
`bench/aead/codec/compact.rb`) are raw evidence. Following `.gitignore`, they are kept only on the
measurement branch (`claude/async-aead-page-codec-bench`), in a separate commit that is not meant
for `master`.

## Setup

- Harness revision `be1da62a` (clean tree), `mise run bench:aead_codec_browser`, default settings.
- Five process runs; each launches a fresh headless Chromium 149.0.7827.55 (V8 14.9.207.21) with a
  fresh profile and OPFS. Variant order is rotated per run.
- Linux 7.0.0 x86-64, Intel Core Ultra 9 285K, 24 logical CPUs, no CPU pinning. The page is
  cross-origin isolated (5 µs timers).
- 8 KiB pages, `synchronous = on`. The engine is driven directly in a dedicated worker over a real
  `FileSystemSyncAccessHandle`, with the production MessageChannel scheduler.
- Integrity: the final 41,418,752-byte file has the same SHA-256 in all 35 variant runs, and every
  query check matches. Each codec counted 31,495 page seals and 75,380 page opens. The measurement
  codecs changed no bytes.

"Crypto" is the wall time from issuing each AEAD call to holding its result, summed per operation.
Under 16- or 4-way windows it also counts calls queued behind one another (see finding 1), so the
**window-1 figure is the in-situ primitive cost**.

## Findings

### 1. Chromium's worker WebCrypto is not concurrent and does not yield

The engine-free probe ran 15 chains of 2,000 awaited 8 KiB AES-GCM seals. They averaged
**11.5 µs/seal**, and **92% of that time is spent inside the `encrypt()` call**: the browser does
the work on the calling thread. The promise then resolves without a task boundary, and **0 of 107
pings** were admitted while a chain ran.

This has two consequences on this browser:

- **The encode window buys nothing.** Window sizes 16, 4 and 1 give commit and open latencies
  within process-run noise of each other (large commit medians 191.4 / 192.4 / 194.8 ms).
  Overlapping awaited WebCrypto calls does not overlap their execution.
- **Awaiting crypto does not make the worker responsive.** An async codec moves the write side off
  the inline path but leaves the worker's event loop as blocked as with no codec (finding 3).

These are observations of one Chromium build on one machine. Other engines and versions may
dispatch WebCrypto differently, so the seam's window should stay. On this evidence, concurrency is
not a reason to prefer AES-GCM over a synchronous pure-JS cipher.

### 2. Integrated cost per operation

Pooled medians in ms over five runs. "Crypto" is the window-1 AES or synchronous XChaCha in-situ
total.

| Operation | pages sealed/opened | none | identity-async | AES-GCM (w16 / w1) | AES crypto (w1) | XChaCha sync | XChaCha crypto |
|---|---:|---:|---:|---:|---:|---:|---:|
| Single-row autocommit INSERT | 4 / 2 | 0.955 | 1.145 | 1.100 / 1.030 | 0.055 | 1.340 | 0.250 |
| 64-row COMMIT | 6 / 1 | 1.400 | 1.575 | 1.620 / 1.925 | 0.080 | 1.860 | 0.320 |
| Large COMMIT (16,000 rows) | 461 / 0 | 185.7 | 190.9 | 191.4 / 194.8 | 3.6 | 207.7 | 18.9 |
| Cold open, large last commit | 0 / 481 | 86.7 | 99.3 | 95.9 / 98.6 | 3.3 | 174.5 | 20.0 |
| Cold full scan (~458 leaves) | 0 / 458 | 69.6 | 85.4 | 79.8 / 72.3 | 3.4 | 127.1 | 18.5 |
| Warm full scan | 0 / 0 | 19.5 | 19.8 | 19.9 / 19.9 | — | 19.9 | — |

p95s, per-run medians and the window-4 and yield variants are in [summary.md](2026-10-19-async-codec/summary.md).

- **AES-GCM's in-situ cost is about 7–9 µs per page**, consistent with the isolated browser
  primitive (8.27/6.95 µs seal/open in the [lifecycle report](2026-10-17-browser-lifecycle.md)).
  That is 0.06–0.08 ms on a small commit (about 5–8% of the statement), 3.6 ms on a 461-page commit
  (about 2%), and about 3.4 ms on a cold open or cold scan of about 470 pages (about 4–5%).
- **XChaCha's in-situ cost is about 40–46 µs per page**, also matching its primitive (Noble
  40.19/38.08 µs). That is 0.25–0.32 ms on a small commit (about 25%), about 19 ms on a large commit
  (about 10%), and about 19–20 ms on a cold open or scan (about 25%). Its measured cold open/scan
  deltas (+88 / +58 ms) exceed the time inside the cipher calls. The rest was not attributed: it may
  be allocation/GC pressure from the pure-JS output buffers, or simply noise (next bullet).
- **Cold open and cold scan are noisy across process runs**, even with no codec: run medians span
  44–161 ms (open) and 50–114 ms (scan). AES-GCM's 3–4 ms is well below that variation, so the
  table's AES deltas for those rows are not resolvable. Only XChaCha's open/scan slowdown exceeds
  the noise. Commits and the warm scan vary much less between runs.
- **The async mode itself** (identity-async: no crypto, only suspension) cost about 0.1–0.2 ms per
  small commit and about 5 ms on the large commit, both within or near run-to-run variation. A warm
  scan hits the plaintext page cache and is unchanged for every codec.
- Large commits are dominated by non-crypto work (about 186 ms without a codec: serialization, CRC
  and manifest work, 461 OPFS writes and the flush), so any suite costs a small fraction there.

An earlier five-run pass of the same harness (dirty tree, without crypto accounting, not retained)
gave the same ordering: AES-GCM about 1.03–1.07× on large commits, XChaCha about 2.07× on cold
open and 1.82× on cold scan.

### 3. Event-loop responsiveness

The main thread pinged the worker about every 4 ms during each operation. The worker can admit a
ping only when its event loop is free, which is when the runtime could admit a cancel or close.
Main-thread animation frames never exceeded 16.7 ms in any observation: the main thread is
unaffected in every variant. Worker results, 10 observations per cell:

| Operation | none | AES w16 | AES w1 | AES + yield/16 | XChaCha sync |
|---|---|---|---|---|---|
| Open, pings admitted (max RTT median) | 1/367 (113 ms) | 0/423 (145 ms) | 1/339 (111 ms) | 353/367 (16 ms) | 0/361 (125 ms) |
| Large txn, pings admitted (max RTT median) | 292/753 (181 ms) | 286/763 (186 ms) | 292/762 (187 ms) | 787/826 (119 ms) | 285/797 (204 ms) |
| Cold scan, max RTT median | 1.2 ms | 1.0 ms | 1.2 ms | 0.9 ms | 0.9 ms |

- **Commit and open block the worker for their whole duration**, with or without a codec, and with
  AES-GCM at every window size. That follows from finding 1, plus a write side whose device steps
  are inline on OPFS. (In the "large" observation, the pings admitted arrive during the INSERT's
  cooperative execution, not during the COMMIT.)
- **Scans stay responsive** for every variant: query page reads take a task turn per read (§10 policy).
- **An explicit task turn every 16 crypto operations** (`aes-gcm-yield`) restores admission during
  open (max RTT median 16 ms) and cuts the large-commit block from about 186 to about 119 ms. That
  remainder is synchronous serialization and writes, which no codec choice changes. The turns cost
  about 15 ms (8%) on the large commit (206.3 vs 191.4 ms) and are within noise elsewhere. A
  synchronous XChaCha codec gives no such point; equivalent responsiveness would need engine-side
  yields between pages, which are not implemented.

## What this does and does not establish

- It establishes integrated costs for one Chromium/x86 configuration: AES-GCM via WebCrypto is
  about 7–9 µs per page in situ, pure-JS XChaCha about 40–46 µs. Neither suite needs the encode
  window's concurrency here, and neither makes the worker responsive by itself.
- It does not measure Firefox/Safari, ARM/mobile, key derivation per writer or commit, nonce
  allocation, real ciphertext layout or capacity, crash behavior, or the production
  `RuntimeServer` admission path. It is not a general database slowdown percentage.
- **No suite is chosen.** Engine-side yield points in long commits and opens are an independent
  responsiveness question, raised here but not decided.
