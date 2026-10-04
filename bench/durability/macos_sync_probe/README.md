# macOS durability syscall probe

This standalone, standard-library-only Go experiment measures the macOS flush
choices without changing any engine or database format. The implementation and
protocol tests were built on Linux; **no macOS performance or power-loss result
has been measured here**. Cross-compilation establishes buildability only.

## Findings

The current jed paths are already stronger than ordinary macOS `fsync`:

- Rust's `File::sync_data()` and `sync_all()` both invoke `F_FULLFSYNC` on
  Apple targets ([Rust standard-library implementation](https://github.com/rust-lang/rust/blob/master/library/std/src/sys/fs/unix.rs)).
- Go's `File.Sync()` invokes `F_FULLFSYNC`; the standard library falls back to
  ordinary `fsync` on `ENOTSUP`, such as some network mounts
  ([Go implementation](https://go.dev/src/internal/poll/fd_fsync_darwin.go)).
- Node/libuv's macOS `fdatasync` path also invokes its full-sync implementation,
  with weaker fallbacks if unavailable
  ([libuv implementation](https://github.com/libuv/libuv/blob/v1.x/src/unix/fs.c)).

`fdatasync` **does exist in Darwin**. XNU dispatches syscall 187 to
`fsync_common(..., MNT_DWAIT)`, requesting data-integrity completion rather than
all timestamp metadata. This is present in both current source and the older
`xnu-7195.81.3` release. A missing high-level wrapper is not evidence that the
kernel lacks the operation. These sources alone do not establish APFS speed or
drive-cache durability equivalent to `F_FULLFSYNC`.
([XNU syscall table](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/syscalls.master),
[current VFS implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_syscalls.c),
[older VFS implementation](https://github.com/apple-oss-distributions/xnu/blob/xnu-7195.81.3/bsd/vfs/vfs_syscalls.c))

The useful distinction is **ordering versus durable completion**, as well as
data versus metadata. Apple explains that `fsync` moves OS-cached writes to the
device, without guaranteeing their physical persistence or ordering. The drive's
volatile write cache remains relevant on SSDs. `F_FULLFSYNC` additionally drains
that cache; this can affect other workloads on the same device. Merely replacing
both jed barriers with `fdatasync` or ordinary `fsync` cannot preserve its stated
durability contract.
([Apple's storage presentation, sync section](https://developer.apple.com/videos/play/wwdc2019/419/),
[Apple's fsync manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/fsync.2))

`F_BARRIERFSYNC` provides a promising smaller change. Apple documents that it
flushes the file and orders earlier I/O before later I/O; it does **not** promise
persistence when the call returns. Hardware support is required; Apple SSDs
provide it. The proposed sequence is:

1. Write fresh COW body pages.
2. `F_BARRIERFSYNC` (or full sync if unsupported).
3. Publish the alternate meta slot.
4. `F_FULLFSYNC` before acknowledging the commit.

Inference from that contract: this preserves body-before-root crash ordering and
durable completion while reducing two full drive-cache drains to one. It retains
two synchronization calls, requires no WAL or format change, and offers no claimed
speedup until measured on macOS. Do not replace the final full sync with another
barrier: a barrier alone allows acknowledged writes to disappear after power loss.
The current five-method `BlockStore` promises durability from every `sync()`;
production integration needs a distinct ordering operation or an explicit
commit-phase interface, rather than silently weakening `sync()`.
([Apple fcntl contract](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/fcntl.2))

## Running on a Mac

From this directory, select a directory on the actual filesystem of interest:

```sh
go run . --dir /path/on/local/apfs --iterations 1000 --warmup 100 \
  --mode fullsync-fullsync,barrier-fullsync
go run . --dir /path/on/local/apfs --iterations 1000 --warmup 100 \
  --body-pages 16 --mode barrier-fullsync,fullsync-fullsync
```

Repeat both mode orders and compare medians and tails. Record macOS version,
filesystem, device model, whether storage is external, and concurrent I/O. A
nonzero `barrier_fallbacks` count means the candidate actually used two full
flushes; do not attribute that timing to successful barriers. `--mode all` adds
explicitly unsafe controls. `--help` describes each mode:

| Mode | Sequence | What it establishes |
| --- | --- | --- |
| `fullsync-fullsync` | body, full, meta, full | Existing two-phase durability baseline |
| `barrier-fullsync` | body, barrier, meta, full | Proposed ordered publication plus durable completion |
| `fullsync` | body, meta, full | One-drain timing only; current COW recovery is unsafe before completion |
| `fsync` | body, meta, fsync | Unsafe timing control: no guaranteed drive-cache durability or ordering |
| `fdatasync` | body, meta, fdatasync | Unsafe timing control: no guaranteed drive-cache durability or ordering |

Every iteration writes identical amounts of data for the selected body-page count:
body pages plus a 4096-byte meta page. The probe exclusively creates a temporary
file, initializes eight MiB with real zero writes, and full-syncs allocation before
timing. Writes alternate preallocated regions; file size stays fixed. Content is
seeded and changes each iteration. Warmup, allocation, final content verification,
and cleanup are excluded from timing. JSONL includes end-to-end mean/p50/p95/p99,
call counts, per-primitive total time, bytes, seed, platform, and fallback count.

The fresh-descriptor read check verifies bytes, **not** recovery after lost OS or
drive cache. This probe neither implements jed recovery nor proves storage-hardware
behavior. Temporary-file namespace durability is outside the measurement; no user
database is opened and no directory fsync is included.

Darwin calls use Go's existing pointer-free `syscall.Syscall` entry points with
`CGO_ENABLED=0`, including Apple's `F_BARRIERFSYNC = 85`. There is no `unsafe`, FFI,
new dependency, or misuse of the flock wrapper. This is an experiment using the
raw syscall ABI; before production adoption, validate it on supported Darwin
versions and assess Apple's supported API boundary. Rust std exposes no separate
ordering barrier; adding a custom Rust system-call wrapper would need to resolve
the repository's no-unsafe/dependency constraints.

An unsupported barrier (`ENOTSUP`, `EINVAL`, `ENOSYS`) falls back to
`F_FULLFSYNC`. Other errors abort before publishing meta. The final full sync
never falls back to a weaker primitive, even though some language runtimes do.

## Linux verification

```sh
go test ./...
go vet ./...
GOOS=darwin GOARCH=arm64 CGO_ENABLED=0 go build -o /tmp/jed-macos-sync-probe-arm64 .
GOOS=darwin GOARCH=amd64 CGO_ENABLED=0 go build -o /tmp/jed-macos-sync-probe-amd64 .
```

Tests exercise operation ordering, unsupported-barrier fallback, I/O failure
before root publication, refusal to weaken the final barrier, and rejection of a
non-Darwin runtime before touching the requested directory. They validate probe
control flow, not macOS kernel behavior.
