# spec/encoding/ — order-preserving key encoding + byte vectors

Keys are stored sorted and iterated in **raw byte order**, so the encoding of a value must
sort, byte-for-byte (`memcmp`), identically to the value's logical order — across *every*
implementation (CLAUDE.md §8). Shared `(value → expected bytes)` test vectors make this
verifiable, not hoped-for.

## The rule (integers)

Fixed-width **big-endian**, with the **sign bit inverted** for signed types. Big-endian is
forced by byte-wise comparison: lexicographic comparison reads the most-significant byte
first, so the MSB must be stored first. The sign-bit flip maps the two's-complement signed
range monotonically onto the unsigned range so negatives sort below positives. Descending
order is bitwise inversion of a component; composite keys are concatenation. (CockroachDB's
`encoding` package is the reference design.)

The per-type encoding rule is recorded as a field on each type in
[../types/scalars.toml](../types/scalars.toml). **The reasoning — bare encoding, the
nullable presence tag, composition, and the NULLs-last decision — is in
[../design/encoding.md](../design/encoding.md). Read that first.**

## Files

| File | Contents |
|---|---|
| [integers.toml](integers.toml) | Byte-exact `(value → bytes)` **key-encoding** vectors: `i16`/`i32`/`i64` bare values, the nullable presence-tag slot, and the descending (inverted) encoding. |
| [timestamps.toml](timestamps.toml) | `timestamp`/`timestamptz` parse / render vectors — `(input → micros)`, `(input → error)`, `(micros → text)` ([../design/timestamp.md](../design/timestamp.md)). |
| [intervals.toml](intervals.toml) | `interval` parse / render vectors — `(input → months/days/micros)` and `(fields → text)` ([../design/interval.md](../design/interval.md)). |
| [prng.toml](prng.toml) | splitmix64 PRNG stream + v4/v7 UUID byte-layout fixtures for the entropy seam ([../design/entropy.md](../design/entropy.md)). |
| [verify.rb](verify.rb) | Independent reference encoder that checks every key-encoding vector for round-trip, byte-exactness, and order preservation. Run `rake verify` (or `bundle exec ruby spec/encoding/verify.rb`); test-time only. |
| [prng_verify.rb](prng_verify.rb) | Independent Ruby reference that recomputes the splitmix64 + UUID fixtures and asserts they match (`rake verify`); test-time only. |

## NULL ordering (ratified here)

A nullable key slot carries a 1-byte presence tag (`0x00` present, `0x01` NULL), so **NULLs
sort last** in ascending order (descending inverts → NULLs first). This is the PostgreSQL
model (NULL is the largest value), ratifying the NULL sort-position decision that
[../types/compare.toml](../types/compare.toml) deferred to this step
(`null_ordering = "nulls-last-ascending"` — see [../design/encoding.md §4](../design/encoding.md)).

> Status: rule defined and fixtures authored + verified. Non-integer keys now exercised:
> `uuid` (method `uuid-raw16`: fixed 16 raw bytes, no sign-flip/escape/terminator;
> [../design/encoding.md §2.7](../design/encoding.md)), `boolean` (method `bool-byte`: a single
> byte 0x00 false / 0x01 true, no sign-flip/escape/terminator;
> [../design/encoding.md §2.9](../design/encoding.md)), and `timestamp`/`timestamptz` (key
> encoding = the i64 rule) — all usable as a `PRIMARY KEY`. The remaining non-integer key
> vectors (decimal/text/bytea/float/interval) and composite keys follow when those features
> exercise keys. The directory has also grown beyond pure key encoding to hold cross-core
> parse/render byte vectors (`timestamps.toml`, `intervals.toml`) and the entropy-seam
> PRNG/UUID fixtures (`prng.toml`).
