# Parity update (2026-10-27)

The maintainer explicitly requested this update to keep the experiment alive.
It brings the pure Ruby core level with the supported cores at source revision
`1c5a8961` (Add jed check, the integrity check as a CLI verb). It does not change
the core's experimental or frozen status. Later features again do not update it
unless requested. Historical reports and benchmark results are unchanged; this
update records no new performance measurements.

The previous surface tracked `d4a01ebd`. The supported-core work since then that
this update ports:

| Area | Supported-core commits | Ruby work |
|---|---|---|
| `MERGE`, joined sources, view targets, EXPLAIN, hash form | `0c604e64`, `2023ed2e` | `merge.rb` |
| Per-row key metering for sorts, grouping, dedup | `586c50a3`, `684effad` | `key_metering.rb` and every sort/hash site |
| Cost-ordered filter conjuncts | `71340f7f` | `compile_filter` sorts; constraint, index, arbiter, check-option and MERGE WHEN predicates keep their written order |
| `SELECT DISTINCT ON` | `bb012dbd` | base `select`, EXPLAIN `Distinct on=N` |
| `SIMILAR TO`, `similar_to_escape`, SIMILAR substring | `24aeebe4` | `similar.rb`, regex VM |
| Cheapest implicit argument coercions | `f41da319` | `arg_coercion.rb`, generated `ARG_COERCIONS` |
| Format v50 UNIQUE constraint ownership; DROP INDEX / DROP SEQUENCE refusals; ALTER SEQUENCE RENAME rewrites defaults | `c28c6995`, `69089517` | file format, DDL |
| Integrity-check host API | `b00efd19`, `9ad44b08`, `a9e01aa2`, `524e4597`, `0c26dfcb` | `Database#integrity_check`, `integrity.rb` |
| Time-based flush for `synchronous = off`; attachments close in name order | `61c7a236`, `0b1869c8` | idle flusher, environment clock |
| Page-service wire follow-ups | `a70af0f2`, `f5fe6154` | hello credential, begin_write flags, write_begun pending/reuse |
| Encryption at rest, credential change, rekey, sealed scratch, encrypted backups and service | `d6bbf570`, `b05e5185` | `crypto.rb`, `encryption_operations.rb` |

`aacad872` (deferred SQL-language triggers and functions) is documentation only.
`25bee30a` (window sort sharing) needed no Ruby change beyond key metering.
Simulator-runner fixes (`1e81d77c`, `1b05dbe6`, `081df0cc`, `678656d5`,
`b43fbd31`) do not apply: the Ruby core has no simulator. Cancellation changes
(`666575e8`, `d4c8ef54`) do not apply: it has no cancellation. TypeScript-only
runtime, OPFS and benchmark work does not apply.

## Dependencies

Encryption extends the approved Ruby utility dependencies:
- **OpenSSL:** AES-256, AES-256-GCM and HKDF-SHA-256, with `SecureRandom` for key
  material. Only XAES-256-GCM's derivation glue is handwritten, and it is checked
  against the C2SP vectors.
- **`argon2-kdf` 1.0.0:** Argon2id for passphrase credentials. The gem is a Fiddle
  binding to vendored prebuilt libargon2 libraries, not a compiled extension. It is
  required lazily, only when a passphrase is used; without it a passphrase fails
  with `0A000`. Plaintext and host-key databases need only the standard library.
  It reaches the gem's raw `argon2id_hash_raw` entry point, because the
  convenience method accepts only power-of-two memory costs.

## Verification

All counts are from `1c5a8961` plus this update, interpreter Ruby 4.0.7:

| Audit | Result |
|---|---:|
| Memory SQL/concurrency corpus | 20,844 / 20,844 |
| Disk corpus, plaintext (`JED_NATIVE_FILE_TIMEOUT=300`) | 18,827 / 18,827 |
| Disk corpus, encrypted (`JED_NATIVE_ENCRYPTED=1`) | 18,792 / 18,792 |
| Unit test files, each run separately | 73 / 73 |
| Ruby/Go/Rust process pairings (`mise run ruby-native:process`) | 124 / 124 |

Before this update the memory corpus had 413 failures out of 20,637 records.
Other checks:
- All `encryption_vectors.toml` vectors pass.
- The six `encrypted_*` goldens are byte-exact, including a real `rekey`.
- The 50 shared integrity fixtures reproduce their reports exactly.
- The encrypted run's cost transcript agrees with the Go core's on every line.
- Under the default 10-second deadline, `dml/index_spilled_key`, `expr/to_timestamp`
  and `rqg/join` can time out on a loaded machine. Each passes with a longer deadline.

## Omissions and divergences

- **Page-service stream client (`b65dba76`):** no stream client and no `jed pages`
  server. The process scenarios run Ruby as the owner; their stream-client steps run
  on another core.
- **Relaxed page clients:** a relaxed page client commits synchronously through the
  service. The client cumulative manifests and the owner-side relaxed batch from
  `a70af0f2` are not ported. The bytes stay compatible.
- **Integrity check memory:** it holds a table's decoded rows and an index's
  entries in memory, which the bounded memory of the design (§5.4) does not allow.
  Two further gaps:
  - it has no `catalog_mismatch` re-encode check;
  - indexes with pinned time-zone data are skipped as `missing_loaded_data`.
- **Rekey memory:** a rekey rebuilds trees in memory, not in a sealed, spillable
  scratch domain. Memory is unbounded for large databases; the bytes are identical.
- **Unparsable defaults:** ALTER SEQUENCE RENAME does not rewrite a stored
  default this binary cannot parse. The drop check still sees it.
- **Replication's key-block item:** not built, as in every core.
