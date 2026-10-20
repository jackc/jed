# Integrated async page-codec experiment (browser, real OPFS)

A **measurement-only** experiment, not an encryption implementation. It installs page codecs at the
TypeScript pager's `PageTransform` seam ([ts-runtime.md](../../../spec/design/ts-runtime.md) §11.5)
that pay a real AEAD cost for every page crossing the seam, then return the **original plaintext**.
File bytes, rows and costs are therefore unchanged, and every run checks that. It informs the
AES-GCM vs XChaCha comparison in [encryption.md](../../../spec/design/encryption.md) §3.1/§8–§9;
it selects no suite, nonce protocol, format, provider or production dependency.

Run `mise run bench:aead_setup` once (the approved, locked `@noble/ciphers`), then
`mise run bench:aead_codec_browser`. It reuses impl/ts's Vite, Playwright and Chromium and downloads
nothing. It is outside `mise run ci`. `mise run bench:aead_codec_browser --help` lists the settings.

## Variants

| Variant | Codec | Write-side mode |
|---|---|---|
| `none` | no transform | inline (production OPFS today) |
| `identity-async` | async forms that return the input, no work | asynchronous |
| `aes-gcm-w16` | WebCrypto AES-256-GCM seal per encoded page, authenticated open per decoded page | asynchronous |
| `aes-gcm-w4`, `aes-gcm-w1` | as w16, at most 4 / 1 crypto operations in flight | asynchronous |
| `aes-gcm-yield` | as w16, plus a MessageChannel task turn before every 16th operation | asynchronous |
| `xchacha-sync` | pure-JS XChaCha20-Poly1305 (`@noble/ciphers` 2.4.0), synchronous forms only, 24-byte nonce from `crypto.getRandomValues` | inline |

**Benchmark inputs, never a production policy.** The key is the public bytes `00..1f`. AES-GCM
nonces are a fixed prefix plus a per-codec counter; AAD is the page index. Per-writer/per-commit
HKDF key derivation (encryption.md §3.1) is **not** measured. Stored pages stay plaintext, so a
decode cannot open them: it opens a page-sized fixture sealed at setup with the same key, checks
the result, and returns the stored page. Authenticated-open cost depends on length, not content;
repeatedly opening one fixture is cache-hotter than opening distinct pages. Ciphertext is
discarded, so the 16-byte tag/nonce overhead does not change page capacity or I/O size. Setup
validates each codec's round trip and rejection of altered ciphertext, tag and AAD.

**Windows.** The engine's encode/read-ahead window is fixed at 16 pages (ts-runtime.md §11.3).
Windows 4 and 1 are emulated by a limiter inside the codec, without changing the engine. The
engine still requests up to 16 encodes before it writes.

## Workload and procedure

Each **process run** launches a fresh headless Chromium with a fresh profile and OPFS. One page
starts one dedicated worker. The worker drives the TS core directly over a real
`FileSystemSyncAccessHandle` through `runAsync`, with the production worker's MessageChannel task
scheduler. The public message transport is excluded. Variants run in an order rotated by the run
index. Each variant gets a new 8 KiB-page file:

- setup: `t` (2,000 rows, 100-byte text) and `s` (16,000 rows, 200-byte text, about 430 leaves);
- **single-row**: 20 warmups, then 200 durable autocommit `INSERT`s into `t`, timed per statement;
- **64-row**: 5 warmups, then 40 `BEGIN; INSERT … SELECT` of 64 rows, with `COMMIT` timed;
- **large**: 1 warmup, then 6 transactions inserting 16,000 rows of 200-byte text into a new table
  (about 430 dirty pages each), with `COMMIT` timed;
- **cold open** ×8: close, then open, timed (the last commit is large, so recovery validates its
  manifest and reads every listed dependency). Handle acquisition is not timed. "Cold" means a new
  engine handle with an empty page cache; the OS file cache is not dropped;
- **cold scan**: the first `SELECT sum(length(v)) FROM s` after each open; **warm scan**: two more
  scans on the same handle (the 256 MiB page cache holds `s`).

Every write is durable (`synchronous = on`, OPFS `flush()` at the barrier). Latency samples are
taken with the main thread idle. Afterwards, a **responsiveness pass** repeats each operation twice.
That covers a burst of 50 single-row commits, one 64-row commit, one large commit, open, cold scan
and warm scan. During each operation the main thread posts a ping every 2 ms (Chromium clamps
repeated intervals to about 4 ms), and the worker echoes it from a port handler. A ping is answered
only when the worker's event loop is free, which is when the runtime could admit a cancel or close
request. The pass records every round trip, whether each was answered before the operation
finished, and main-thread animation-frame gaps. Each run also measures an idle-worker ping floor and
runs an engine-free probe: three chains of 2,000 awaited 8 KiB WebCrypto seals, recording the time
spent inside the `encrypt()` call itself.

At the end of each variant the worker records query checksums and the SHA-256 of the whole file.
The summary requires them to be identical across all variants and runs. Pages sealed/opened per
operation are counted, giving a per-page delta against `none`. `results.json` holds every raw
sample and observation, source hashes, built-artifact hashes and environment metadata. The report
uses nearest-rank percentiles over pooled samples, plus each run's median.

## Limitations

One x86-64 Linux machine, headless Chromium only, no Firefox/Safari, ARM or mobile. OS file cache
warm. Timer resolution is 5 µs (the page is cross-origin isolated). No key derivation, real nonce
allocation, ciphertext layout, crash/fault behavior or public-API transport. Engine-direct worker,
not the production `RuntimeServer`, so admission of real control messages was not exercised.
Results are integrated storage costs for this workload, not a general database slowdown percentage.

## Results

See [the 2026-10-19 report](../results/2026-10-19-async-codec/report.md).
