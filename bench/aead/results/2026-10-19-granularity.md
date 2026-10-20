# AES-GCM key granularity: per page vs per commit attempt (2026-10-19)

**Measurement only. No granularity, HKDF layout, page format or salt length is selected.**
This report informs the per-page versus per-commit-attempt choice in
[encryption.md](../../../spec/design/encryption.md) §3.1, together with the reserve size (§4.1).
Reproduce with `mise run bench:aead_granularity` (sources: `bench/aead/go/cmd/granularity/`,
`bench/aead/rust/src/bin/granularity.rs`).

## Setup

- Harness revision `ff521c9f` plus the uncommitted probe sources whose SHA-256s are listed at the
  end (the committed sources are byte-identical). Default settings: five 100 ms samples after one
  discarded 100 ms warmup per case, batches of 64 operations, both workers run one after the other,
  pinned to logical CPU 0.
- Linux 7.0.0 x86-64, Intel Core Ultra 9 285K. Go 1.27.1 standard library (`crypto/aes`,
  `crypto/cipher`, `crypto/hkdf`, `crypto/rand`), `CGO_ENABLED=0`. Rust 1.99.0 release build with
  RustCrypto `aes-gcm` 0.11.1, `hkdf` 0.13.0 (`hmac` 0.13.0), `sha2` 0.11.0, and salts from
  `getrandom` 0.4.3 through `aes-gcm`'s `Generate` (no extra crate). `hkdf`/`hmac` were approved
  for this benchmark on 2026-10-19 (CLAUDE.md §14). Default CPU dispatch on both.
- Native Go and Rust only. TS/WebCrypto and the browser are out of scope here (§3.1 finding 3);
  the WebCrypto derivation figure (4.3 µs) is in the earlier lifecycle report.

## What is measured

Each key is AES-256-GCM, derived with HKDF-SHA-256 from a 32-byte public data key, a fresh
32-byte salt and the purpose label `jed bench page key v0`. Two HKDF layouts:

- **extract-per-key**: `HKDF(salt = page/commit salt, IKM = data key, info = label)`: an
  Extract and an Expand per key (Tink's AES-GCM-HKDF shape).
- **expand-per-key**: `PRK = HKDF-Extract(empty salt, data key)` once at unlock, then
  `HKDF-Expand(PRK, info = label ‖ salt)` per key. Rust keeps the keyed `Hkdf` object, so each
  expand reuses the HMAC key state; Go's `hkdf.Expand` takes the PRK bytes and re-keys HMAC on
  every call, and its `string` info costs an allocation. Both are the libraries' ordinary APIs.

A *derive* is: random salt from the OS (seal side only), the HKDF layout, AES key expansion and
GCM initialization. Two granularities:

- **Per page.** Every page seal draws a salt and derives a key; every page open derives the
  page's key from its stored salt. No key cache. The nonce is fixed (all zero), since a key
  seals one page.
- **Per commit attempt (`Commit N`).** Seal draws one salt and derives once per N pages, with
  the page ordinal in the nonce (8 zero bytes then a big-endian u32). Open goes through a
  bounded FIFO cache of derived ciphers keyed by salt (Go `map[[32]byte]`, Rust `HashMap` with
  the default SipHash). Pages are grouped into 1024/N commits and read in commit order with the
  cache bound at 2 entries, below every commit count, so **each commit's first page misses and
  the other N−1 hit**. `Commit 1` is therefore the cold-cache case: every page comes from a
  different commit. The harness checks that the miss count is exactly one per commit read.
- **All-hit cache.** Commits of 16 pages with every key already cached (cache bound 1024):
  the steady-state cost of a lookup.
- **Fixed key** is the reference: one long-lived key, counter nonces, no derivation.

All page cases use the same 1024 distinct pages per size (4, 8, 16 MiB of plaintext at 4, 8,
16 KiB), each with 32 bytes of AAD carrying its page index, cycling through them; open cases cycle
over 1024 stored ciphertexts. Output goes to one reusable buffer and the input copy into it is
timed, as in the main benchmark's in-place mode. Times are per page (per key for key setup) and
include allocation and GC caused by the library APIs (Go's HKDF and cipher constructors
allocate). Disk/OS-cache I/O, SQL, page decoding, and multi-core contention are excluded.

## Validation (before any timing)

Each worker checks RFC 5869 test cases 1 and 3 (PRK and OKM, through both the one-shot and the
extract/expand APIs), then for both layouts, both granularities and every size: round trip, and
rejection of a flipped ciphertext byte (first and last), a flipped tag byte (first and last), a
flipped AAD byte, a salt with one bit flipped, the other layout's key, and the next nonce
ordinal. Fixed-salt fixtures (the two derived keys and the full ciphertext+tag of page 1 per
layout/scheme/size, 14 in all) are byte-identical between Go and Rust; the driver aborts on any
difference. Every timed seal case checks its final output opens; per-commit seal checks one
derivation per N pages; open cases check their output and miss counts.

## Results (median µs)

### Key setup, per key

| Operation | Go | Rust |
|---|---:|---:|
| 32-byte OS random salt | 0.051 | 0.044 |
| AES-256 key expansion + GCM init | 0.164 | 0.047 |
| HKDF, extract-per-key | 0.577 | 0.219 |
| HKDF, expand-per-key | 0.337 | 0.080 |
| Full derive (salt + HKDF + setup), extract-per-key | 0.813 | 0.337 |
| Full derive (salt + HKDF + setup), expand-per-key | 0.572 | 0.180 |

Go's 0.813 µs reproduces the earlier lifecycle probe's 0.812 µs.

### 8 KiB pages (the default)

| Backend | Op | Fixed key | Per page, extract / expand | Commit 1 (cold) | Commit 4 | Commit 16 | Commit 256 | All-hit |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| Go | seal | 0.977 | 1.883 / 1.621 | 1.856 / 1.623 | 1.229 / 1.166 | 1.042 / 1.022 | 0.983 / 0.981 | — |
| Go | open | 0.903 | 1.762 / 1.409 | 1.839 / 1.535 | 1.120 / 1.057 | 0.969 / 0.950 | 0.919 / 0.913 | 0.915 |
| Rust | seal | 2.683 | 3.081 / 2.938 | 3.068 / 2.940 | 2.817 / 2.768 | 2.722 / 2.717 | 2.738 / 2.691 | — |
| Rust | open | 2.720 | 2.981 / 2.814 | 3.071 / 2.916 | 2.853 / 2.794 | 2.794 / 2.748 | 2.748 / 2.777 | 2.778 |

(Commit columns give extract / expand.) Overhead over the fixed key, per 8 KiB page:

| Backend | Per-page open (cache miss) | Per-page seal | Commit-16 open | Commit-16 seal |
|---|---:|---:|---:|---:|
| Go, extract | +0.86 (+95%) | +0.91 (+93%) | +0.07 (+7%) | +0.07 (+7%) |
| Go, expand | +0.51 (+56%) | +0.64 (+66%) | +0.05 (+5%) | +0.05 (+5%) |
| Rust, extract | +0.26 (+10%) | +0.40 (+15%) | +0.07 (+3%) | +0.04 (+1%) |
| Rust, expand | +0.09 (+3%) | +0.26 (+10%) | +0.03 (+1%) | +0.03 (+1%) |

### All sizes

#### extract-per-key layout

| Backend | Bytes | Op | Fixed key | Per page | Commit 1 | Commit 4 | Commit 16 | Commit 256 | All-hit cache |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|
| go | 4096 | seal | 0.501 | 1.340 | 1.330 | 0.727 | 0.573 | 0.516 | — |
| go | 4096 | open | 0.468 | 1.215 | 1.275 | 0.714 | 0.540 | 0.483 | 0.476 |
| go | 8192 | seal | 0.977 | 1.883 | 1.856 | 1.229 | 1.042 | 0.983 | — |
| go | 8192 | open | 0.903 | 1.762 | 1.839 | 1.120 | 0.969 | 0.919 | 0.915 |
| go | 16384 | seal | 1.943 | 2.746 | 2.757 | 2.158 | 1.963 | 1.916 | — |
| go | 16384 | open | 1.742 | 2.603 | 2.704 | 2.014 | 1.873 | 1.786 | 1.751 |
| rustcrypto | 4096 | seal | 1.371 | 1.729 | 1.728 | 1.468 | 1.401 | 1.373 | — |
| rustcrypto | 4096 | open | 1.390 | 1.674 | 1.752 | 1.501 | 1.449 | 1.415 | 1.413 |
| rustcrypto | 8192 | seal | 2.683 | 3.081 | 3.068 | 2.817 | 2.722 | 2.738 | — |
| rustcrypto | 8192 | open | 2.720 | 2.981 | 3.071 | 2.853 | 2.794 | 2.748 | 2.778 |
| rustcrypto | 16384 | seal | 5.368 | 5.783 | 5.729 | 5.490 | 5.325 | 5.368 | — |
| rustcrypto | 16384 | open | 5.377 | 5.645 | 5.797 | 5.508 | 5.411 | 5.385 | 5.363 |

#### expand-per-key layout

| Backend | Bytes | Op | Fixed key | Per page | Commit 1 | Commit 4 | Commit 16 | Commit 256 | All-hit cache |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|
| go | 4096 | seal | 0.501 | 1.091 | 1.082 | 0.667 | 0.556 | 0.508 | — |
| go | 4096 | open | 0.468 | 1.008 | 1.054 | 0.643 | 0.530 | 0.483 | 0.476 |
| go | 8192 | seal | 0.977 | 1.621 | 1.623 | 1.166 | 1.022 | 0.981 | — |
| go | 8192 | open | 0.903 | 1.409 | 1.535 | 1.057 | 0.950 | 0.913 | 0.915 |
| go | 16384 | seal | 1.943 | 2.509 | 2.509 | 2.084 | 1.957 | 1.939 | — |
| go | 16384 | open | 1.742 | 2.363 | 2.454 | 1.939 | 1.805 | 1.754 | 1.751 |
| rustcrypto | 4096 | seal | 1.371 | 1.552 | 1.562 | 1.439 | 1.396 | 1.380 | — |
| rustcrypto | 4096 | open | 1.390 | 1.510 | 1.595 | 1.475 | 1.440 | 1.417 | 1.413 |
| rustcrypto | 8192 | seal | 2.683 | 2.938 | 2.940 | 2.768 | 2.717 | 2.691 | — |
| rustcrypto | 8192 | open | 2.720 | 2.814 | 2.916 | 2.794 | 2.748 | 2.777 | 2.778 |
| rustcrypto | 16384 | seal | 5.368 | 5.615 | 5.537 | 5.468 | 5.323 | 5.342 | — |
| rustcrypto | 16384 | open | 5.377 | 5.452 | 5.682 | 5.530 | 5.438 | 5.437 | 5.363 |

## Findings

- **Derivation is a fixed per-key cost, so its share shrinks with page size and with AES speed.**
  Go's AES-GCM is about three times faster than RustCrypto's on this CPU, while its HKDF and
  key setup are 2.6–4.2 times slower, so per-page keys cost Go 56–95% more crypto time per
  8 KiB cache miss and Rust 3–10%. At 16 KiB, Go's per-page open overhead is 36–49%.
- **The expand-per-key layout saves 0.24 µs per key in Go and 0.16 µs in Rust** (about 30%
  and 47% of the full derive), by doing the Extract once at unlock. Rust benefits more because
  `Hkdf` retains the keyed HMAC state; Go re-keys HMAC from the PRK on each `Expand`.
- **Per commit, a cold cache is no better than per page.** With every page from a different
  commit (`Commit 1`), opens cost 0.05–0.23 µs more than per-page keys (0.08–0.13 at 8 KiB),
  for cache insertion and eviction. The read-side benefit appears only when consecutive misses
  come from the same commit: at 4 pages per commit it recovers most of the derivation, and from
  16 up the cost is within 7% of a fixed key. A cache hit costs about the same as a fixed key.
- **Sealing amortizes as expected**: per-commit seal of 16+ pages is within 1–7% of the fixed key
  on both cores; per-page seal pays one OS random salt plus a derive per page.
- Reserve per page with a 32-byte salt: 48 bytes per page (salt + tag) versus 52 per commit
  (salt + 4-byte ordinal + tag); a 24-byte per-page salt would make it 40.

## Limitations

One CPU model and OS, single-threaded, hot-ish memory (working sets of 4–16 MiB). A real cache
miss also pays storage or OS-cache I/O and page decoding, which these numbers exclude, so the
crypto share of a miss is lower than shown. Read locality by commit depends on workload and is
not modelled beyond the fixed commit-ordered scan. Cache bounds are benchmark choices, not a
design; Rust's `HashMap` uses the default SipHash. No ARM, software-fallback, WASM or WebCrypto
measurements here. This is not a security analysis of either layout.

## Raw data

The run's per-worker JSONL, `results.json` (every sample, fixture hashes, metadata) and
`metadata.json` stay local under `bench/results/aead-granularity-20261020-040141-3296493/`
(gitignored). Every timed case follows, with fixture hashes and source hashes.

| Backend | Scheme | Layout | Commit pages | Cache | Op | Bytes | Median µs | Min–max µs |
|---|---|---|---:|---|---|---:|---:|---:|
| go | key-setup | — | — | — | salt-random32 | 0 | 0.051 | 0.051–0.051 |
| go | key-setup | — | — | — | aes-gcm-key-setup | 0 | 0.164 | 0.156–0.165 |
| go | key-setup | — | — | — | hkdf-extract-per-key | 0 | 0.577 | 0.571–0.581 |
| go | key-setup | — | — | — | derive-extract-per-key | 0 | 0.813 | 0.810–0.816 |
| go | key-setup | — | — | — | hkdf-expand-per-key | 0 | 0.337 | 0.335–0.338 |
| go | key-setup | — | — | — | derive-expand-per-key | 0 | 0.572 | 0.558–0.582 |
| go | fixed-key | — | — | — | seal | 4096 | 0.501 | 0.501–0.503 |
| go | per-page | extract-per-key | — | — | seal | 4096 | 1.340 | 1.329–1.354 |
| go | per-commit | extract-per-key | 1 | — | seal | 4096 | 1.330 | 1.324–1.347 |
| go | per-commit | extract-per-key | 4 | — | seal | 4096 | 0.727 | 0.718–0.741 |
| go | per-commit | extract-per-key | 16 | — | seal | 4096 | 0.573 | 0.566–0.578 |
| go | per-commit | extract-per-key | 256 | — | seal | 4096 | 0.516 | 0.503–0.524 |
| go | per-page | expand-per-key | — | — | seal | 4096 | 1.091 | 1.084–1.098 |
| go | per-commit | expand-per-key | 1 | — | seal | 4096 | 1.082 | 1.082–1.086 |
| go | per-commit | expand-per-key | 4 | — | seal | 4096 | 0.667 | 0.664–0.690 |
| go | per-commit | expand-per-key | 16 | — | seal | 4096 | 0.556 | 0.541–0.575 |
| go | per-commit | expand-per-key | 256 | — | seal | 4096 | 0.508 | 0.506–0.515 |
| go | fixed-key | — | — | — | open | 4096 | 0.468 | 0.464–0.474 |
| go | per-page | extract-per-key | — | — | open | 4096 | 1.215 | 1.211–1.223 |
| go | per-commit | extract-per-key | 1 | miss-per-commit | open | 4096 | 1.275 | 1.259–1.299 |
| go | per-commit | extract-per-key | 4 | miss-per-commit | open | 4096 | 0.714 | 0.699–0.728 |
| go | per-commit | extract-per-key | 16 | miss-per-commit | open | 4096 | 0.540 | 0.537–0.550 |
| go | per-commit | extract-per-key | 256 | miss-per-commit | open | 4096 | 0.483 | 0.482–0.487 |
| go | per-page | expand-per-key | — | — | open | 4096 | 1.008 | 0.997–1.095 |
| go | per-commit | expand-per-key | 1 | miss-per-commit | open | 4096 | 1.054 | 1.042–1.199 |
| go | per-commit | expand-per-key | 4 | miss-per-commit | open | 4096 | 0.643 | 0.640–0.647 |
| go | per-commit | expand-per-key | 16 | miss-per-commit | open | 4096 | 0.530 | 0.518–0.728 |
| go | per-commit | expand-per-key | 256 | miss-per-commit | open | 4096 | 0.483 | 0.482–0.491 |
| go | per-commit | — | 16 | all-hit | open | 4096 | 0.476 | 0.476–0.478 |
| go | fixed-key | — | — | — | seal | 8192 | 0.977 | 0.976–0.991 |
| go | per-page | extract-per-key | — | — | seal | 8192 | 1.883 | 1.872–1.983 |
| go | per-commit | extract-per-key | 1 | — | seal | 8192 | 1.856 | 1.851–1.897 |
| go | per-commit | extract-per-key | 4 | — | seal | 8192 | 1.229 | 1.219–1.259 |
| go | per-commit | extract-per-key | 16 | — | seal | 8192 | 1.042 | 1.039–1.058 |
| go | per-commit | extract-per-key | 256 | — | seal | 8192 | 0.983 | 0.979–0.993 |
| go | per-page | expand-per-key | — | — | seal | 8192 | 1.621 | 1.585–1.666 |
| go | per-commit | expand-per-key | 1 | — | seal | 8192 | 1.623 | 1.593–1.645 |
| go | per-commit | expand-per-key | 4 | — | seal | 8192 | 1.166 | 1.150–1.184 |
| go | per-commit | expand-per-key | 16 | — | seal | 8192 | 1.022 | 1.020–1.026 |
| go | per-commit | expand-per-key | 256 | — | seal | 8192 | 0.981 | 0.973–0.990 |
| go | fixed-key | — | — | — | open | 8192 | 0.903 | 0.896–0.918 |
| go | per-page | extract-per-key | — | — | open | 8192 | 1.762 | 1.755–1.795 |
| go | per-commit | extract-per-key | 1 | miss-per-commit | open | 8192 | 1.839 | 1.815–1.885 |
| go | per-commit | extract-per-key | 4 | miss-per-commit | open | 8192 | 1.120 | 1.112–1.146 |
| go | per-commit | extract-per-key | 16 | miss-per-commit | open | 8192 | 0.969 | 0.965–0.971 |
| go | per-commit | extract-per-key | 256 | miss-per-commit | open | 8192 | 0.919 | 0.909–0.923 |
| go | per-page | expand-per-key | — | — | open | 8192 | 1.409 | 1.399–1.427 |
| go | per-commit | expand-per-key | 1 | miss-per-commit | open | 8192 | 1.535 | 1.484–1.559 |
| go | per-commit | expand-per-key | 4 | miss-per-commit | open | 8192 | 1.057 | 1.048–1.101 |
| go | per-commit | expand-per-key | 16 | miss-per-commit | open | 8192 | 0.950 | 0.940–0.952 |
| go | per-commit | expand-per-key | 256 | miss-per-commit | open | 8192 | 0.913 | 0.905–0.915 |
| go | per-commit | — | 16 | all-hit | open | 8192 | 0.915 | 0.908–0.916 |
| go | fixed-key | — | — | — | seal | 16384 | 1.943 | 1.881–1.955 |
| go | per-page | extract-per-key | — | — | seal | 16384 | 2.746 | 2.735–2.903 |
| go | per-commit | extract-per-key | 1 | — | seal | 16384 | 2.757 | 2.729–2.856 |
| go | per-commit | extract-per-key | 4 | — | seal | 16384 | 2.158 | 2.131–2.195 |
| go | per-commit | extract-per-key | 16 | — | seal | 16384 | 1.963 | 1.950–1.972 |
| go | per-commit | extract-per-key | 256 | — | seal | 16384 | 1.916 | 1.887–1.941 |
| go | per-page | expand-per-key | — | — | seal | 16384 | 2.509 | 2.486–2.627 |
| go | per-commit | expand-per-key | 1 | — | seal | 16384 | 2.509 | 2.497–2.570 |
| go | per-commit | expand-per-key | 4 | — | seal | 16384 | 2.084 | 2.062–2.128 |
| go | per-commit | expand-per-key | 16 | — | seal | 16384 | 1.957 | 1.938–1.985 |
| go | per-commit | expand-per-key | 256 | — | seal | 16384 | 1.939 | 1.908–1.965 |
| go | fixed-key | — | — | — | open | 16384 | 1.742 | 1.734–1.805 |
| go | per-page | extract-per-key | — | — | open | 16384 | 2.603 | 2.567–2.710 |
| go | per-commit | extract-per-key | 1 | miss-per-commit | open | 16384 | 2.704 | 2.660–2.784 |
| go | per-commit | extract-per-key | 4 | miss-per-commit | open | 16384 | 2.014 | 2.010–2.211 |
| go | per-commit | extract-per-key | 16 | miss-per-commit | open | 16384 | 1.873 | 1.833–2.939 |
| go | per-commit | extract-per-key | 256 | miss-per-commit | open | 16384 | 1.786 | 1.780–1.824 |
| go | per-page | expand-per-key | — | — | open | 16384 | 2.363 | 2.338–2.733 |
| go | per-commit | expand-per-key | 1 | miss-per-commit | open | 16384 | 2.454 | 2.409–2.491 |
| go | per-commit | expand-per-key | 4 | miss-per-commit | open | 16384 | 1.939 | 1.901–1.972 |
| go | per-commit | expand-per-key | 16 | miss-per-commit | open | 16384 | 1.805 | 1.803–1.838 |
| go | per-commit | expand-per-key | 256 | miss-per-commit | open | 16384 | 1.754 | 1.743–1.802 |
| go | per-commit | — | 16 | all-hit | open | 16384 | 1.751 | 1.749–1.793 |
| rustcrypto | key-setup | — | — | — | salt-random32 | 0 | 0.044 | 0.044–0.044 |
| rustcrypto | key-setup | — | — | — | aes-gcm-key-setup | 0 | 0.047 | 0.047–0.047 |
| rustcrypto | key-setup | — | — | — | hkdf-extract-per-key | 0 | 0.219 | 0.218–0.220 |
| rustcrypto | key-setup | — | — | — | derive-extract-per-key | 0 | 0.337 | 0.337–0.340 |
| rustcrypto | key-setup | — | — | — | hkdf-expand-per-key | 0 | 0.080 | 0.080–0.080 |
| rustcrypto | key-setup | — | — | — | derive-expand-per-key | 0 | 0.180 | 0.179–0.181 |
| rustcrypto | fixed-key | — | — | — | seal | 4096 | 1.371 | 1.362–1.376 |
| rustcrypto | per-page | extract-per-key | — | — | seal | 4096 | 1.729 | 1.717–1.779 |
| rustcrypto | per-commit | extract-per-key | 1 | — | seal | 4096 | 1.728 | 1.724–1.751 |
| rustcrypto | per-commit | extract-per-key | 4 | — | seal | 4096 | 1.468 | 1.460–1.484 |
| rustcrypto | per-commit | extract-per-key | 16 | — | seal | 4096 | 1.401 | 1.388–1.407 |
| rustcrypto | per-commit | extract-per-key | 256 | — | seal | 4096 | 1.373 | 1.367–1.376 |
| rustcrypto | per-page | expand-per-key | — | — | seal | 4096 | 1.552 | 1.548–1.572 |
| rustcrypto | per-commit | expand-per-key | 1 | — | seal | 4096 | 1.562 | 1.558–1.580 |
| rustcrypto | per-commit | expand-per-key | 4 | — | seal | 4096 | 1.439 | 1.424–1.441 |
| rustcrypto | per-commit | expand-per-key | 16 | — | seal | 4096 | 1.396 | 1.383–1.399 |
| rustcrypto | per-commit | expand-per-key | 256 | — | seal | 4096 | 1.380 | 1.370–1.385 |
| rustcrypto | fixed-key | — | — | — | open | 4096 | 1.390 | 1.385–1.433 |
| rustcrypto | per-page | extract-per-key | — | — | open | 4096 | 1.674 | 1.662–1.694 |
| rustcrypto | per-commit | extract-per-key | 1 | miss-per-commit | open | 4096 | 1.752 | 1.739–1.800 |
| rustcrypto | per-commit | extract-per-key | 4 | miss-per-commit | open | 4096 | 1.501 | 1.493–1.537 |
| rustcrypto | per-commit | extract-per-key | 16 | miss-per-commit | open | 4096 | 1.449 | 1.435–1.468 |
| rustcrypto | per-commit | extract-per-key | 256 | miss-per-commit | open | 4096 | 1.415 | 1.411–1.423 |
| rustcrypto | per-page | expand-per-key | — | — | open | 4096 | 1.510 | 1.510–1.523 |
| rustcrypto | per-commit | expand-per-key | 1 | miss-per-commit | open | 4096 | 1.595 | 1.582–1.619 |
| rustcrypto | per-commit | expand-per-key | 4 | miss-per-commit | open | 4096 | 1.475 | 1.465–1.487 |
| rustcrypto | per-commit | expand-per-key | 16 | miss-per-commit | open | 4096 | 1.440 | 1.435–1.453 |
| rustcrypto | per-commit | expand-per-key | 256 | miss-per-commit | open | 4096 | 1.417 | 1.410–1.425 |
| rustcrypto | per-commit | — | 16 | all-hit | open | 4096 | 1.413 | 1.409–1.426 |
| rustcrypto | fixed-key | — | — | — | seal | 8192 | 2.683 | 2.676–2.705 |
| rustcrypto | per-page | extract-per-key | — | — | seal | 8192 | 3.081 | 3.021–3.087 |
| rustcrypto | per-commit | extract-per-key | 1 | — | seal | 8192 | 3.068 | 3.049–3.085 |
| rustcrypto | per-commit | extract-per-key | 4 | — | seal | 8192 | 2.817 | 2.764–2.830 |
| rustcrypto | per-commit | extract-per-key | 16 | — | seal | 8192 | 2.722 | 2.707–3.014 |
| rustcrypto | per-commit | extract-per-key | 256 | — | seal | 8192 | 2.738 | 2.737–2.744 |
| rustcrypto | per-page | expand-per-key | — | — | seal | 8192 | 2.938 | 2.933–3.401 |
| rustcrypto | per-commit | expand-per-key | 1 | — | seal | 8192 | 2.940 | 2.909–3.907 |
| rustcrypto | per-commit | expand-per-key | 4 | — | seal | 8192 | 2.768 | 2.738–2.782 |
| rustcrypto | per-commit | expand-per-key | 16 | — | seal | 8192 | 2.717 | 2.699–2.732 |
| rustcrypto | per-commit | expand-per-key | 256 | — | seal | 8192 | 2.691 | 2.668–2.712 |
| rustcrypto | fixed-key | — | — | — | open | 8192 | 2.720 | 2.709–2.744 |
| rustcrypto | per-page | extract-per-key | — | — | open | 8192 | 2.981 | 2.967–3.020 |
| rustcrypto | per-commit | extract-per-key | 1 | miss-per-commit | open | 8192 | 3.071 | 3.055–3.095 |
| rustcrypto | per-commit | extract-per-key | 4 | miss-per-commit | open | 8192 | 2.853 | 2.838–2.912 |
| rustcrypto | per-commit | extract-per-key | 16 | miss-per-commit | open | 8192 | 2.794 | 2.740–2.817 |
| rustcrypto | per-commit | extract-per-key | 256 | miss-per-commit | open | 8192 | 2.748 | 2.726–2.783 |
| rustcrypto | per-page | expand-per-key | — | — | open | 8192 | 2.814 | 2.809–2.837 |
| rustcrypto | per-commit | expand-per-key | 1 | miss-per-commit | open | 8192 | 2.916 | 2.905–2.921 |
| rustcrypto | per-commit | expand-per-key | 4 | miss-per-commit | open | 8192 | 2.794 | 2.784–2.842 |
| rustcrypto | per-commit | expand-per-key | 16 | miss-per-commit | open | 8192 | 2.748 | 2.744–2.778 |
| rustcrypto | per-commit | expand-per-key | 256 | miss-per-commit | open | 8192 | 2.777 | 2.755–2.798 |
| rustcrypto | per-commit | — | 16 | all-hit | open | 8192 | 2.778 | 2.748–2.785 |
| rustcrypto | fixed-key | — | — | — | seal | 16384 | 5.368 | 5.355–5.411 |
| rustcrypto | per-page | extract-per-key | — | — | seal | 16384 | 5.783 | 5.726–5.842 |
| rustcrypto | per-commit | extract-per-key | 1 | — | seal | 16384 | 5.729 | 5.665–5.799 |
| rustcrypto | per-commit | extract-per-key | 4 | — | seal | 16384 | 5.490 | 5.428–5.523 |
| rustcrypto | per-commit | extract-per-key | 16 | — | seal | 16384 | 5.325 | 5.292–5.454 |
| rustcrypto | per-commit | extract-per-key | 256 | — | seal | 16384 | 5.368 | 5.283–5.403 |
| rustcrypto | per-page | expand-per-key | — | — | seal | 16384 | 5.615 | 5.588–5.656 |
| rustcrypto | per-commit | expand-per-key | 1 | — | seal | 16384 | 5.537 | 5.495–5.629 |
| rustcrypto | per-commit | expand-per-key | 4 | — | seal | 16384 | 5.468 | 5.413–5.490 |
| rustcrypto | per-commit | expand-per-key | 16 | — | seal | 16384 | 5.323 | 5.294–5.474 |
| rustcrypto | per-commit | expand-per-key | 256 | — | seal | 16384 | 5.342 | 5.298–5.351 |
| rustcrypto | fixed-key | — | — | — | open | 16384 | 5.377 | 5.336–5.497 |
| rustcrypto | per-page | extract-per-key | — | — | open | 16384 | 5.645 | 5.571–5.698 |
| rustcrypto | per-commit | extract-per-key | 1 | miss-per-commit | open | 16384 | 5.797 | 5.747–5.896 |
| rustcrypto | per-commit | extract-per-key | 4 | miss-per-commit | open | 16384 | 5.508 | 5.469–5.670 |
| rustcrypto | per-commit | extract-per-key | 16 | miss-per-commit | open | 16384 | 5.411 | 5.382–5.479 |
| rustcrypto | per-commit | extract-per-key | 256 | miss-per-commit | open | 16384 | 5.385 | 5.380–5.529 |
| rustcrypto | per-page | expand-per-key | — | — | open | 16384 | 5.452 | 5.435–5.476 |
| rustcrypto | per-commit | expand-per-key | 1 | miss-per-commit | open | 16384 | 5.682 | 5.627–5.759 |
| rustcrypto | per-commit | expand-per-key | 4 | miss-per-commit | open | 16384 | 5.530 | 5.470–5.658 |
| rustcrypto | per-commit | expand-per-key | 16 | miss-per-commit | open | 16384 | 5.438 | 5.398–5.480 |
| rustcrypto | per-commit | expand-per-key | 256 | miss-per-commit | open | 16384 | 5.437 | 5.352–5.509 |
| rustcrypto | per-commit | — | 16 | all-hit | open | 16384 | 5.363 | 5.342–5.429 |

| Fixture | Layout | Bytes | SHA-256 of key or ciphertext+tag |
|---|---|---:|---|
| key | extract-per-key | 0 | `63d84c70a73e6b7de087c04fb2fe1700dcb35f1aff9efa09292eca677eb5bc6c` |
| key | expand-per-key | 0 | `9fd7e90e76d3a1336ed43b2d0b0509733f13963685f4304a2ca4c16331df0f3f` |
| per-page | extract-per-key | 4096 | `be2011c8c76a9cd7d3c92d15a90c9f93073e03f38b06b5ca62836ca5873b17be` |
| per-commit | extract-per-key | 4096 | `0b31d16a10659a2962ba421e4668530846e1eb09f7ec2d0e868082802c15485f` |
| per-page | expand-per-key | 4096 | `04d08d4f5e406a52b8e0b5fac30ca42163013c7e6a15625fd3ffe0f1bcb29ab3` |
| per-commit | expand-per-key | 4096 | `9bcae06c9de27e94fcf57799b83240dd52ddee82f9495dcc816a6af71cbe6a35` |
| per-page | extract-per-key | 8192 | `fb748ae8c6aee6253ee0deead68a1d968c1e056d226391bd43fe908fa0a8db17` |
| per-commit | extract-per-key | 8192 | `edfa7a109abe926f2ecc0c733334729838e1a6bf2ef8c1ca4a3daba434d70717` |
| per-page | expand-per-key | 8192 | `1fba93731c8afb7117136ef499ab91e2cfaf4d80bfe1f1500ca981db11a5793d` |
| per-commit | expand-per-key | 8192 | `854377cade209aa7cda7ea1926ad6da62142109447c4bf6ecbded7848263237c` |
| per-page | extract-per-key | 16384 | `ffced0c9df1ccfeeb9ac9d3e4b50cc0052496ac3350ecf50b80bbd6dfff019e1` |
| per-commit | extract-per-key | 16384 | `ec25be94c3bf0e8f36f9cd50587789d18b09c90a3cedc3954cf253a4ba13642f` |
| per-page | expand-per-key | 16384 | `7f551630ed3677f0659c8b74994e9b28f6cfc46c99f5a42280e16741874a7505` |
| per-commit | expand-per-key | 16384 | `842934e363510bdbdf34face66a172e62ab6b4dc3e9b68f34d7fb7cff5fb8dac` |

- `bench/aead/go/cmd/granularity/main.go`: `a702d6bf1a2527dc16d8425f3b1be64dc365e89c0ff79e2aa5b2f97c2e8da668`
- `bench/aead/go/go.mod`: `03adcbae8ca9b5639e7aba8900bd45745f01a6559e198f6ba336c8f952f36e33`
- `bench/aead/go/go.sum`: `29025c5d36db542e961df27da21810c6901df208558510cbc2793a51b1901450`
- `bench/aead/rust/Cargo.lock`: `ea54edd49b03aa2d0e4aff13043316b7504dde57551f704a03cc16fec8621b12`
- `bench/aead/rust/Cargo.toml`: `7df0d27d2b55c1de2d64bc7cd2ff4dbe30cd3e2a2694b1ed0844f18fc943b0a2`
- `bench/aead/rust/src/bin/granularity.rs`: `9062639465dcc603ebd8a0072e70011236574885adc34225d39f65e7c3b50405`
