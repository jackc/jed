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
[../types/scalars.toml](../types/scalars.toml); the reasoning is in
[../design/types.md](../design/types.md).

> Status: rule defined; the `(value → bytes)` fixtures are produced at CLAUDE.md §11
> step 4.
