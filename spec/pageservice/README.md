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

Replies arrive in request order, with one exception: a `begin_write` waits for the writer gate, and
the owner keeps answering the connection's later requests meanwhile, so `write_begun` (or the wait's
error) arrives whenever the wait ends. A client matches every reply to its request by `request_id`.
A client may pipeline: it may send further requests before earlier replies arrive. Messages marked
`reply = "none"` in the catalog get no reply on success.

## 3. Messages

[messages.toml](messages.toml) is the catalog. Request codes are `0x01`–`0x3F`, reply codes
`0x40`–`0x7D`, and `0x7E`/`0x7F` are `notice` and `error`. A message's `phase` is the
page-service.md §16 phase that first uses it. Phases 2 and 3 have landed. **Messages of phases not
yet landed are provisional: the phase that lands them may revise their bodies.** The protocol version stays **1** until the first
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

A connection is read-only exactly when the client asked for it (bit 0 of `hello`'s `flags`; a client
whose own handle is read-only asks). An owner whose own handle is read-only reports it in bit 1 and
answers `begin_write` with `25006` (phase 4 hands ownership to the writer instead, page-service.md
§8.2).

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

### 3.5 Write path (phase 3)

A write transaction holds the owner's writer gate from `write_begun` until its commit is published or
it ends. Its messages:

| Message | Body | Reply |
|---|---|---|
| `begin_write` | `incarnation id16`, `last_txid u64` (as `begin_read`), `lock_timeout_ms u64` (the client session's; 0 = no deadline) | `write_begun`: `incarnation id16`, `txid u64`, `invalidate u8`, `indices u32_list` (as `read_begun`), `reuse_grant bool`, `meta page` |
| `commit_begin` | `txid u64` (the new one), `meta page`, `manifest_pages indexed_page_list` (the manifest overflow chain, in chain order) | none |
| `commit_pages` | `pages indexed_page_list` | none |
| `commit_end` | empty | `committed`: `txid u64` |
| `abort` | empty | none |
| `commit_status` | `txid u64`, `digest u64` | `commit_status_reply`: `status u8` (0 committed, 1 not committed, 2 unknown) |

**`begin_write`.** A read-only connection, or an owner whose own handle is read-only, answers
`25006` (non-fatal). A `begin_write` while the connection already holds or awaits the gate is a
protocol violation. The owner waits for the gate up to `lock_timeout_ms`, answering `55P03` (non-fatal)
on expiry, while it keeps serving the connection's other requests (§2). With the gate it pins its
committed snapshot for the connection — the **write pin**, counted like a read pin and released when
the write transaction ends — and replies with the same cache answer as `begin_read`, plus the reuse
grant: true when every pin on the database, the write pin included, is at or after the free list's
generation (page-service.md §6). The meta is the pinned snapshot's.

A connection that holds the gate may name the write pin's `txid` in `read_pages`.

**The commit.** The client sends `commit_begin`, every `commit_pages`, and `commit_end` back to back;
no other request may come between them on the connection. A delta's frames must fit `max_frame_length`:
the client splits body pages across `commit_pages` frames, and a delta whose manifest chain does not fit
one `commit_begin` cannot be committed (the client refuses it with `54000`). `txid` and every page
are checked against the owner state fixed at `begin_write`: the committed `txid` and page count, and,
under the reuse grant, the free pages of the committed free list (the **granted** pages). A page index
is **legal** when it is at least 2, below the new meta's page count, and either at least the committed
page count or granted.

- **Check A, at `commit_begin`, before anything is written:** `txid` is the committed `txid` + 1; the
  meta is one page that parses as a meta page of the file's format and page size, names `txid`, and has
  a page count no smaller than the committed one and a nonzero dirty count; `manifest_pages` is exactly
  the meta's manifest chain (its head, length, per-page header and ordinal, and chain CRC, as recovery
  checks them, format.md); every listed index is legal and strictly ascending, and every manifest page
  index is legal and not listed; and the listed pages include the catalog root and the free-list head.
  The manifest pages are then written.
- **Check B, for each `commit_pages` page, before it is written:** its index is listed and not yet
  received, its page type is a body type (1–7), its CRC-32 is valid, and its CRC-64 matches the
  manifest.
- **Check C, at `commit_end`:** every listed page arrived.

When a check fails, the owner discards the rest of the delta and answers `commit_end` with a fatal
`08P01`; the write transaction ends with it. Pages already written are unreachable orphans (beyond the
committed high-water or granted free pages), and no meta is written. A `commit_begin`,
`commit_pages`, or `commit_end` from a connection without the gate is a protocol violation (but see
revocation below).

Check C passing is the **point of no return**: the owner writes the meta slot `txid & 1`, executes the
single durable barrier, publishes the commit (§4, §6.4), answers `committed`, and only then releases the
gate, whatever happens to the connection. A write or sync failure poisons the owner: it stops serving
(every connection closes) and its own handle refuses commits until reopened. A failure after check C
sends no error frame, since the meta may already be durable: the client sees a lost connection and
resolves the outcome (§6.4).

**`abort`** ends the write transaction: the write pin and the gate are released and any partial delta
is discarded. It is a protocol violation without a write transaction (unless revoked, below).

**Revocation.** A connection holding the gate that sends no frame for the owner's
idle-in-transaction timeout (page-service.md §11.1) is revoked: the delta is discarded and the write
pin and the gate are released. Its next write-path request reports `25P03` (non-fatal): `commit_end`
(any `commit_begin` and `commit_pages` before it are discarded), `begin_write`, or a `read_pages` naming
the revoked write pin's `txid`. An `abort` clears the revocation silently.

**`commit_status`** resolves a commit whose acknowledgement was lost (page-service.md §8.4). The owner
first waits for any commit past its point of no return to be published, then answers from its
resolution window (§4): 0 when the window holds `txid` with this digest, 1 when it holds `txid` with
another digest or `txid` is above the committed `txid`, 2 otherwise. A commit's `digest` is CRC-64/ECMA-182
(the validated-COW manifest CRC, initial value 0) of the complete meta page the commit published.

### 3.6 Ownership, backup, replication (phases 4–7, provisional)

| Message | Body | Reply |
|---|---|---|
| `request_ownership` | empty | `ownership_reply`: `status u8` (0 the owner is yielding, 1 refused) |
| `snapshot_stream` | reserved (phase 6) | — |
| `subscribe` | reserved (phase 7) | — |

### 3.7 `notice` and `error`

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
- **Pins.** Each connection's pins, its write pin included, are counted per `txid` and are part of
  the owner's reader watermark (transactions.md §8), exactly like a local reader's.
- **Resolution window.** The `(txid, digest)` pairs of the owner's most recent **256** published
  commits (its own, its clients', maintenance commits, and compactions), seeded at open with the
  committed meta slot and, when the other slot holds the commit just before it, that one too.
- **Writer gate.** One gate per owned database, taken by the owner's own write transactions (after
  its in-process writer gate), by its compaction (without waiting), and by client write transactions.

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
   (not a symlink) owned by the owner's effective uid with mode exactly `0700`: permission bits
   `0700` and no setuid, setgid, or sticky bit.
2. **Runtime directory** (the path would be longer than 100 bytes, or the bundle directory failed the
   check above): `<dir>/jed-<first 8 bytes of the incarnation, 16 hex>.sock`, where `<dir>` is the
   first that exists and passes the same ownership and `0700` check: `$XDG_RUNTIME_DIR` (Linux),
   `$TMPDIR` (macOS, already per-user), then `/tmp/jed-<uid>`, which the owner creates with `0700`.
   If the result is still longer than 100 bytes, the owner does not serve and records `flags = 0`.

Only a `presence EX` holder may unlink a stale endpoint at its own path before binding. An owner
removes its endpoint when it closes. Directory search permission is the access control: the kernel
checks it on `connect`, so only processes of the owner's user can connect (page-service.md §13).

Windows serving is not yet specified (page-service.md §14): `locking = service` fails `0A000` there.

## 6. Roles under `locking = service` (phases 2–3)

The behavior every core implements for the read and write paths. It is host state, so the real-process corpus
(`spec/conformance/process/`) and the corpus-through-a-client run (§7) check it, not byte fixtures.

### 6.1 Election at open

1. Resolve the path and prepare the bundle exactly as a coordinated open does (locking.md §3.1),
   including the process-local duplicate-open check and the hard-link rejection. Take `arrival SH` and
   validate the marker.
2. Loop until `file_lock_timeout_ms`:
   - Try `presence EX` without waiting. On success the opener is the **owner**: it releases `arrival`
     and opens the file as an alone process does, without a join-window probe.
   - Otherwise read the owner record. If it parses, names this process's uid, and is serving,
     connect to its endpoint and send `hello`. On `hello_ok` the opener is a **client**: it releases
     `arrival` and closes its bundle handles (a client holds no bundle lock, page-service.md §4.2). A
     refused `hello` (`0A000`) fails the open; a failed connect or a dropped connection is retried.
   - Pause briefly and repeat.
3. On timeout fail `55006` naming the path, adding "owned by another user's process" or "owned by a
   non-serving process" when the last record read said so.

`create` under `locking = service` takes `presence EX` with the exclusive acquisition loop and is the
owner. There is no promotion before phase 4: a client whose owner exits fails every later request
with `08006` until it is reopened. File attachments do not support `locking = service` yet (`0A000`);
the cross-owner gate order of page-service.md §5.5 lands with them.

### 6.2 The owner

- After opening, it draws its incarnation, records the committed meta page (the slot whose `txid` is
  the committed one), writes the owner record, binds the endpoint (§5.2), and accepts connections,
  each served concurrently with the owner's own sessions.
- Each owner commit of the main database publishes, together with its new root and under the same
  lock that orders reader pins against publication, the commit's meta page and its written indices
  (§4). A begin_read therefore always returns a meta page and invalidation list consistent with the
  snapshot it pins.
- A connection's pins count in the owner's reader registry, so they hold back free-page reuse,
  in-commit reclamation, and compaction (`55006` while any client pins) exactly as local readers do.
- Pages are read from the file at their index; the owner may use a separate read handle so a page
  read never waits for its own commit's barrier. Compaction resets the window (§4).
- It answers ownership and reserved requests with a fatal `08P01`.
- On close it stops accepting, closes every connection (clients see EOF), and removes its endpoint
  and owner record before releasing `presence`.

### 6.3 The client

- Its pager's byte store serves page indices 0 and 1 from the meta page of the version it adopted, and
  every other page through `read_pages` (or from the commit in progress, §6.4). It trusts that meta (no
  manifest re-validation; page-service.md §5.3); each page's own CRC-32 is still checked on decode. Its
  handle is read-only exactly when its connection is.
- **Read begin.** At each read begin that pins a snapshot for a local reader (a read-only session's
  first read, `BEGIN READ ONLY`, and the like), a client sends `begin_read` with its cache's incarnation and adopted `txid`. It drops the listed
  indices (or, for `invalidate = 1`, its whole page cache) from its buffer pool; when the reply's
  `txid` is newer than its adopted one (or the cache was invalidated wholesale) it adopts the meta,
  reloads its catalog/interior skeleton, and invalidates its plan caches. Read begins and adoptions
  are serialized, so the cache always corresponds to exactly one adopted version.
- **Pin discipline.** Besides its write pin (§6.4), a client holds exactly one owner pin on each
  version a local reader pins, and no other: an idle client pins nothing, so it never holds back compaction. A read begin's
  `read_begun` pin is kept for the local reader it begins: no `end_read` for that version may be sent
  between the reply and the local reader's registration, even if every other local reader of the
  version ends in between. When the reply duplicates a pin already held, the client sends `end_read`
  for the duplicate at once. A writer's begin sends `begin_write` instead and holds the write pin (§6.4). When a
  version's last local reader ends, the client sends `end_read` for it. `read_pages` names the newest
  version the client holds a pin on (the write pin included), chosen and sent while no `end_read`,
  `abort`, or `commit_end` releasing that version can overtake it on the connection: page counts only grow between compactions and a pin blocks compaction, so the
  newest pinned version bounds every page of every older pinned one.
- Requests from the process's sessions share the connection; replies are matched by `request_id`
  (§2), so a writer waiting for the gate never stalls a reader's page fetch. A lost connection fails
  every waiting request, and every later one, with `08006`.
- Closing the client closes the connection; the owner releases its pins at EOF.
- `end_read` and EOF are asynchronous: the owner releases a pin when it reads them, which may be a
  moment after the client's reader ended. So another process's compaction right after that can
  still fail `55006` (compaction never waits); a caller that needs it retries.

### 6.4 The write path (phase 3)

- **Begin.** A client's write transaction (an autocommit write, a writable `BEGIN`, a write session)
  takes the process's in-process writer gate, then sends `begin_write` with the session's
  `lock_timeout_ms`. It applies the cache answer like a read begin (dropping pages, adopting a newer
  version), so the transaction starts from the owner's newest commit, and records the reuse grant.
  Local writers of one process wait on the in-process gate as today.
- **Commit recipe.** The client builds the commit with the ordinary validated-COW recipe
  ([validated-cow.md](../design/validated-cow.md)) over its store, with two differences: it never
  reclaims (no reachability walk, no forced compaction for `max_storage_bytes`; the free list it
  persists is the committed one minus the pages it reused), and it reuses free pages only under the
  reuse grant. Its writes collect in the store, and the store's sync sends them as the delta: the meta
  and the manifest chain in `commit_begin`, every other page in ascending index order.
- **Outcome.** `committed`: the client adopts its own commit as its current version (no other commit
  came between its base and it) and publishes it to its sessions. `25P03` or `08P01`, a send that fails
  before `commit_end` was written, or a refusal before sending: not committed; the transaction fails
  with that error and the handle stays usable unless the connection is lost. A connection lost after
  `commit_end` was written: the client reconnects to whichever process owns the database (the owner
  record) until its open timeout and asks `commit_status`. Committed: the commit succeeds; not
  committed: it fails `08006`; unknown, or no owner answered: `08007`. The lost connection stays lost
  (`08006` for every later request) until the handle is reopened (promotion is phase 4).
- **Rollback.** A write transaction that ends without a commit reaching the owner sends `abort`.
- **Owner publication of a client commit.** At the point of no return the owner writes and syncs the
  meta, then, under the lock that orders pins against publication, records the new page count and
  free list (read from the committed free-list chain), the commit's meta and written indices (the
  listed pages and the manifest pages) in its invalidation window, and its resolution pair. Its own
  sessions adopt the commit's catalog at their next transaction begin, never in the commit path, so
  nothing a client commits can fail after it is durable (page-service.md §7); a transaction whose begin
  cannot adopt it (a collation this owner lacks, say) fails alone. After acknowledging, still holding
  the gate, the owner adopts the catalog itself and, when the commit left the high-water past the
  reclamation threshold (`page_count > compact_min_pages` and `page_count > compact_growth × the
  live count at the last reclamation`, or that count unknown) and no pin is older than the next
  `txid`, runs a **maintenance commit** (page-service.md §6): the unchanged snapshot committed at the
  next `txid` with in-commit reclamation — it rewrites the catalog chain and the free list. An owner
  whose service runs off its engine thread (TS) hands the held gate to the engine thread, which adopts
  the catalog and runs the maintenance commit at its next event-loop turn, transaction begin, or gate
  wait, then releases the gate; until then client writers wait for the gate.
- **Compaction** takes the gate without waiting (`55006` while a client holds it) and first adopts any
  client commit. A client cannot compact (`55006`: another process owns the file).
- **Test hook.** Each core exposes a test-only call that makes a serving owner drop the connection
  instead of acknowledging its next client commit, after the commit is durable, so the real-process
  corpus can exercise `commit_status` resolution.

## 7. The corpus through a client

`mise run conformance:service` runs the SQL corpus with each core as a page client of another core's
owner (Rust→Go, Go→TS, TS→Rust, as client→owner). It is the disk storage mode (conformance.md §3)
with the owner out of process: the harness creates the database under `locking = service`, hands it to
the other core's owner actor, and runs **every record, writes included,** on a page client, reopening a
fresh client handle and session before each record as the disk mode reopens its handle. Rows, errors,
plans, and costs must match the corpus exactly, because page reads are logical cost units. Files
skipped in disk mode are skipped here too.
