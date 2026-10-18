# Browser, WASM, and key-lifecycle probes (2026-10-17)

Five 100 ms samples after one warmup per case; medians. Sequential timing workers pinned to CPU 0, Intel Core Ultra 9 285K. Chromium 149.0.7827.55; Node v26.10.0. Raw JSON retains samples, fixtures and source/artifact hashes.

## Browser worker crypto

Rust/WASM uses release optimization, LTO, one codegen unit, stripped symbols and `+simd128`. The probe calls the existing approved RustCrypto libraries; JS copies input/nonce/AAD into linear memory and copies results out. Noble allocates its results. WebCrypto includes dispatch/promise/output allocation, reuses an imported key, and executes inside a real Chromium dedicated worker. Initialization, network/compilation, OPFS, SQL and worker RPC are outside timing. All crypto inputs are public benchmark data. Nonces vary on timed encryption; this is not a production allocation scheme.

The entire browser is pinned to one CPU: batch-16 numbers are amortized per-page throughput, **not individual request latency or multi-core scaling**. Both operations use 32-byte keys, 32-byte AAD and 16-byte tags; AES uses 12-byte nonces and XChaCha 24-byte nonces. The 24-byte CSPRNG measurement is separate from browser XChaCha seal timings.

Full ciphertext/tag equality, round trips and rejection of changed ciphertext, tag, AAD, nonce and key pass for every browser backend/size. Native Go XChaCha fixture hashes match the browser fixtures (which Rust/WASM matches byte for byte).

| Backend / suite | Payload | Operation | Batch | Median µs/page |
|---|---:|---|---:|---:|
| noble-js/aes-256-gcm | 8192 | seal | 1 | 166.297 |
| noble-js/aes-256-gcm | 8192 | open | 1 | 139.570 |
| rust-wasm/aes-256-gcm | 8192 | seal | 1 | 54.836 |
| rust-wasm/aes-256-gcm | 8192 | open | 1 | 53.047 |
| webcrypto/aes-256-gcm | 8192 | seal | 1 | 8.270 |
| webcrypto/aes-256-gcm | 8192 | open | 1 | 6.950 |
| webcrypto-batch16/aes-256-gcm | 8192 | seal | 16 | 8.078 |
| webcrypto-batch16/aes-256-gcm | 8192 | open | 16 | 6.392 |
| noble-js/xchacha20-poly1305 | 8192 | seal | 1 | 40.192 |
| noble-js/xchacha20-poly1305 | 8192 | open | 1 | 38.075 |
| rust-wasm/xchacha20-poly1305 | 8192 | seal | 1 | 17.583 |
| rust-wasm/xchacha20-poly1305 | 8192 | open | 1 | 16.257 |
| noble-js/aes-256-gcm | 65536 | seal | 1 | 947.500 |
| noble-js/aes-256-gcm | 65536 | open | 1 | 946.328 |
| rust-wasm/aes-256-gcm | 65536 | seal | 1 | 419.453 |
| rust-wasm/aes-256-gcm | 65536 | open | 1 | 417.402 |
| webcrypto/aes-256-gcm | 65536 | seal | 1 | 39.425 |
| webcrypto/aes-256-gcm | 65536 | open | 1 | 36.925 |
| webcrypto-batch16/aes-256-gcm | 65536 | seal | 16 | 41.819 |
| webcrypto-batch16/aes-256-gcm | 65536 | open | 16 | 37.431 |
| noble-js/xchacha20-poly1305 | 65536 | seal | 1 | 269.896 |
| noble-js/xchacha20-poly1305 | 65536 | open | 1 | 269.961 |
| rust-wasm/xchacha20-poly1305 | 65536 | seal | 1 | 127.554 |
| rust-wasm/xchacha20-poly1305 | 65536 | open | 1 | 125.192 |
| webcrypto | 0 | random32+hkdf+aes-key | 1 | 4.276 |
| browser-csprng | 24 | random | 1 | 0.380 |

## Native Go lifecycle

AES setup includes OS randomness for a 32-byte salt, standard-library HKDF-SHA-256, AES key expansion and GCM initialization. Master key and purpose label are fixed public benchmark inputs. `rotate-0` reuses a prepared writer key; other cases rotate every N pages and include that cost. Counter allocation is timed. XChaCha seal includes fresh OS random 24-byte nonces per operation. Both use reusable output buffers with separate input; this is not the earlier in-place-plus-input-copy benchmark mode. Authenticated open is measured separately. HKDF invocation passes RFC 5869 test case 1; final outputs round-trip. No durable writer epochs or headers are implemented.

| Operation | Payload | Median µs |
|---|---:|---:|
| random32+hkdf+aes-key | 0 | 0.812 |
| aes-counter-rotate-0 | 8192 | 0.839 |
| aes-counter-rotate-1 | 8192 | 1.689 |
| aes-counter-rotate-16 | 8192 | 0.901 |
| aes-counter-rotate-256 | 8192 | 0.844 |
| xchacha-random24-seal | 8192 | 2.066 |
| xchacha-open | 8192 | 2.037 |
| aes-counter-rotate-0 | 65536 | 6.345 |
| aes-counter-rotate-1 | 65536 | 7.182 |
| aes-counter-rotate-16 | 65536 | 6.479 |
| aes-counter-rotate-256 | 65536 | 6.501 |
| xchacha-random24-seal | 65536 | 14.725 |
| xchacha-open | 65536 | 15.239 |

## Whole-engine SQL control (Node/WASI, not browser OPFS)

Existing `impl/wasm` full Rust core, release/LTO/one codegen unit/stripped, versus independent TS core bundled using existing esbuild. In-memory database with 10,000 deterministic integer rows; setup excluded. Statements prepared once. Both paths materialize rendered text rows; timings include current result marshalling. All result arrays agree before and after timing. Five narrow cases do not establish a general winner or database encryption overhead. No WebCrypto/AEAD is integrated into either engine.

| Case | TypeScript µs | Rust/WASM µs |
|---|---:|---:|
| constant | 0.978 | 1.237 |
| point | 3.123 | 1.954 |
| aggregate | 3725.892 | 1239.898 |
| sort | 1809.110 | 646.299 |
| return1000 | 316.473 | 446.593 |

Whole-engine WASM payload: 4656541 bytes, 1564991 gzip, before a browser storage adapter. Crypto-only WASM probe containing both suites: 59800 bytes, 23570 gzip. Neither is an incremental production-encryption size measurement.

## Reproduce

```sh
JED_AEAD_WASM_SIMD=1 mise run bench:aead_browser
mise run bench:aead_lifecycle
mise run bench:aead_wasm_sql
```

These tasks reuse approved dependencies and existing build/browser tooling; they perform no downloads. Set `JED_ESBUILD`, `JED_PLAYWRIGHT` (playwright-core index.mjs), and `JED_NOBLE_HASHES` if tools are installed outside the detected locations. Requires installed wasm32-wasip1 Rust target and Chromium matching Playwright. Browser runner needs localhost serving permission. Five pre-existing private-interface warnings in impl/wasm are unrelated to this experiment.

Outstanding: Firefox/Safari/mobile/ARM and unpinned multi-core tests; cold startup and memory; encrypted OPFS queries/commits; async suspension/queue behavior; nonce lifecycle fault fixtures, security review, key limits and exact authenticated metadata. No production suite or async-engine migration is ratified.
