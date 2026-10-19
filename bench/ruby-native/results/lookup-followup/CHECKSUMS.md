The `checksum-probe-interpreter.json` and `checksum-probe-yjit.json` artifacts
are diagnostic kernel measurements against the original optimized library at
commit `c3ba8e69`, with library SHA-256
`05b5aa0e67adf3fd2c19f99059397772d141ed0f0d3a6094eefde9702c5adbfb`.
They compare its handwritten CRC32/CRC64 with standard-library Zlib CRC32 and
compare concatenated versus streamed page/meta spans. They measure neither SQL
execution nor end-to-end storage performance. Both isolated workers had a
512 MiB sampled RSS limit; observed peaks were 61,788,160 bytes without a JIT and
60,760,064 bytes with YJIT. The interpreter address-space limit was 1 GiB; the
YJIT limit was 2 GiB. Both had a 60-second deadline.

Each operation received 1,000 warmup calls followed by seven batches. The
original Ruby kernels used 1,000 iterations per batch; Zlib cases used 10,000.
The raw artifacts retain the original probe's `ruby_crc32_8k` and
`ruby_crc64_8k` labels. The reusable driver in `../../checksum_probe.rb` calls
these `source_crc32_8k` and `source_crc64_8k` so a future run cannot mistake an
updated source's implementation for the original handwritten kernel. Set
`RUBY_NATIVE_SOURCE` to the intended immutable source directory; the driver
records its hash and checks that it remains unchanged during measurement.

`checksum-tests.json` records the bounded validation after replacing CRC32
with Zlib: the independent checksum tests, shared file-format images and exact
encodes, storage recovery tests, and page-service wire/live-host tests. It is a
separate validation phase and records the source hashes before and after that
run. The reusable probe driver is retained in the repository at the path above
for future reruns.

The [Ruby Zlib API](https://docs.ruby-lang.org/en/4.0/Zlib.html#method-c-crc32)
provides incremental CRC32 through its optional seed argument; the shared
[file-format contract](../../../../spec/fileformat/format.md) specifies this
same standard zlib CRC32, including continuation across disjoint page spans.
