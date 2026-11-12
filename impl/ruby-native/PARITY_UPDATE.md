# Parity update (2026-10-27)

The maintainer explicitly requested this update to keep the experiment alive.
It brings the pure Ruby core level with the supported cores at source revision
`56f49192` (Add jed check, the integrity check as a CLI verb). It does not change
the core's experimental or frozen status. Later features again do not update it
unless requested. Historical reports and benchmark results are unchanged; this
update records no new performance measurements.

The previous surface tracked `d980b536`. The supported-core work since then that
this update ports:

| Area | Supported-core commits | Ruby work |
|---|---|---|
| `MERGE`, joined sources, view targets, EXPLAIN, hash form | `80687977`, `16d6e14d` | `merge.rb` |
| Per-row key metering for sorts, grouping, dedup | `8e678ec0`, `c40f1255` | `key_metering.rb` and every sort/hash site |
| Cost-ordered filter conjuncts | `42b93e5e` | `compile_filter` sorts; constraint, index, arbiter, check-option and MERGE WHEN predicates keep their written order |
| `SELECT DISTINCT ON` | `b6c1aa55` | base `select`, EXPLAIN `Distinct on=N` |
| `SIMILAR TO`, `similar_to_escape`, SIMILAR substring | `3eb6af94` | `similar.rb`, regex VM |
| Cheapest implicit argument coercions | `d4dbd5fd` | `arg_coercion.rb`, generated `ARG_COERCIONS` |
| Format v50 UNIQUE constraint ownership; DROP INDEX / DROP SEQUENCE refusals; ALTER SEQUENCE RENAME rewrites defaults | `d3519b82`, `69089517` | file format, DDL |
| Integrity-check host API | `929a2623`, `6ddba9d5`, `f1fd2555`, `e63bc849`, `56fa800c` | `Database#integrity_check`, `integrity.rb` |
| Time-based flush for `synchronous = off`; attachments close in name order | `6c5a9683`, `218015fa` | idle flusher, environment clock |
| Page-service wire follow-ups | `55c96483`, `fb8cc97e` | hello credential, begin_write flags, write_begun pending/reuse |
| Encryption at rest, credential change, rekey, sealed scratch, encrypted backups and service | `783f7eee`, `b897413a` | `crypto.rb`, `encryption_operations.rb` |

`628cdf0f` (deferred SQL-language triggers and functions) is documentation only.
`6ad1616c` (window sort sharing) needed no Ruby change beyond key metering.
Simulator-runner fixes (`1bc20cf6`, `f16e2dca`, `fbc8bc82`, `1c485270`,
`b3172b26`) do not apply: the Ruby core has no simulator. Cancellation changes
(`f47757ca`, `154303c9`) do not apply: it has no cancellation. TypeScript-only
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

All counts are from `56f49192` plus this update, interpreter Ruby 4.0.7:

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

- **Page-service stream client (`bd5a84f3`):** no stream client and no `jed pages`
  server. The process scenarios run Ruby as the owner; their stream-client steps run
  on another core.
- **Relaxed page clients:** a relaxed page client commits synchronously through the
  service. The client cumulative manifests and the owner-side relaxed batch from
  `55c96483` are not ported. The bytes stay compatible.
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
