# spec/pageservice/ — the page-service wire contract

The byte contract of the page service ([../design/page-service.md](../design/page-service.md)): the
framing, the message catalog, the field encodings, the owner record, and the endpoint layout, with
byte fixtures. Like the file format ([../fileformat/](../fileformat/README.md)), every core
**hand-writes** its codec and is tested against the shared fixtures; nothing here is generated.

| File | Contents |
|---|---|
| [messages.toml](messages.toml) | The message catalog as data: every message's code, direction, reply, phase, and field list. |
| [fixtures.toml](fixtures.toml) | Byte fixtures: declarative field values → the exact frame bytes, plus malformed frames every decoder must reject. |
| [verify.rb](verify.rb) | Independent Ruby reference codec. It checks the catalog's shape, encodes every fixture from its fields and compares the bytes, decodes the bytes back, and checks that each malformed frame is rejected. `--generate` rewrites the fixture bytes. Run via `mise run verify`. |

[locking.md](../design/locking.md) remains the binding multi-process protocol until page-service.md
phase 4 lands. Until then a core speaks this protocol only under the test-only `locking = service`
value.

## 1. Conventions

All integers are **big-endian** and unsigned, as in the file format. The field types are:

| Type | Encoding |
|---|---|
| `u8`, `u16`, `u32`, `u64` | 1, 2, 4, 8 bytes, big-endian |
| `bool` | one byte, `0` or `1`; any other value is malformed |
| `id16` | 16 raw bytes (an incarnation id) |
| `sqlstate` | 5 bytes, each an ASCII digit or upper-case letter |
| `string` | `u32` byte length, then that many bytes of valid UTF-8 |
| `page` | exactly `page_size` bytes, where `page_size` is the connection's (from `hello_ok`) |
| `u32_list` | `u32` count, then `count` × `u32` |
| `page_list` | `u32` count, then `count` × `page` |
| `indexed_page_list` | `u32` count, then `count` × (`u32` page index, `page`) |

A decoder rejects a body that ends before its last field or that has bytes after it. A `flags`
field's reserved bits are written as 0 and ignored on receipt, so a later protocol version can give
them a meaning without breaking an older receiver. Counts are
checked against the remaining body before anything is allocated, so a forged count cannot make a
decoder allocate more than the frame holds.

## 2. Framing

Both directions use one frame shape:

```text
frame := length u32 | code u8 | request_id u32 | body
```

- `length` counts the bytes after itself: `5 + len(body)`. It must be at least 5 and at most
  `max_frame_length` = **16 777 216** (16 MiB). A larger or smaller `length` is malformed, and the
  receiver rejects it **before** reading the body.
- `code` names the message (§3). An unknown code is malformed.
- `request_id`: the client numbers its requests 1, 2, 3, … per connection, wrapping from
  `0xFFFFFFFF` to 1. A reply or error carries the `request_id` of the request it answers. `0` is
  reserved for unsolicited notices, which only the owner sends.

Replies arrive in request order. A client may pipeline: it may send further requests before earlier
replies arrive. Messages marked `reply = "none"` in the catalog get no reply on success.

## 3. Messages

[messages.toml](messages.toml) is the catalog. Request codes are `0x01`–`0x3F`, reply codes
`0x40`–`0x7D`, and `0x7E`/`0x7F` are `notice` and `error`. A message's `phase` is the
page-service.md §16 phase that first uses it. **Messages of phases not yet landed are provisional:
the phase that lands them may revise their bodies.** The protocol version stays **1** until the first
release that ships the page service; after that, any change to a landed message needs a new version.

### 3.1 `hello` → `hello_ok` (phase 2)

The first frame on a connection, and only the first, is `hello`; any other first frame, or a second
`hello`, is a protocol violation.

- `hello`: `protocol_min u16`, `protocol_max u16` (the versions the client speaks),
  `format_version u16` (the one on-disk format the client reads and writes), `flags u8` (bit 0:
  the client asks for a read-only connection; bits 1–7 are 0), `client_name string` (diagnostic,
  e.g. `jed-go`; never parsed).
- The owner picks the highest version both sides speak. With none, or when `format_version` differs
  from the file's, it replies `error` `0A000` (fatal) with the supported versions in the detail.
- `hello_ok`: `protocol u16`, `format_version u16`, `page_size u32`, `connection_id u64` (unique
  among this owner's connections; never 0), `flags u8` (bit 0: the connection is read-only; bit 1:
  the owner's own handle is read-only; bits 2–7 are 0), `max_read_pages u32`, the most indices one
  `read_pages` may name: `min(1024, floor((max_frame_length − 9) / page_size))`.

`page` fields in either direction are decoded with this connection's `page_size`. A client whose
engine does not support the page size (page sizes are a file-format fact every core supports)
closes the connection.

**Phase 2 owners make every connection read-only** (the write path is phase 3).

### 3.2 `begin_read` → `read_begun` (phase 2)

- `begin_read`: `incarnation id16` (the owner incarnation the client's cache came from; all zero when
  it has no cache), `last_txid u64` (the newest `txid` the client adopted from that incarnation; 0
  with no cache).
- The owner pins its current committed snapshot for this connection (pins are counted: the same
  `txid` may be pinned more than once) and replies.
- `read_begun`: `incarnation id16` (the owner's), `txid u64` (the pinned snapshot), `invalidate u8`
  (`0`: the list that follows is exact; `1`: invalidate everything), `indices u32_list` (when
  `invalidate = 0`: every page index written by a commit with `last_txid < txid' ≤ txid`, ascending,
  without duplicates; empty when `invalidate = 1`), `meta page` (the exact meta-slot bytes that
  published `txid`).

The owner replies `invalidate = 1` when the incarnation differs from its own, when `last_txid` is 0,
greater than `txid`, or older than its retained window (§4), or when the exact list would exceed
65 536 entries. The client drops the listed indices (or its whole page cache), and when `txid`
differs from its current snapshot, reloads its catalog/interior skeleton from `meta` and invalidates
its plan caches (page-service.md §5.3–§5.4).

### 3.3 `read_pages` → `pages` (phase 2)

- `read_pages`: `txid u64`, `indices u32_list`.
- `txid` must be pinned by this connection, `1 ≤ count ≤ max_read_pages`, and every index must be at
  least 2 (meta slots come only from `read_begun`) and below the pinned snapshot's `page_count`.
  Otherwise the request is a protocol violation.
- `pages`: `pages page_list`, the current bytes at each requested index, in request order. They are
  the file's stored bytes (ciphertext under encryption — page-service.md §5.1); the client checks each
  page's CRC-32 when it decodes it, as for a local read.

### 3.4 `end_read` (phase 2, no reply)

`end_read`: `txid u64`. Releases one pin. Releasing a `txid` this connection has not pinned is a
protocol violation. Closing the connection (EOF) releases every pin it holds.

### 3.5 Write path, ownership, backup, replication (phases 3–7, provisional)

| Message | Body | Reply |
|---|---|---|
| `begin_write` | `incarnation id16`, `last_txid u64`, `lock_timeout_ms u64` (the client session's; 0 = no deadline) | `write_begun`: the `read_begun` fields, then `reuse_grant bool` before `meta` |
| `commit_begin` | `txid u64` (the new one), `meta page`, `manifest_pages indexed_page_list` | none; failures are reported at `commit_end` |
| `commit_pages` | `pages indexed_page_list` | none |
| `commit_end` | empty | `committed`: `txid u64` |
| `abort` | empty | none |
| `commit_status` | `txid u64`, `digest u64` | `commit_status_reply`: `status u8` (0 committed, 1 not committed, 2 unknown) |
| `request_ownership` | empty | `ownership_reply`: `status u8` (0 the owner is yielding, 1 refused) |
| `snapshot_stream` | reserved (phase 6) | — |
| `subscribe` | reserved (phase 7) | — |

A commit's `digest` is CRC-64/ECMA-182 (the validated-COW manifest CRC, initial value 0) of the
complete meta page the commit published.

### 3.6 `notice` and `error`

- `notice` (owner → client, `request_id` 0): `kind u8`, `connection_id u64`. Kinds: `1` yield begun,
  naming the requester's connection; `2` yield cancelled; `3` shutdown begun. Unused
  `connection_id` is 0. Phase 4.
- `error`: `sqlstate sqlstate`, `message string`, `detail string`, `flags u8` (bit 0: fatal — the
  owner closes the connection after this frame; bits 1–7 are 0). It answers the request whose
  `request_id` it carries. A protocol violation is answered with a fatal `08P01` carrying the
  offending `request_id` (0 when the frame could not be decoded that far); the owner then closes the
  connection, which releases its pins.

## 4. Owner state

- **Incarnation id.** 16 random bytes an owner draws when it becomes owner, redrawn if all zero.
  It never changes during that ownership.
- **Invalidation window.** The owner retains the written-index sets of its most recent **256**
  commits (page-service.md §5.4): every index a commit's manifest lists (dirty body, free-list, and
  manifest-overflow pages), the meta slots excluded. The window begins at the snapshot the owner
  recovered at open; a compaction resets it to the compacted snapshot, so every older `last_txid`
  receives `invalidate = 1`.
- **Pins.** Each connection's pins are counted per `txid` and are part of the owner's reader
  watermark (transactions.md §8), exactly like a local reader's.

## 5. Bundle entries and the endpoint

The coordination bundle ([locking.md](../design/locking.md) §3.1) gains two entries. Neither holds
database bytes, and neither is a lock.

### 5.1 The owner record: `<path>.lock/owner`

Written by every owner after it acquires `presence EX` and before it binds the endpoint (or, when not
serving, before it finishes opening). Written to `<path>.lock/owner.tmp-<incarnation hex>` and
renamed into place, so a reader never sees a torn record.

```text
owner_record := magic "JEDOWNER" | record_version u16 = 1 | flags u8 | uid u32 | incarnation id16
                | pid u32 | endpoint string | crc u32
```

- `flags` bit 0: the owner serves; bits 1–7 are 0, and a record with any of them set fails to parse.
- `uid`: the owner's numeric user id. A client running as another user does not connect and words its
  eventual `55006` as "owned by another user's process" (page-service.md §13).
- `pid`: diagnostic only; never used for liveness (locking.md §2 invariant 6).
- `endpoint`: the absolute socket path; empty when not serving.
- `crc`: CRC-32/IEEE of every preceding byte. A record that fails to parse is treated as absent.

A stale record from a dead owner can only mislabel an open-timeout error, never change an outcome:
`presence` is the only authority, and a client that finds no live endpoint keeps retrying until
`file_lock_timeout_ms`.

### 5.2 The endpoint

A stream socket (`AF_UNIX`), never in Linux's abstract namespace.

1. **Bundle.** `<path>.lock/uid-<uid>/endpoint`, when that path is at most **100 bytes**. The owner
   creates `uid-<uid>` with mode `0700` if absent and serves from it only if it is a real directory
   (not a symlink) owned by the owner's uid with mode exactly `0700`.
2. **Runtime directory** (the path would be longer than 100 bytes, or the bundle directory failed the
   check above): `<dir>/jed-<first 8 bytes of the incarnation, 16 hex>.sock`, where `<dir>` is the
   first that exists and passes the same ownership and `0700` check: `$XDG_RUNTIME_DIR` (Linux),
   `$TMPDIR` (macOS, already per-user), then `/tmp/jed-<uid>`, which the owner creates with `0700`.
   If the result is still longer than 100 bytes, the owner does not serve and records `flags = 0`.

Only a `presence EX` holder may unlink a stale endpoint at its own path before binding. An owner
removes its endpoint when it closes. Directory search permission is the access control: the kernel
checks it on `connect`, so only processes of the owner's user can connect (page-service.md §13).

Windows serving is not yet specified (page-service.md §14): `locking = service` fails `0A000` there.
