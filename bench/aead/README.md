# Page-sized AEAD experiment

This is an isolated benchmark, not an encryption implementation or a production dependency
selection. It measures AES-256-GCM and ChaCha20-Poly1305 over hot page-sized buffers, following
[the encryption design](../../spec/design/encryption.md). No database or PostgreSQL cluster is
needed. All dependencies below were explicitly approved for this experiment on 2026-10-17.

| Backend | Implementation | Lifecycle measured |
|---|---|---|
| Rust | RustCrypto `aes-gcm` 0.11.1 / `chacha20poly1305` 0.11.0 | Cached cipher/key setup; allocating and reusable in-place output |
| Go | Go 1.27.1 `crypto/aes` + `crypto/cipher`; `golang.org/x/crypto` 0.58.0 | Cached cipher/key setup; allocating and reusable in-place output; `CGO_ENABLED=0` |
| TypeScript, pure JS | `@noble/ciphers` 2.4.0 | Synchronous allocating API; a fresh cipher object per message |
| Node native | `node:crypto` / OpenSSL | Synchronous allocating API; a fresh cipher context per message |
| Node WebCrypto | `crypto.subtle`, AES-GCM only | Imported key reused; allocating async API; one operation awaited at a time |

The key-granularity probe (below) also uses RustCrypto `hkdf` 0.13.0 (with `hmac` 0.13.0) and
`sha2` 0.11.0, approved for it on 2026-10-19, and Go's standard `crypto/hkdf`.
Versions of transitive dependencies are recorded in Cargo.lock, go.sum and package-lock.json.
Native implementations use their default CPU dispatch, without forcing portable or custom CPU
backends. Node native/WebCrypto are comparison baselines, not an approved engine integration.
The pure JS implementation runs under Node here; this is not a browser measurement.

## Reproduce

Run `mise run bench:aead_setup` once to prepare the approved locked dependencies, then
`mise run bench:aead`. Subsequent measurement runs build Rust offline and Go with its module
proxy disabled. The task fails if dependencies have not been prepared; it does not silently
substitute another backend. No dependency is added to an engine manifest.

Optional environment settings:

- `JED_AEAD_SAMPLES=5`: measured samples per case, after one discarded warmup sample.
- `JED_AEAD_SAMPLE_MS=100`: minimum duration of each warmup/measured sample.
- `JED_AEAD_CPU`: Linux logical CPU; defaults to the first CPU allowed by process affinity.
- `JED_AEAD_RESULTS`: output directory; default `bench/results/aead-<UTC timestamp>-<pid>`.

Workers run sequentially and, on Linux, on the same pinned CPU. Power management and other
machine activity are not controlled. Each sample checks elapsed time after batches of 64
operations. The summary reports the median (upper middle sample for an even count), minimum,
maximum, microseconds/operation and MiB/s of **plaintext**. Runtime startup, compilation and
untimed fixture validation are excluded. Source hashes, CPU features, tool versions, Node's
OpenSSL version, selected environment flags and affinity are recorded in `metadata.json`.

## Inputs and verification

Payload lengths are 4,096, 8,192, 16,384 and 65,536 bytes. These are whole-page-sized payloads,
not a final file-layout claim: the encrypted page's eventual header/tag will consume capacity.
All backends use a 32-byte key (`00` through `1f`), a 12-byte nonce, 32 bytes of AAD (`a0` through
`bf`), and a 16-byte tag. Plaintext byte `i` is `(31*i + 7) mod 256`. The nonce consists of the
big-endian payload length (4 bytes) and an operation counter (8 bytes).

Counter zero is the cross-backend fixture. Seal advances the counter on every timed/warmup
operation, including across allocation modes; open repeatedly authenticates the fixed fixture.
The counters and keys are public test inputs, reset in each process and rerun. They are **not a
production nonce-allocation scheme** and carry no secrets.

Before any timing worker starts, every backend checks round trips and rejects independently
altered ciphertext, tag, AAD and key. The driver compares the **entire ciphertext and tag** for
every algorithm/size across all applicable backends. Timing workers repeat those checks, and
reusable-output paths check their final result. Rust uses `black_box`; Go and JS consume output
bytes. Output and assertions are outside timed regions except authentication error checking
and the small output-consumption operation.

## What the timing includes

Allocating modes use each library's ordinary allocating interface. Node assembles ciphertext
and tag with `Buffer.concat`; this copying and per-message context setup are included. Rust and
Go keep key schedules/cipher objects outside the loop; Noble creates the message cipher within
the loop, as its API requires. These are usable API costs, not identical bare-kernel measurements.

Reusable Rust/Go modes preallocate one output buffer and copy the original plaintext/ciphertext
into it before each in-place operation. That copy is timed, avoiding the unrealistic assumption
that the pager can destroy its shared plaintext cache. Output allocation is excluded in that
mode; internal library work remains included. Async WebCrypto includes promise/dispatch latency
with one operation in flight, not maximum pipelined throughput. Repeated open of one hot fixture
also does not model the cache behavior of reading a large database.

Disk I/O, SQL, page cache misses, manifests, KDF, key wrapping, rotation, spill-file protection,
and real browser/OPFS integration are excluded. No result is a database slowdown percentage or a
side-channel/security certification. Selection also needs nonce-safety and supported-platform
analysis; fast AES on this CPU does not establish the best choice on every host.

## Results

Measured 2026-10-17 on Linux x86-64, Intel Core Ultra 9 285K, logical CPU 0,
Rust 1.99.0, Go 1.27.1, Node 26.10.0 (OpenSSL 3.5.8).
Five samples of at least 100 ms followed one 100 ms warmup for each of 104 timing cases.
All cross-backend fixtures and authentication-rejection checks passed before timing.

Default-page-size results (8 KiB plaintext; median microseconds per operation):

| Backend | AES-GCM seal / open (µs) | ChaCha20-Poly1305 seal / open (µs) |
|---|---:|---:|
| rustcrypto | 2.60 / 2.67 | 4.65 / 4.65 |
| go | 0.87 / 0.78 | 2.00 / 1.98 |
| noble-js | 112.10 / 111.26 | 30.51 / 29.97 |
| node-native | 4.11 / 3.88 | 6.38 / 5.68 |
| node-webcrypto | 11.55 / 11.64 | — |

Rust/Go rows use reusable buffers **including the input copy**; JS/Node rows use allocating
APIs. [All sizes and both native allocation modes](results/2026-10-17.md) and
[all raw timing samples, metadata and fixture hashes](https://github.com/jackc/jed/blob/f97c2e6b8d50b07da29aeabb4f609ed78bb50045/bench/aead/results/2026-10-17.json) are retained.
The full per-worker outputs remain locally under `bench/results/aead-20261017-230702-21/` (gitignored).

On this CPU, native AES-GCM wins against ChaCha20-Poly1305. Pure-JS ChaCha wins by about 3.7×
over pure-JS AES at 8 KiB. The Go AES reusable-buffer path is around 0.8–0.9 µs per page;
Rust is around 2.6–2.7 µs; native Node around 3.9–4.1 µs. Pure-JS AES costs roughly 111–112 µs
and pure-JS ChaCha roughly 30 µs. The WebCrypto sequential API costs about 11.6 µs for AES.

Those differences make the synchronous browser/JS path a material algorithm-selection concern.
They do not justify choosing a cipher solely from this one accelerated x86 machine. Browser,
ARM and portable-fallback measurements, plus integrated storage measurements, remain outstanding.
No production cipher, nonce construction, KDF or crypto backend is selected by this experiment.

### Browser, WASM, and writer-key follow-up

The [browser/lifecycle report](results/2026-10-17-browser-lifecycle.md) extends this investigation
using only the already approved libraries and existing tooling. `mise run bench:aead_browser`
compares pure JS, JS-wrapped RustCrypto/WASM and actual Chromium-worker WebCrypto at 8/64 KiB;
`mise run bench:aead_lifecycle` measures native Go AES key derivation/amortization and XChaCha
with random nonces; `mise run bench:aead_wasm_sql` compares the existing whole Rust/WASI engine
wrapper with the TS core on five in-memory SQL probes under Node. The last is **not a browser
OPFS result**. These are isolated probes, not an encrypted codec or an async engine refactor.

With LTO and WASM SIMD enabled, 8 KiB browser AES seal/open was 54.84/53.05 µs through Rust/WASM,
166.30/139.57 µs in Noble, and 8.27/6.95 µs through WebCrypto. Browser XChaCha was 17.58/16.26 µs
through Rust/WASM and 40.19/38.08 µs in Noble. Native Go random salt + HKDF + AES/GCM setup cost
0.81 µs; amortizing it over 16 pages gave 0.90 µs/page AES seal, versus 2.07 µs/page for XChaCha
including fresh randomness. These numbers motivate an async page-loading/commit-encryption
prototype; native AES acceleration is not inherited merely by compiling Rust to WASM.

The report records all samples and validation, copying/allocation, CPU affinity, batching,
build flags and artifact sizes. Cross-browser/ARM portability, whole-file crash behavior and
integrated database performance remain open. See [the design investigation](../../spec/design/encryption.md#81-browser-execution-and-the-rustwasm-option).

### Key granularity: per page vs per commit attempt (native Go and Rust)

`mise run bench:aead_granularity` ([2026-10-19 report](results/2026-10-19-granularity.md))
compares the two random-salt HKDF key granularities of [encryption.md §3.1](../../spec/design/encryption.md#key-granularity-per-page-or-per-commit-attempt)
at 4, 8 and 16 KiB: a salt, HKDF-SHA-256 derivation and AES-256-GCM key setup for every page
seal and every page open (no key cache), versus one derivation per commit of 1, 4, 16 or 256
pages with ordinal nonces, read through a bounded salt-keyed cache that misses once per commit
(`Commit 1` is the cold case, every page from a different commit), plus an all-hit cache and a
fixed-key reference. Each runs under two HKDF layouts: the salt as the Extract salt per key, or
one Extract at unlock and the salt in a per-key Expand's info. Workers check RFC 5869 vectors,
round trips and tamper rejection, and Go and Rust must produce identical derived keys and
ciphertext for fixed salts, before any timing. Settings are the `JED_AEAD_*` variables above.

At 8 KiB, a full derive cost 0.81/0.57 µs (extract/expand layout) in Go and 0.34/0.18 µs in
Rust. Per-page keys raised a cache-miss open from 0.90 to 1.76/1.41 µs in Go and from 2.72 to
2.98/2.81 µs in Rust; per-commit keys read cold cost slightly more than per-page, and with 16 or
more pages per commit came within 7% of the fixed key. No granularity is selected.

### Integrated async page codec (browser OPFS)

`mise run bench:aead_codec_browser` ([method](codec/README.md),
[2026-10-19 report](results/2026-10-19-async-codec.md)) installs measurement-only codecs at
the TS pager's page seam over real OPFS in headless Chromium. They pay WebCrypto AES-GCM or
synchronous Noble XChaCha per page, return the plaintext, and are verified byte-identical. In situ,
AES cost about 7–9 µs and XChaCha about 40–46 µs per 8 KiB page. Chromium's worker WebCrypto ran
inside the call and yielded no concurrency or event-loop turn. No suite is selected.

### Node native-engine comparison

The [three-way Node report](results/2026-10-18-node.md) adds the existing Rust/Node-API package to
the TS and Rust/WASI comparison. `mise run bench:aead_node_compare` runs five prepared warm-read
queries over identical 10,000-row **file-backed** databases, with result conversion included.
Native Rust wins the sort and larger result-set probes; TS wins the constant query and WASM wins
the point/aggregate probes. These measurements support explicit deployment choices, not a universal
winner. The report retains seven 300 ms samples and the shorter initial run, validation and caveats.
The follow-on [browser/Node runtime proposal](../../spec/design/ts-runtime.md) uses this evidence
to motivate platform adapters and an async public API; it selects no production engine backend.


## Dependency and binary footprint

`mise run bench:aead_size` builds four small SQL embeddings per native language: existing jed,
jed plus AES-GCM, jed plus ChaCha20-Poly1305, and jed plus both. Each executes SQL supplied on the
command line; crypto variants also encrypt, decrypt and check runtime input. Crypto is invoked
beside the engine, not integrated into its pager. This estimates the linked library increment
including a small calling wrapper. It avoids confusing a benchmark harness's size with the AEAD
code itself. All native executables passed the SQL and crypto round-trip smoke checks.

The 2026-10-17 Linux x86-64 run used Rust 1.99.0 release optimization with symbols stripped and
no explicit LTO, and Go 1.27.1 with `CGO_ENABLED=0 -trimpath -ldflags='-s -w'`. Baseline embedding
executables were 8,292,528 bytes (Rust) and 9,330,848 bytes (Go). The native deltas were:

| Included algorithms | Rust added KiB | Go added KiB |
|---|---:|---:|
| AES-256-GCM | 49.2 | 52.0 |
| ChaCha20-Poly1305 | 31.3 | 72.0 |
| Both | 77.0 | 120.0 |

For JS, esbuild 0.28.2 bundled/minified only the selected Noble cipher exports for the browser:

| Included algorithms | Minified KiB | Gzip KiB |
|---|---:|---:|
| AES-256-GCM | 8.6 | 3.7 |
| ChaCha20-Poly1305 | 10.8 | 4.6 |
| Both | 16.5 | 6.8 |

JS figures are standalone cipher bundles, not measured deltas against a complete jed browser
bundle; shared utility code could affect that delta. Native Node/WebCrypto reuse crypto already
in the runtime, so there is no separate library to bundle for those providers. Their wrapper
and integration still have a footprint.

**Dependency delivery differs from linked code.** In the approved default-feature benchmark
configuration, AES-GCM's native Rust graph contains 21 crates including itself (~1.52 MiB of
compressed crate archives); ChaCha's contains 18 (~1.33 MiB). Those graphs overlap, include
infrastructure such as integer/array types and CPU dispatch, and overlap with jed's existing
hash dependencies. They are not counts or bytes of entirely new shipped code. Go AES-GCM needs
no external module; the ChaCha module archives are ~2.24 MiB for `x/crypto` and ~2.02 MiB for
`x/sys`, which contain many unrelated packages the linker does not retain. Noble is one package
with no runtime dependencies: ~535 KiB installed including source, declarations and docs, much
larger than the selected bundle.

[Exact sizes, dependency inventory, tool versions and source hashes](https://github.com/jackc/jed/blob/f97c2e6b8d50b07da29aeabb4f609ed78bb50045/bench/aead/results/2026-10-17-size.json)
are retained. Generated probes and binaries remain in the report's local `raw_directory`.
The task reuses approved dependencies offline and needs an already-installed esbuild, selected
with `JED_ESBUILD` or discovered in the local npm execution cache; it installs no bundler.

These are **AEAD library** measurements, not the complete future encryption feature. KDF,
key wrapping, encrypted page format/recovery, rotation, scratch-file encryption and API code
are excluded. KDF code size and its intentionally configurable working memory remain to be
measured after selection. ARM, WASM, other link profiles and final engine integration can change
these sizes. Neither the dependency downloads nor these code deltas measure runtime heap use.
