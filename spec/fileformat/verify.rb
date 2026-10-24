require "set"
require "zlib"
#!/usr/bin/env ruby
# frozen_string_literal: true

# Independent reference implementation of the on-disk file format (format.md).
# It encodes the golden fixtures from a declarative description and decodes them
# back, so the goldens in fixtures/ are pinned by a THIRD implementation, not just
# self-certified by the two cores that also read/write them (CLAUDE.md S8). Pure
# Ruby, ASCII-only, no gem dependency; test-time only (CLAUDE.md S5).
#
# DO NOT retire this in favor of "let a core author the fixtures" without reading
# spec/design/conformance.md (the `# fixture:` section). Beyond cross-checking the
# goldens (a diminishing-returns 4th voice now that Rust+Go+TS agree), this file has
# a SECOND, irreplaceable role the cores cannot take over: it FORGES on-disk images
# the engine refuses to produce by invariant — e.g. collation_skew_corrupt.jed (a
# bogus 9999.0.0 collation pin + an index keyed in the WRONG collation). A core could
# only author that with invariant-violating, test-only seams (decoupling an index's
# key collation from its column; letting a recorded version diverge from the loaded
# bundle), i.e. polluting the engine to serve a test. So full removal is off the table
# while the negative-path fixtures exist (decision evaluated 2026-06-27).
#
#   ruby spec/fileformat/verify.rb              # verify fixtures/ match the reference
#   ruby spec/fileformat/verify.rb --generate   # (re)write fixtures/ from the reference
#   (or: mise run verify)
#
# Exit 0 = all fixtures conform; nonzero = mismatch (prints the offending case).

MAGIC = "JEDB".b
VERSION = 50 # v50: index_flags bit5 is_constraint — set on the unique index that backs a UNIQUE
# constraint, clear on a bare CREATE UNIQUE INDEX (legal only with bit0 unique; constraints.md §5.5).
# v49: batched durability — a commit writes the meta slot not holding the newest durable
# meta, and a pending commit's manifest lists every page written since that floor; no byte layout
# change (validated-cow.md §4).
# v48: view column defaults, dependency kinds 2 (sequence) / 3 (composite type), and a
# view flags byte (security_barrier, security_invoker) in the view entry; a catalog entry longer than a
# page spans consecutive catalog pages flagged `continued` in header byte 1 (views.md §3/§6/§16,
# format.md *Catalog*). No other byte layout change.
# v47: multi-column GIN indexes — a GIN index may list several key columns, and each
# term of a multi-column GIN index is prefixed by its column_tag (u8, the 0-based key position,
# gin.md §4.4) — and GIN posting runs: every GIN tree record is a run of one term's storage keys,
# term ‖ last ‖ front-coded body ‖ u16 body length (gin.md §4.5). No catalog byte layout change.
# v46: ordered index keys — index_flags bit4 has_key_options; when set, one option byte
# per key element (bit0 desc, bit1 nulls_first) follows the optional timezone dependencies, and the
# element's entry slots use the encoding.md §2.16 ordered slot (indexes.md §11). No other change.
# v45: exclusion-constraint follow-ons (gist.md §7.1/§7.2) — no byte layout change; the
# v27 has_predicate bit is legal on a GiST index backing an exclusion (EXCLUDE … WHERE (predicate)),
# and an exclusion may name a non-unique ordered (btree) backing index whose operators are all `=`
# (EXCLUDE USING btree). A fixture index's `where:` lambda mirrors its predicate (this reference does
# not evaluate SQL): only rows it admits are indexed.
             # v44: a composite column may carry an expression DEFAULT — column flags bit3 + the
             # expr-text (u16 length + UTF-8) after the type name (composite.md §12). No other byte
             # layout change. v43: timestamp(p) / interval qualifier typmods — a timestamp/timestamptz column or
             # composite field appends u8 precision (0xFF = none), an interval one u8 fields ‖ u8
             # precision (spec/design/timestamp.md §8, interval.md §6.1). v42: GIN term statistics — a statistics summary's flags gain bit2 term_eligible, a
# term-eligible summary appends term_occurrences i64 ‖ untracked_term_rows i64 ‖ term_count u16, and
# stats_kind 3 term items follow (statistics.md §4.5/§7). No other byte layout change.
# v41: a composite-type entry admits a range-typed field — field_type_code 17 + the
# one-byte range element descriptor, before the field flags byte (composite.md §3). No other byte
# layout change.
# v40: bit 6 of a foreign key's actions byte is match_full (MATCH FULL; 0 = MATCH SIMPLE —
# constraints.md §6.3/§6.9); bit 7 stays reserved 0. No other byte layout change.
# v39: a trailing u8 check_option on every view entry (0 none, 1 LOCAL, 2 CASCADED —
# views.md §15.6); no other byte layout change.
# v38: views — entry_kind 5 view entries emitted last, in name byte order (views.md §3);
# no other byte layout change.
# v37: exact (folded) identifier names — no byte layout change; catalog groups sort by
# exact-name bytes and persisted expression text renders bare words folded (grammar.md §3).
# v36: an array column entry's flags gain bit6 has_collation (a text[] column's
# non-C collation, the name after the element descriptor) and a collated text[] key stores each
# element's sort key (collation.md §1, encoding.md §2.14). v35: every CHECK entry gains a trailing check_flags byte (extensibility.md §7.1.3).
# format_version 35: after each check's expression text, a u8 check_flags (bit0 has_host_deps; bits
# 1-7 reserved, written 0) and, only when bit0 is set, the v31 host-function dependency list (same
# bytes as the per-index list below). A fixture writes it as `host_deps:` on the check Hash.
# format_version 34: meta offset 56 carries the exact live page count (memory.md S8.7).
# format_version 32 adds timezone dependencies; v31: host-function index dependencies (extensibility.md §8.1) — the
# per-index index_flags byte gains bit2 has_host_deps, and (only when set) after the v27 predicate a
# u16 dep_count + per dependency name (u16 len + UTF-8) ‖ arg_count u16 ‖ arg type codes (u8 each) ‖
# result type code (u8) ‖ component_id (u16 len + UTF-8) ‖ semantic_version u32, in ascending
# (name, arg-type codes) order. A fixture writes it as `host_deps: [{ name:, arg_types: [codes],
# result:, component_id:, semantic_version: }]`. An index with no host-function key is byte-identical
# to v30, so a file with no such index moves to v31 only by its version byte + meta CRC.
# format_version 30: FK actions use two three-bit fields in the actions byte;
# format_version 29: deterministic per-column statistics use kind-4 catalog entries;
# format_version 28: every table catalog entry appends row_count i64 (big-endian
# two's-complement, restricted to nonnegative values) after root_data_page. The reference derives it
# from the declarative rows and rejects `(root == 0) != (count == 0)` on decode. format_version 27:
# partial-index predicates (indexes.md §9) — the per-index index_flags
# byte gains bit1 has_predicate, and (only when set) a u16 length + canonical predicate text follows
# index_root_page (a fixture writes it as `predicate: "<text>"`). B-tree only. A non-partial index is
# byte-identical to v26, so a file with no partial index moves to v27 only by its version byte + meta CRC.
# format_version 26: expression index keys (indexes.md §1/§6) — a per-index key element
# is a u16 column ordinal OR the 0xFFFF sentinel + u16 length + canonical expression text (a fixture
# writes it as `{ expr: "<text>" }`). Only the index-list changes; a plain column index is byte-identical
# to v6, so a file with no expression index moves to v26 only by its version byte + meta CRC.
# format_version 25: on-disk free-list persistence (format.md; storage.md §6) — meta
# offset 28 becomes free_list_head (0 = empty), and a page_type 7 free-list page persists the
# unconsumed free-list so open reads it directly instead of reconstructing it by walking every leaf.
# A from-scratch image (build_image) has an EMPTY free-list, so free_list_head = 0 and no page_type 7
# page: every golden's only v25 change is its version byte + meta CRC (a non-empty persisted free-list
# arises only from incremental churn, pinned by per-core tests). format_version 24: the B+tree reshape
# (format.md; bplus-reshape.md B1) — records live
# (page_type 2) stores its records COLUMN-MAJOR: key directory (N+1 u32 prefix-sum) | key blob |
# column directory (K+1 u32 region offsets, colStart[K] = payload end) | per column a value directory
# (N+1 u32 prefix-sum) then that column's N value bodies. The per-value codec is byte-unchanged;
# interior pages (page_type 3) stay row-major. The per-record key_len u16 is dropped, so a record's
# split weight is unchanged (2 + key_len + Σ value_size) but a leaf's payload gains
# directoryOverhead(N,K) and RECORD_MAX tightens to (C − (12+16K))/2. No catalog/interior/overflow
# byte change. format_version 22: varchar(n) length limits (spec/design/types.md §15) — a text column
# entry appends a u32 varchar_max_len in the typmod slot (type_code 4): 0 = unbounded, 1…10485760 =
# the varchar(n)/string(n) limit; a composite text field carries the same u32. The value codec is
# unchanged (a value is checked/truncated before encoding). A file whose every text column is unbounded
# still moves to v22 by its version byte + a 0 on each text column/field. format_version 21: EXCLUDE
# constraints (spec/design/gist.md §7/§8, GX3) — a per-table
# exclusion list after the foreign-key list: excl_count(u16), then per exclusion the name, the backing
# GiST index name, and the (column ordinal u16, operator strategy u8) element vector (&& = 0, = 1). The
# backing GiST index is stored like any GiST index — the index list now admits MULTI-COLUMN GiST
# indexes whose leaf/interior bound is the per-column component bounds concatenated (single-column
# GX1/GX2 bytes are unchanged). A table with no exclusion still moves to v21 by its version byte + the
# zero count. format_version 20: GiST indexes (spec/design/gist.md, GX1) — a per-index index_kind = 2
# selects the GiST access method, and the index's on-disk form is a persisted R-tree of bounding-
# predicate nodes (page types 5 = GiST leaf, 6 = GiST interior, §4.1). A leaf entry is
# bound_len(u16) ‖ encode_range_body(bound) ‖ skey_len(u16) ‖ skey; an interior entry is
# bound_len(u16) ‖ encode_range_body(union) ‖ child_page(u32). Entries are ordered canonically
# (range_total_cmp, ties by storage key / subtree-min key); pages allocated post-order. The catalog
# index entry is unchanged (index_root_page points at the R-tree root, 0 for empty). A file with no
# GiST index still moves to v20 only by its version byte. format_version 19: storable json/jsonb (below) — a
# column type can be json (type_code 18) or jsonb (type_code 19), both plain scalar catalog entries
# with NO extra descriptor (like text/uuid). A json value's body is the verbatim text, length-prefixed
# like text (§4); a jsonb value's body is the self-delimiting tagged-node tree (§2 — node tags + LEB128
# varint counts, a number is the decimal body), riding the large-value overflow + LZ4 path. No catalog
# shape change, so a file with no json/jsonb column moves to v19 only by its version byte.
# format_version 18: reference-only collations — the catalog entry_kind 3 collation
# entry is now METADATA ONLY (a flags byte bit0 is_default; then name + unicode_version +
# cldr_version + description, each u16-len + UTF-8), emitted after sequences and before tables. The
# compiled table is NOT in the file — it is vendored into the binary and resolved by name on open
# (spec/design/collation.md §2/§5/§9); the recorded version is the pin. This supersedes v17's baked
# snapshot (the LZ4-compressed `.coll` artifact is gone). The per-column collation is unchanged (the
# column-entry flags byte bit6 has_collation + a trailing name; C leaves it clear). format_version 16
# was range columns — a column type can be a range (type_code 17 + an
# inline element-type descriptor, one scalar code, spec/design/ranges.md §3), and a range value is a
# flags byte (bit0 EMPTY, bit1 LB_INF, bit2 UB_INF, bit3 LB_INC, bit4 UB_INC) followed by the present
# bound bodies (each the element's value-codec body, no presence tag — §4). An empty range is the
# lone flags byte 0x01. Discrete subtypes (i32/i64/date) are stored canonical `[)`. A range owns no
# B-tree and carries no default this slice. format_version 15 was IDENTITY columns — the column-entry
# flags byte gains bit4 is_identity + bit5 identity_always (GENERATED ALWAYS set / GENERATED BY DEFAULT
# clear); an identity column desugars exactly like serial (an owned sequence + a nextval expression
# default + NOT NULL), so the only on-disk change is those two bits, which restore the INSERT/UPDATE
# 428C9 gating after a reopen (spec/design/sequences.md §13). format_version 14 was the serial
# OWNED-sequence link — the sequence-entry flags byte gains bit2 has_owner; when set, the flags byte is
# followed by the owner reference (owner table name u16-len + bytes, then owner column ordinal u16). A
# non-owned sequence writes nothing after the flags byte (the v12 shape). The link records the OWNED BY
# relationship a serial column establishes, so a reopened database still auto-drops the owned sequence
# on DROP TABLE (spec/design/sequences.md §12).
# format_version 13 was GIN inverted indexes — each catalog index entry gains a one-byte
# index_kind (0 = ordered B-tree, 1 = GIN) between index_flags and index_root_page; a GIN index's
# tree holds term‖storage-key entries, empty payload (spec/design/gin.md). format_version 12 was
# SEQUENCES — a third kind-tagged catalog entry (entry_kind u8: 2 = sequence; joining 0 table, 1
# composite-type). A sequence entry is name + six fixed i64 fields (increment, min_value, max_value,
# start, cache, last_value; big-endian two's-complement, no sign-flip) + a flags byte (bit0 cycle,
# bit1 is_called). Emission order: composites (1), sequences (2), tables (0), each name-sorted. A
# sequence owns no B-tree (spec/design/sequences.md §3). format_version 11 was FOREIGN KEY
# constraints — the table catalog entry gains a
# foreign-key list (fk_count + per FK: name, local ordinals, ref table, ref ordinals, actions byte)
# after the index list, before root_data_page (spec/design/constraints.md §6, format.md). An FK owns
# no B-tree (no root page); a file with no FKs still moves to v11 (every table entry gains fk_count=0).
# v10 was array (T[]) columns — a column type can be an array (type_code 15
# + an element-type descriptor in the catalog, spec/design/array.md §3), and a value is the compact
# array body (§4). v9 was composite (row) types — the catalog became a chain of kind-tagged entries
# (entry_kind u8: 0 table, 1 composite-type), composite-type entries first (name order); two-pass
# load (format.md *Version scope*). v8 was the per-column expression-default flag (bit3) + expr-text;
# v7 added a per-page CRC-32 on every body page (header 12→16)
PAGE_HEADER = 16 # v7: the 12-byte v6 header (page_type/item_count/next_page) + a 4-byte per-page crc32 at offset 12
ROOT_PAGE = 2  # the catalog root of a *fresh empty* db; relocatable thereafter (meta.root_page)
TXID = 1
PAGE_CATALOG = 1
PAGE_LEAF = 2
PAGE_INTERIOR = 3
PAGE_OVERFLOW = 4 # an out-of-line value slab, chained by next_page (large-values.md §12)
PAGE_GIST_LEAF = 5     # v20: a GiST R-tree leaf (spec/design/gist.md §4.1)
PAGE_GIST_INTERIOR = 6 # v20: a GiST R-tree interior (one bound per child, not N separators / N+1 children)
GIST_FANOUT = 4 # v20: max entries per GiST node; the (N+1)-th triggers a median picksplit (gist.md §4.1)

# Value-codec presence tags beyond 0x00 present-inline-plain / 0x01 NULL (large-values.md §12/§13;
# format.md "Large values"): 0x02 external-plain (u32 first_page + u32 payload_len), 0x03
# inline-compressed (u32 raw_len + u16 comp_len + LZ4 block), 0x04 external-compressed
# (u32 first_page + u32 stored_len + u32 raw_len; the chain carries the COMPRESSED block).
# The *_LEN constants are each form's full in-record size (tag included).
TAG_EXTERNAL = 0x02
TAG_INLINE_COMP = 0x03
TAG_EXTERNAL_COMP = 0x04
EXTERNAL_PTR_LEN = 1 + 4 + 4
INLINE_COMP_OVERHEAD = 1 + 4 + 2 # + comp_len bytes
EXTERNAL_COMP_PTR_LEN = 1 + 4 + 4 + 4
S_COMPRESS = 32 # payloads below this are never fed to the encoder (large-values.md §13)

require_relative "lz4"
require_relative "xaes"

# --- the encrypted variant (spec/fileformat/encrypted.md) -------------------------------------
# `$enc` is nil while a plaintext image is built or decoded. For an encrypted one it holds the page
# and meta keys, the nonce stream (encoder), and `tags` — every page's tag, recorded when the page is
# sealed (encoder) or when a pointer naming it is read (decoder), so a pointer binds it and a read
# checks it (§2).
$enc = nil
RESERVE = 40 # 24-byte nonce + 16-byte tag at the end of every sealed page (§1)
TAG_LEN = 16
ZERO_TAG = ("\x00" * 16).b
ENC_FIRST_BODY = 4 # pages 2-3 are the key block
ENC_SEED = 0x4A454442 # the goldens' insecure seeded key-entropy stream (§8)
ENC_KEY = ("\x4A" * 32).b # the goldens' host key (kdf 2)

def tag_len = $enc ? TAG_LEN : 0
def first_body = $enc ? ENC_FIRST_BODY : ROOT_PAGE

# The tag a pointer to page `index` carries (§2): nothing in a plaintext file, zeros for no page.
def tag_of(index)
  return "".b unless $enc
  return ZERO_TAG if index.zero?

  $enc[:tags].fetch(index)
end

# A page pointer in the variant's layout: u32 index ‖ its tag.
def ptr(index) = u32(index) + tag_of(index)

# Read a pointer, recording the tag it binds for the read of that page (decoder).
def read_ptr(buf, pos)
  ib, pos = take(buf, pos, 4)
  index = ib.unpack1("N")
  return [index, pos] unless $enc

  tag, pos = take(buf, pos, TAG_LEN)
  raise "a pointer to no page carries a tag" if index.zero? && tag != ZERO_TAG

  $enc[:tags][index] = tag.b unless index.zero?
  [index, pos]
end

def ad_prefix(role, position) = "JEDB".b + [1, role].pack("CC") + u32(position)

# Seal body page `index` (§4): the plaintext page of capacity C (header with a zero CRC; a chain
# page's successor tag leads the payload), XAES-256-GCM under the page key with a fresh nonce.
def seal_body(index, type, count, nxt, payload, flags: 0)
  ps = $enc[:ps]
  body = "\x00".b * (ps - RESERVE)
  body.setbyte(0, type)
  body.setbyte(1, flags)
  body[4, 4] = u32(count)
  body[8, 4] = u32(nxt)
  lead = [PAGE_CATALOG, PAGE_OVERFLOW, 7].include?(type) ? tag_of(nxt) : "".b
  content = lead + payload
  raise "page payload overflows its page" if content.bytesize > ps - RESERVE - PAGE_HEADER

  body[PAGE_HEADER, content.bytesize] = content
  nonce = $enc[:rng].bytes(24)
  sealed = Xaes.seal($enc[:page_key], nonce, body, ad_prefix(1, index))
  tag = sealed.byteslice(-16, 16)
  $enc[:tags][index] = tag
  sealed.byteslice(0, ps - RESERVE) + nonce + tag
end

# Record a finished body page: sealed now in an encrypted image (its tag is final before any parent
# names it), else kept as fields for write_page.
def put_page(pages, index, type, count, payload, nxt, flags: 0)
  pages[index] = if $enc
                   [:sealed, seal_body(index, type, count, nxt, payload, flags: flags)]
                 else
                   [type, count, payload, nxt]
                 end
end

# The 128-byte key block for the goldens' host key (§5): draws the DEK, KDF salt and wrap nonce.
def new_key_block(rng)
  dek = rng.bytes(32)
  [wrap_key_block(dek, 1, ENC_KEY, rng), dek]
end

# The key block wrapping `dek` for `host_key` at `generation` (§5): draws the KDF salt, then the wrap
# nonce. A credential change is this at the next generation over the same DEK.
def wrap_key_block(dek, generation, host_key, rng)
  salt = rng.bytes(32)
  nonce = rng.bytes(12)
  head = "JEDK".b + u16(1) + [1, 2].pack("CC") + u64(generation) + ("\x00" * 16).b + salt + nonce
  kek = Xaes.host_kek(host_key, salt)
  block = head + Xaes.gcm_seal(kek, nonce, dek, head)
  block + u32(crc32(block))
end

# The host keys an encrypted image may be under: the goldens' (ENC_KEY, 32 bytes of 0x4A) first, then
# the simulation's keys 1–3 (32 bytes of 0x4A + n; simulation-testing.md §14 S12).
DECODE_KEYS = (0..3).map { |n| ([0x4A + n].pack("C") * 32).b }.freeze

# Unwrap a host-key key-block copy with `key` (decoder): the DEK, or nil.
def unwrap_key_block(block, key = ENC_KEY)
  return nil unless block.byteslice(0, 4) == "JEDK".b && block.byteslice(124, 4).unpack1("N") == crc32(block.byteslice(0, 124))
  return nil unless block.byteslice(4, 4) == u16(1) + [1, 2].pack("CC")

  kek = Xaes.host_kek(key, block.byteslice(32, 32))
  Xaes.gcm_open(kek, block.byteslice(64, 12), block.byteslice(76, 48), block.byteslice(0, 76))
end

# One key-block copy (§5) as [block, generation], or nil when it is not a valid host-key copy.
def key_block_copy(page)
  b = page.byteslice(0, 128)
  return nil unless b.byteslice(0, 4) == "JEDK".b && b.byteslice(124, 4).unpack1("N") == crc32(b.byteslice(0, 124))
  return nil unless b.byteslice(4, 4) == u16(1) + [1, 2].pack("CC") && b.byteslice(16, 16) == ("\x00" * 16).b
  return nil unless page.byteslice(128, page.bytesize - 128).bytes.all?(&:zero?)

  generation = b.byteslice(8, 8).unpack1("Q>")
  generation >= 1 ? [b, generation] : nil
end

# Select the key-block copy to unwrap (§5): the valid copy with the greater generation; two valid
# copies of one generation must be identical.
def select_key_block(page2, page3)
  copies = [key_block_copy(page2), key_block_copy(page3)].compact
  raise "no valid key block" if copies.empty?
  raise "key block copies disagree" if copies.size == 2 && copies[0][1] == copies[1][1] && copies[0][0] != copies[1][0]

  copies.max_by { |_, generation| generation }[0]
end

# Seal an encrypted meta page (§6): an empty-plaintext AEAD under the meta key over the slot and the
# page with its CRC and reserve zeroed (no manifest chain in a from-scratch image); CRC last.
def seal_meta(meta, slot)
  ps = meta.bytesize
  meta[32, 4] = u32(0)
  meta[ps - RESERVE, RESERVE] = "\x00".b * RESERVE
  nonce = $enc[:rng].bytes(24)
  tag = Xaes.seal($enc[:meta_key], nonce, "".b, ad_prefix(2, slot) + meta)
  meta[ps - RESERVE, RESERVE] = nonce + tag
  meta[32, 4] = u32(meta_crc(meta))
  meta
end

WIDTH = { "i16" => 2, "i32" => 4, "i64" => 8, "timestamp" => 8, "timestamptz" => 8,
          "date" => 4 }.freeze
TYPECODE = { "i16" => 1, "i32" => 2, "i64" => 3, "text" => 4, "boolean" => 5, "decimal" => 6,
             "bytea" => 7, "uuid" => 8, "timestamp" => 9, "timestamptz" => 10, "interval" => 11,
             "f64" => 12, "f32" => 13, "date" => 16, "json" => 18, "jsonb" => 19 }.freeze
CODETYPE = TYPECODE.invert.freeze

# An array (T[]) column type is the element type's string with a trailing "[]" (spec/design/array.md
# §2 — structural, no catalog object). `array_elem("i32[]")` => "i32"; nil for a non-array type.
def array_elem(type) = type.is_a?(String) && type.end_with?("[]") ? type[0...-2] : nil

# The six built-in range types map to their element (subtype) string (spec/design/ranges.md §1 —
# structural, no catalog object). `range_elem("i32range")` => "i32"; nil for a non-range type. The
# jed canonical names are used (`int4range`/`int8range` are parser-only aliases, never stored).
RANGE_ELEM = { "i32range" => "i32", "i64range" => "i64", "numrange" => "decimal",
               "tsrange" => "timestamp", "tstzrange" => "timestamptz",
               "daterange" => "date" }.freeze
def range_elem(type) = type.is_a?(String) ? RANGE_ELEM[type] : nil

# uuid-raw16 (encoding.md §2.7): the 16 raw bytes of the canonical 8-4-4-4-12 form. Used both
# as the value-codec body (fixed 16 bytes, no length prefix) and as the PRIMARY-KEY bytes
# (uuid is the first non-integer key — no sign-flip, escape, or terminator).
def uuid_to_bytes(s) = [s.delete("-")].pack("H*")

def uuid_from_bytes(bytes)
  h = bytes.unpack1("H*")
  "#{h[0, 8]}-#{h[8, 4]}-#{h[12, 4]}-#{h[16, 4]}-#{h[20, 12]}"
end

# --- declarative fixtures (mirror what the cores build via SQL) --------------

# A column. `precision`/`scale` are the decimal typmod (only meaningful for type "decimal";
# nil = an unconstrained `numeric` column, or a non-decimal column). `not_null` defaults to the
# pk flag (a PRIMARY KEY is implicitly NOT NULL). `default` is the column's CONSTANT DEFAULT value
# as the cores store it (already type-coerced): the sentinel :none = no default (flags bit2 off),
# `nil` = an explicit DEFAULT NULL, any other value = that default. `default_expr` is the column's
# EXPRESSION DEFAULT as persisted text (flags bit3, v8; e.g. "uuidv7 ( )" — the rendered token
# sequence), or nil for none; mutually exclusive with `default`. Always carried so the
# decode-side column hash compares equal (format.md stores the typmod/default only when its bit is
# set).
# `identity` is nil (not an identity column), :always (GENERATED ALWAYS), or :by_default (GENERATED
# BY DEFAULT) — the v15 column flag bits 4 (is_identity) + 5 (identity_always); an identity column
# also carries not_null + an expression default (the nextval, spec/design/sequences.md §13).
def col(name, type, pk: false, not_null: nil, precision: nil, scale: nil, varchar_len: nil,
        default: :none, default_expr: nil, identity: nil, collation: nil, temporal: nil)
  { name: name, type: type, pk: pk, not_null: not_null.nil? ? pk : not_null,
    precision: precision, scale: scale, varchar_len: varchar_len, default: default,
    default_expr: default_expr, identity: identity, collation: collation, temporal: temporal }
end

# A timestamp / interval typmod (v43, spec/design/timestamp.md §8, interval.md §6.1): `fields` is the
# interval qualifier code (spec/types/datetime_units.toml, 0 = none — always 0 for a timestamp) and
# `precision` 0..6 or nil.
def temporal(fields: 0, precision: nil) = { fields: fields, precision: precision }

TEMPORAL_TYPES = %w[timestamp timestamptz interval].freeze
NO_TEMPORAL_PRECISION = 0xFF

# The v43 temporal typmod slot of a timestamp/timestamptz (u8 precision) or interval (u8 fields ‖ u8
# precision) column or field; empty for every other type.
def temporal_typmod_bytes(type, tm)
  prec = tm&.dig(:precision) || NO_TEMPORAL_PRECISION
  case type
  when "timestamp", "timestamptz" then [prec].pack("C")
  when "interval" then [tm&.dig(:fields) || 0, prec].pack("CC")
  else "".b
  end
end

# Decode the slot temporal_typmod_bytes writes (nil = no typmod); an out-of-range precision or an
# unknown qualifier code is corrupt.
def decode_temporal_typmod(type, buf, pos)
  return [nil, pos] unless TEMPORAL_TYPES.include?(type)

  fields = 0
  if type == "interval"
    fb, pos = take(buf, pos, 1)
    fields = fb.getbyte(0)
    raise "unknown interval qualifier #{fields}" if fields > 13
  end
  pb, pos = take(buf, pos, 1)
  prec = pb.getbyte(0)
  raise "invalid temporal precision #{prec}" unless prec == NO_TEMPORAL_PRECISION || prec <= 6

  precision = prec == NO_TEMPORAL_PRECISION ? nil : prec
  return [nil, pos] if fields.zero? && precision.nil?

  [temporal(fields: fields, precision: precision), pos]
end

# A composite-type field (format.md *Composite-type entry*, v9): a name + type (a scalar string,
# or a composite type NAME for a nested composite) + NOT NULL flag + decimal typmod / varchar(n) len.
def field(name, type, not_null: false, precision: nil, scale: nil, varchar_len: nil, temporal: nil)
  { name: name, type: type, not_null: not_null, precision: precision, scale: scale,
    varchar_len: varchar_len, temporal: temporal }
end

# A composite (row) type definition (`CREATE TYPE name AS (...)`): a name + ordered field list.
def ctype(name, fields) = { name: name, fields: fields }

# A sequence definition (`CREATE SEQUENCE name ...`, spec/design/sequences.md §3). The six i64
# fields + the cycle/is_called flags exactly as the cores persist them; `last_value` defaults to
# `start` (a fresh sequence). `owned_by` is `[table, column_ordinal]` for a `serial` column's OWNED
# sequence (§12, v14), else `nil`. The on-disk entry is fixed-width through the flags byte, with the
# owner reference a conditional tail (format.md *Sequence entry*).
def seq(name, increment:, min_value:, max_value:, start:, cache: 1, cycle: false,
        last_value: nil, is_called: false, owned_by: nil)
  { name: name, increment: increment, min_value: min_value, max_value: max_value,
    start: start, cache: cache, cycle: cycle,
    last_value: last_value.nil? ? start : last_value, is_called: is_called, owned_by: owned_by }
end

# A composite type defined, persisted, AND used by a stored column (S3 — composite.md §4). The
# value codec is pinned: row 1 has both fields present; row 2's `zip` is NULL (the bitmap's bit 1
# is set and the field contributes ZERO body bytes). A composite value is the field-value array;
# `nil` is a NULL field. The composite column's expression DEFAULT (v44) pins its flags bit3 + the
# expr-text after the type name. The cores build this via
#   CREATE TYPE addr AS (street text NOT NULL, zip i32)
#   CREATE TABLE t (id i32 PRIMARY KEY, home addr DEFAULT ROW('Elm', 1))
#   INSERT (1, ROW('Main', 90210)); INSERT (2, ROW('Oak', NULL))
COMPOSITE_TYPE_TABLE = {
  types: [ctype("addr", [field("street", "text", not_null: true), field("zip", "i32")])],
  tables: [{ name: "t", columns: [col("id", "i32", pk: true),
                                  col("home", "addr", default_expr: "row ( 'Elm' , 1 )")],
             rows: [[1, ["Main", 90210]], [2, ["Oak", nil]]] }]
}.freeze

# A table whose PRIMARY KEY is a COMPOSITE-TYPED column (the third container key,
# `composite-field-slots`, ../design/encoding.md §2.15 / composite.md §6) — distinct from the
# multi-column COMPOSITE_PK_TABLE below (a flat tuple of scalar columns). The stored key is the
# concatenation of the per-field §2.2 nullable slots: `0x00`‖text-terminated-escape(street) then
# `0x00`‖int-be-signflip(zip) — a recursive container key, self-delimiting by fixed arity (no
# terminator). Rows are listed in ascending composite-sort-key order (lexicographic — street, then
# zip breaking the 'Main' tie); the cores INSERT in this order (the tree shape is order-sensitive).
# The cores build this via
#   CREATE TYPE addr AS (street text NOT NULL, zip i32 NOT NULL)
#   CREATE TABLE t (id i32, home addr, PRIMARY KEY (home))
#   INSERT (1, ROW('', -1)); (2, ROW('Elm', 100)); (3, ROW('Main', 5)); (4, ROW('Main', 90210))
COMPOSITE_KEY_TABLE = {
  types: [ctype("addr", [field("street", "text", not_null: true), field("zip", "i32", not_null: true)])],
  tables: [{ name: "t", columns: [col("id", "i32"), col("home", "addr", pk: true)],
             rows: [[1, ["", -1]], [2, ["Elm", 100]], [3, ["Main", 5]], [4, ["Main", 90210]]] }]
}.freeze

# Nested composite types (a field whose type is another composite — persisted by NAME) used by a
# stored column. `line` sorts BEFORE `point` but references it, so the two-pass load (collect all,
# then resolve) is exercised: a single name-ordered pass would meet `line`'s reference before
# `point` is read. The row pins the recursive value codec descending through a composite field. The
# cores build this via
#   CREATE TYPE point AS (x i32 NOT NULL, y i32 NOT NULL); CREATE TYPE line AS (a point, b point)
#   CREATE TABLE t (id i32 PRIMARY KEY, ln line); INSERT (1, ROW(ROW(1, 2), ROW(3, 4)))
NESTED_COMPOSITE_TABLE = {
  types: [
    ctype("line", [field("a", "point"), field("b", "point")]),
    ctype("point", [field("x", "i32", not_null: true), field("y", "i32", not_null: true)])
  ],
  tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("ln", "line")],
             rows: [[1, [[1, 2], [3, 4]]]] }]
}.freeze

# A table with a COMPOSITE primary key (constraints.md §3): the stored key is the
# concatenation of the members' encodings in key order (a 4-byte i32 then a
# 2-byte i16 — mixed widths, ../design/encoding.md §2.3), pinning the cross-core
# composite key bytes; the catalog persists the v5 pk ordinal list [0, 1].
# Rows include a negative first component (sign-flip ordering) and
# first-component ties broken by the second. Listed in ascending tuple order — the
# cores build this via `CREATE TABLE t (a i32, b i16, v i16, PRIMARY KEY (a, b))`
# and insert in this order (the tree shape is order-sensitive).
COMPOSITE_PK_TABLE = {
  name: "t",
  columns: [col("a", "i32", pk: true), col("b", "i16", pk: true), col("v", "i16")],
  rows: [[-2, 5, 10], [1, 1, 20], [1, 2, 30], [1, 3, 40],
         [2, 0, 50], [2, 1, 60], [3, 7, 70], [3, 9, 80]]
}.freeze

# A table with CHECK constraints (constraints.md §4): exercises the v4 catalog check list —
# an auto-named single-column check, an explicitly-named multi-column check, and a check
# whose text exercises the token rendering (a doubled-quote string literal, decimal
# literals, >= / <=). Stored in evaluation (name) order: price_range < t_b_check <
# t_note_check. The cores build this via
#   CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2),
#     CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text,
#     CHECK (note = 'ok' OR note = 'a''b'))
# and insert rows in ascending key order.
CHECK_TABLE = {
  name: "t",
  columns: [col("a", "i32", pk: true), col("b", "i32"),
            col("price", "decimal", precision: 8, scale: 2), col("note", "text")],
  checks: [
    { name: "price_range", expr: "price >= 0.50 and price <= 9999.99" },
    { name: "t_b_check", expr: "b > 0" },
    { name: "t_note_check", expr: "note = 'ok' or note = 'a''b'" }
  ],
  rows: [[1, 5, "1.00", "ok"], [2, nil, "9999.99", "a'b"], [3, 100, "0.50", "ok"]]
}.freeze

PK_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("v", "i16")],
  # 20 rows: each record is 14 bytes, so a 256-byte page (cap 240, v7) overflows at 18 rows and
  # the tree becomes interior-root + two leaves (the load-bearing interior-node + split proof).
  # id 3 has a NULL value. Inserted in ascending key order (the tree shape is order-sensitive).
  rows: (1..20).map { |i| [i, i == 3 ? nil : i * 10] }
}.freeze

# The v28 catalog-tail fixture: three rows keep the tree to one small leaf while pinning a nonzero
# `root_data_page` followed by `row_count = 3`. `one_table_empty.jed` pins the zero/zero pair.
ROW_COUNT_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true)],
  rows: [[1], [2], [3]]
}.freeze

# v29 statistics catalog fixture. `fresh` is analyzed at its current four-row state; `stale` was
# analyzed at three rows and then received one insert, so its exact stored facts remain intact with
# the stale bit set. Small complete samples place every distinct non-NULL value in the MCV list.
STATISTICS_TABLES = [
  { name: "fresh",
    columns: [col("id", "i32", pk: true), col("v", "text")],
    rows: [[1, "a"], [2, "a"], [3, "b"], [4, nil]],
    statistics: [
      { column: 0, stale: false, analyzed_rows: 4, null_count: 0, width_sum: 16,
        distinct_count: 4, sample_rows: 4, sample_nonnull_rows: 4,
        mcv: [[1, 1], [2, 1], [3, 1], [4, 1]], histogram: [] },
      { column: 1, stale: false, analyzed_rows: 4, null_count: 1, width_sum: 9,
        distinct_count: 2, sample_rows: 4, sample_nonnull_rows: 3,
        mcv: [["a", 2], ["b", 1]], histogram: [] }
    ] },
  { name: "stale",
    columns: [col("id", "i32", pk: true), col("v", "text")],
    rows: [[1, "x"], [2, "x"], [3, nil], [4, "y"]],
    statistics: [
      { column: 0, stale: true, analyzed_rows: 3, null_count: 0, width_sum: 12,
        distinct_count: 3, sample_rows: 3, sample_nonnull_rows: 3,
        mcv: [[1, 1], [2, 1], [3, 1]], histogram: [] },
      { column: 1, stale: true, analyzed_rows: 3, null_count: 1, width_sum: 6,
        distinct_count: 1, sample_rows: 3, sample_nonnull_rows: 2,
        mcv: [["x", 2]], histogram: [] }
    ] }
].freeze

# v42 GIN term statistics fixture (statistics.md §4.5): `ANALYZE t (tags, doc)` over 105 rows. Every
# row's tags hold its own id and a shared 0, so 106 distinct terms truncate to the 100-term list (0 in
# 105 rows, then ids 1..99 by key bytes) with untracked_term_rows 1 (the 101st term's count). Four rows
# carry a document — nested keys, an array string element (a KEY term), a top-level string — and the
# rest are NULL. The cores build it via
#   CREATE TABLE t (id i32 PRIMARY KEY, tags i32[], doc jsonb)
#   INSERT INTO t SELECT g::i32, ARRAY[g::i32, 0::i32], CASE g WHEN 1 THEN '{"a": 1, "b": [true, "x"]}'
#     WHEN 2 THEN '{"a": 2}' WHEN 3 THEN '[1, "a"]' WHEN 4 THEN '"s"' END  -- each ::jsonb
#     FROM generate_series(1, 105) AS g
#   ANALYZE t (tags, doc)
STATISTICS_TERMS_TABLE = begin
  docs = { 1 => [:obj, [["a", [:num, "1"]], ["b", [:arr, [[:bool, true], [:str, "x"]]]]]],
           2 => [:obj, [["a", [:num, "2"]]]],
           3 => [:arr, [[:num, "1"], [:str, "a"]]],
           4 => [:str, "s"] }
  { name: "t",
    columns: [col("id", "i32", pk: true), col("tags", "i32[]"), col("doc", "jsonb")],
    rows: (1..105).map { |g| [g, [g, 0], docs[g]] },
    term_statistics_columns: [1, 2] }.freeze
end

# A table whose rows force a HEIGHT-2 tree (an interior node whose children are themselves
# interior nodes) at page_size 256. A wide text padding column makes each record ~66 bytes, so a
# leaf holds 3 records and the root interior overflows after ~5 leaves -> a two-level interior.
# 18 rows, ascending i32 PK. Exercises interior-of-interior child pointers + post-order
# page allocation across a deeper tree.
TALL_TREE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("pad", "text")],
  rows: (1..18).map { |i| [i, format("row-%02d-%s", i, "x" * 48)] }
}.freeze

# A table with a text column: exercises the value codec's text branch (u16 byte-length +
# UTF-8 bytes), the empty string (a distinct non-NULL value), an embedded quote, a 2-byte
# UTF-8 char (U+00E9), a NULL text value, and a 4-byte astral char (U+1F600). The PK is an
# i32 (text is not allowed in a key this slice). \u escapes keep this source ASCII-only.
TEXT_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("s", "text")],
  rows: [[1, "alice"], [2, ""], [3, "O'Brien"], [4, "caf\u{E9}"], [5, nil], [6, "\u{1F600}"]]
}.freeze

# A table with a bounded varchar(n) column beside an unbounded text column (v22 — the text-column
# u32 varchar_max_len typmod slot, spec/design/types.md §15). The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, code varchar(5), note text)
# Stored values are within the limit (a too-long value is rejected/truncated BEFORE the value codec,
# so it never reaches a golden); the bounded column pins varchar_max_len = 5, the unbounded one 0.
VARCHAR_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("code", "text", varchar_len: 5), col("note", "text")],
  rows: [[1, "alice", "hi"], [2, "ab", nil], [3, "", "long note text"]]
}.freeze

# A table with a boolean column: exercises the value codec's boolean branch (a single
# bool-byte, 0x00 false / 0x01 true) plus a NULL boolean (the tag alone). The PK is an i32
# (boolean as a value column; the boolean PRIMARY KEY case is BOOL_PK_TABLE below).
BOOL_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("flag", "boolean")],
  rows: [[1, true], [2, false], [3, nil]]
}.freeze

# A table with a boolean PRIMARY KEY (the second non-integer stored key after uuid — the
# bool-byte key encoding, encoding.md §2.9). The stored key is the bare 1-byte body (0x00 false /
# 0x01 true — a PK is NOT NULL, so no presence tag), pinning the cross-core boolean key bytes; rows
# are written in key (byte) order: false (0x00) then true (0x01). The nullable boolean value column
# `v` covers a present and a NULL value. The cores build this via
#   CREATE TABLE t (k boolean PRIMARY KEY, v boolean)
#   INSERT (false, true); INSERT (true, NULL)
BOOL_PK_TABLE = {
  name: "t",
  columns: [col("k", "boolean", pk: true), col("v", "boolean")],
  rows: [[false, true], [true, nil]]
}.freeze

# A table with a decimal column: exercises the value codec's decimal branch (flags + u16 scale
# + u16 ndigits + base-10^4 groups), positive/negative/zero, a multi-group coefficient, a NULL,
# AND the catalog typmod (an unconstrained `numeric` column `d` and a constrained numeric(10,2)
# column `m`). The `m` values are already at scale 2, so storing them is a no-op coercion — the
# stored bytes equal what the cores write when they INSERT the same literals. PK is an i32
# (decimal is not allowed in a key this slice).
DECIMAL_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("d", "decimal"), col("m", "decimal", precision: 10, scale: 2)],
  rows: [[1, "1.50", "1.50"], [2, "-12345.6789", "-12.34"], [3, "0.00", "0.00"],
         [4, "100000000.000001", "100.00"], [5, nil, nil]]
}.freeze

# A table with a bytea column: exercises the value codec's bytea branch (u16 byte-length +
# RAW bytes, no UTF-8 validation). Covers a multi-byte value with a-f hex (\xdeadbeef), the
# empty byte string (a distinct non-NULL value), embedded 0x00 bytes, a high byte (0xFF), a
# NULL bytea, and a lone 0x00 byte. The PK is an i32 (bytea is not allowed in a key this
# slice). All byte values are forced to ASCII-8BIT (.b) so they round-trip verbatim.
BYTEA_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("b", "bytea")],
  rows: [[1, "\xDE\xAD\xBE\xEF".b], [2, "".b], [3, "\x00\x01\x02".b],
         [4, "\xFF".b], [5, nil], [6, "\x00".b]]
}.freeze

# A table with a uuid PRIMARY KEY (the first golden with a NON-integer stored key — the
# load-bearing §8 cross-core key-path proof) plus a nullable uuid column. Exercises the value
# codec's fixed-16-byte uuid branch (no length prefix), the uuid key encoding (bare 16 bytes,
# uuid-raw16), a present and a NULL uuid value, and the nil/max boundary UUIDs. Rows are written
# in key (byte) order. The cores build this via `CREATE TABLE t (id uuid PRIMARY KEY, ref uuid)`.
UUID_TABLE = {
  name: "t",
  columns: [col("id", "uuid", pk: true), col("ref", "uuid")],
  rows: [["00000000-0000-0000-0000-000000000000", "550e8400-e29b-41d4-a716-446655440000"],
         ["550e8400-e29b-41d4-a716-446655440000", nil],
         ["f47ac10b-58cc-4372-a567-0e02b2c3d479", "00000000-0000-0000-0000-000000000000"],
         ["ffffffff-ffff-ffff-ffff-ffffffffffff", "ffffffff-ffff-ffff-ffff-ffffffffffff"]]
}.freeze

# A table with a text PRIMARY KEY (the first VARIABLE-WIDTH non-integer stored key — the
# text-terminated-escape encoding, encoding.md §2.4). The stored key is the bare terminated body
# (a PK is NOT NULL, so no presence tag), pinning the cross-core text key bytes incl. the empty
# string, uppercase-before-lowercase (C collation), and a 2-byte UTF-8 char. Rows are in key
# (code-point / byte) order: "" < "Zeta"(0x5A) < "apple"(0x61) < "banana"(0x62) < "é"(0xC3). The
# value column `v` is a nullable i32 (one NULL). The cores build this via
#   CREATE TABLE t (k text PRIMARY KEY, v i32); INSERT the rows.
TEXT_PK_TABLE = {
  name: "t",
  columns: [col("k", "text", pk: true), col("v", "i32")],
  rows: [["", 4], ["Zeta", nil], ["apple", 2], ["banana", 3], ["é", 5]]
}.freeze

# A table with a bytea PRIMARY KEY (the bytea-terminated-escape encoding, encoding.md §2.6) — like
# text but over raw bytes, so the embedded-0x00 escape (0x00 -> 0x00 0xFF) is exercised on disk.
# Rows are in unsigned-byte (key) order: "" < \x00 < \x61 < \x6100ff62 < \x6161 < \x62. All values
# are forced to ASCII-8BIT (.b). The cores build this via
#   CREATE TABLE t (k bytea PRIMARY KEY, v i32); INSERT the rows.
BYTEA_PK_TABLE = {
  name: "t",
  columns: [col("k", "bytea", pk: true), col("v", "i32")],
  rows: [["".b, 5], ["\x00".b, 6], ["\x61".b, 1], ["\x61\x00\xFF\x62".b, 4],
         ["\x61\x61".b, 2], ["\x62".b, 3]]
}.freeze

# A table with an (unconstrained) decimal PRIMARY KEY (the decimal-order-preserving encoding,
# encoding.md §2.5) — the first variable-width SIGNED key. Rows are in numeric (= key) order:
# -2.5 < -0.5 < 0 < 0.25 < 1.5 < 10 < 100.50, exercising the sign boundary, zero (the single
# class byte), a sub-1 fraction (negative decpt), odd/even decpt, and a trailing-zero value
# ("100.50" stores scale 2 in its VALUE body but normalizes in the KEY). All values are distinct
# (equal decimals would collide on the scale-independent key). The cores build this via
#   CREATE TABLE t (k decimal PRIMARY KEY, v i32); INSERT the rows.
DECIMAL_PK_TABLE = {
  name: "t",
  columns: [col("k", "decimal", pk: true), col("v", "i32")],
  rows: [["-2.5", 6], ["-0.5", 5], ["0", 4], ["0.25", 1], ["1.5", 2], ["10", 3], ["100.50", 7]]
}.freeze

# A table exercising the DEFAULT column constraint on disk (format.md): the catalog flags bit2
# + the column's pre-evaluated default value via the value codec, written AFTER the decimal
# typmod. Covers an int default, a text default, a DEFAULT NULL (the lone 0x01 tag), a NOT NULL
# column with a default (bit1 + bit2), a decimal default coerced to numeric(6,2), and a plain
# no-default column (bit2 off, no extra bytes). The stored defaults and row values are exactly
# what the cores write when they CREATE the table and INSERT (row 1 takes every default; row 2
# provides all values). PK is an i32.
DEFAULT_TABLE = {
  name: "t",
  columns: [
    col("id", "i32", pk: true),
    col("n", "i32", default: 0),
    col("note", "text", default: "none"),
    col("maybe", "i32", default: nil),
    col("req", "i32", not_null: true, default: 7),
    col("amt", "decimal", precision: 6, scale: 2, default: "1.50"),
    col("plain", "i16")
  ],
  rows: [[1, 0, "none", nil, 7, "1.50", nil],
         [2, 42, "hi", 5, 9, "2.00", 100]]
}.freeze

# A table with EXPRESSION column defaults (constraints.md §2, v8): the catalog flags bit3
# (default_is_expr) + the expr-text written AFTER the typmod, via the same token rendering a
# CHECK uses. Covers a `uuid DEFAULT uuidv7()` (text "uuidv7 ( )"), an `i32 DEFAULT 1 + 1`
# (text "1 + 1"), a CONSTANT default beside them (bit2, "k"), and a plain no-default column. An
# EMPTY table — the catalog encoding of expression defaults is the cross-core proof; the per-row
# evaluation (nondeterministic without a seed) is exercised by the conformance corpus instead.
# The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, g uuid DEFAULT uuidv7(), n i32 DEFAULT 1 + 1,
#                   k i32 DEFAULT 7, plain i16)
DEFAULT_EXPR_TABLE = {
  name: "t",
  columns: [
    col("id", "i32", pk: true),
    col("g", "uuid", default_expr: "uuidv7 ( )"),
    col("n", "i32", default_expr: "1 + 1"),
    col("k", "i32", default: 7),
    col("plain", "i16")
  ],
  rows: []
}.freeze

# A table with a timestamp column: exercises the value codec's timestamp branch (the i64
# microsecond instant, the same 8-byte int-be-signflip body as i64 — type code 8). Covers a
# positive instant (2024-01-01 12:00:00), a pre-1970 negative one (1969-12-31 23:59:59.5), a
# BC-era one (0001-01-01 00:00:00 BC), the -infinity/+infinity sentinels (i64::MIN/MAX), and a
# NULL. Values are the raw micros the cores compute from the corresponding literals. PK is i32.
TIMESTAMP_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("ts", "timestamp")],
  rows: [[1, 1_704_110_400_000_000], [2, -500_000], [3, -62_167_219_200_000_000],
         [4, -9_223_372_036_854_775_808], [5, 9_223_372_036_854_775_807], [6, nil]]
}.freeze

# A table with a timestamptz column (type code 10): same 8-byte i64 body. The +05 literal
# normalizes to UTC (12:00+05 -> 07:00Z -> 1_704_092_400_000_000).
TIMESTAMPTZ_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("ts", "timestamptz")],
  rows: [[1, 1_704_110_400_000_000], [2, 1_704_092_400_000_000], [3, -500_000],
         [4, -9_223_372_036_854_775_808], [5, 9_223_372_036_854_775_807], [6, nil]]
}.freeze

# A table with an interval column (type code 11): the fixed 16-byte branch (i32 months ‖ i32 days
# ‖ i64 micros, big-endian, no sign-flip). Covers a positive multi-field value
# ('1 mon 2 days 03:04:05'), a negative value ('-1 day'), the zero interval, a months-only value
# ('1 mon') vs a '30 days' value that is SPAN-EQUAL but byte-distinct, and a NULL. Each interval
# value is the [months, days, micros] triple the cores compute from the literal. PK is i32.
INTERVAL_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("d", "interval")],
  rows: [[1, [1, 2, 11_045_000_000]], [2, [0, -1, 0]], [3, [0, 0, 0]],
         [4, [1, 0, 0]], [5, [0, 30, 0]], [6, nil]]
}.freeze

# A table with an (unconstrained) interval PRIMARY KEY (the interval-span-i128 encoding, encoding.md
# §2.10) — the first FIXED-width SIGNED 16-byte key. Rows are in canonical-span (= key) order:
# -1 mon < -1 day < 0 < 1 sec < 1 day < 1 mon < 100 years, exercising the sign boundary, the zero
# interval, and the month/day/time fields. All spans are distinct (span-equal intervals like
# `1 mon`/`30 days` would collide on the span-independent key). The cores build this via
#   CREATE TABLE t (k interval PRIMARY KEY, v i32); INSERT the rows.
# Each k value is the [months, days, micros] triple.
INTERVAL_PK_TABLE = {
  name: "t",
  columns: [col("k", "interval", pk: true), col("v", "i32")],
  rows: [[[-1, 0, 0], 6], [[0, -1, 0], 5], [[0, 0, 0], 4], [[0, 0, 1_000_000], 1],
         [[0, 1, 0], 2], [[1, 0, 0], 3], [[1200, 0, 0], 7]]
}.freeze

# Timestamp precision and interval qualifier typmods (v43, spec/design/timestamp.md §8, interval.md
# §6.1): the temporal typmod slot on timestamp(3) / timestamptz(0) / interval day to second(2) /
# interval year / interval(4) columns and on a composite type's timestamp(1) / interval minute fields,
# each table beside an all-NULL row (three tables so each record fits a 256-byte page). Stored values
# are already rounded/cut. The cores build this via
#   CREATE TYPE stamp AS (at timestamp(1), d interval minute)
#   CREATE TABLE t (id i32 PRIMARY KEY, ts timestamp(3), tz timestamptz(0), iv interval day to second(2))
#   CREATE TABLE u (id i32 PRIMARY KEY, iy interval year, ip interval(4))
#   CREATE TABLE v (id i32 PRIMARY KEY, s stamp)
#   INSERT INTO t (1, '2024-01-01 12:00:00.1234', '2024-01-01 12:00:00.4+00', '1 day 02:03:04.567'),
#                 (2, NULL, NULL, NULL)
#   INSERT INTO u (1, '2 years 5 months', '1.23449'), (2, NULL, NULL)
#   INSERT INTO v (1, ROW('2024-01-01 12:00:00.05', '3 minutes 20 seconds')), (2, NULL)
TEMPORAL_TYPMOD_TABLE = {
  types: [ctype("stamp", [field("at", "timestamp", temporal: temporal(precision: 1)),
                          field("d", "interval", temporal: temporal(fields: 5))])],
  tables: [{ name: "t",
             columns: [col("id", "i32", pk: true),
                       col("ts", "timestamp", temporal: temporal(precision: 3)),
                       col("tz", "timestamptz", temporal: temporal(precision: 0)),
                       col("iv", "interval", temporal: temporal(fields: 10, precision: 2))],
             rows: [[1, 1_704_110_400_123_000, 1_704_110_400_000_000, [0, 1, 7_384_570_000]],
                    [2, nil, nil, nil]] },
           { name: "u",
             columns: [col("id", "i32", pk: true),
                       col("iy", "interval", temporal: temporal(fields: 1)),
                       col("ip", "interval", temporal: temporal(precision: 4))],
             rows: [[1, [24, 0, 0], [0, 0, 1_234_500]], [2, nil, nil]] },
           { name: "v",
             columns: [col("id", "i32", pk: true), col("s", "stamp")],
             rows: [[1, [1_704_110_400_100_000, [0, 0, 180_000_000]]], [2, nil]] }]
}.freeze

# A table with a date column (type code 16): exercises the value codec's date branch (the i32
# day count, the same 4-byte int-be-signflip body as i32). Covers a positive date (2024-01-15 ->
# day 19737), a pre-1970 negative one (1969-12-31 -> -1), a BC-era one (0044-03-15 BC -> astro -43
# -> day -735160), the -infinity/+infinity sentinels (i32::MIN/MAX), and a NULL. Values are the raw
# day counts the cores compute from the corresponding literals. PK is i32. (spec/design/date.md)
DATE_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("d", "date")],
  rows: [[1, 19_737], [2, -1], [3, -735_160], [4, -2_147_483_648], [5, 2_147_483_647], [6, nil]]
}.freeze

# A table with a f64 column (type code 12): exercises the value codec's 8-byte IEEE branch.
# Covers a positive fraction, a negative value, +0 and -0 (the sign bit is preserved on disk —
# distinct bytes 0x0000…/0x8000…), +Infinity, -Infinity, a canonicalized NaN (stored as the single
# quiet pattern 0x7FF8…000 regardless of source — float.md §10), a NULL, and Float::MAX (a full
# mantissa, bits 0x7FEFFFFFFFFFFFFF). The PK is an i32 so this fixture exercises the float VALUE
# codec in a nullable, non-key column (the float PRIMARY KEY form is the separate float64_pk_table.jed
# golden, §2.8). Values are the exact f64 the cores compute from the literals.
FLOAT64_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("d", "f64")],
  rows: [[1, 1.5], [2, -2.5], [3, 0.0], [4, -0.0], [5, Float::INFINITY],
         [6, -Float::INFINITY], [7, Float::NAN], [8, nil], [9, Float::MAX]]
}.freeze

# A table with a f32 column (type code 13): the 4-byte IEEE branch. Same special-value coverage
# as FLOAT64_TABLE (+0/-0 distinct on disk, ±Infinity, a canonicalized NaN → 0x7FC00000, NULL) plus
# 100.25 (exactly representable in binary32, bits 0x42C88000). Values are exactly representable in
# binary32 so the f64 fixture value equals the f32-widened decode. PK is i32 (the float PRIMARY KEY
# form is float32_pk_table.jed).
FLOAT32_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("r", "f32")],
  rows: [[1, 1.5], [2, -2.5], [3, 0.0], [4, -0.0], [5, Float::INFINITY],
         [6, -Float::INFINITY], [7, Float::NAN], [8, nil], [9, 100.25]]
}.freeze

# A table with a f64 PRIMARY KEY (the float-order-preserving key, ../design/encoding.md §2.8): the
# B-tree iterates float keys in the float TOTAL order (-Inf < finite < +Inf < NaN; -0 = +0). Pins
# that the executor encodes a float PK to the §2.8 bytes (canonicalized IEEE bits + the sign/all
# bit-flip), cross-core byte-identical. In-contract LITERAL values only (no transcendentals), so the
# image is deterministic across cores; the rows are listed out of key order to prove the generator
# sorts by the encoded float key. A PK is NOT NULL, so the key is the bare body (no presence tag).
FLOAT64_PK_TABLE = {
  name: "fk",
  columns: [col("k", "f64", pk: true), col("v", "i32")],
  rows: [[1.5, 1], [-Float::INFINITY, 2], [0.0, 3], [Float::NAN, 4],
         [-1.5, 5], [Float::INFINITY, 6]]
}.freeze

# A table with a f32 PRIMARY KEY (the 4-byte float-order-preserving key, §2.8). Same coverage as
# FLOAT64_PK_TABLE at single precision; values exactly representable in binary32.
FLOAT32_PK_TABLE = {
  name: "fk",
  columns: [col("k", "f32", pk: true), col("v", "i32")],
  rows: [[1.5, 1], [-Float::INFINITY, 2], [0.0, 3], [Float::NAN, 4],
         [-1.5, 5], [Float::INFINITY, 6]]
}.freeze

# Incompressible filler (format.md "Fixtures"): xorshift32(seed "JEDB") mapped to a 64-char
# alphabet (text) or raw bytes (bytea). High-entropy output has no 4-byte repeats, so the LZ4
# encoder never wins store-smaller and the value deterministically stays PLAIN. Mirrored in the
# cores' golden/cost tests. Each call restarts at the seed (one fixed stream per length).
ALPHA64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
FILLER_SEED = 0x4A454442

def filler_step(x)
  x ^= (x << 13) & 0xFFFFFFFF
  x ^= x >> 17
  (x ^ ((x << 5) & 0xFFFFFFFF)) & 0xFFFFFFFF
end

def filler_text(n)
  x = FILLER_SEED
  out = +""
  n.times do
    x = filler_step(x)
    out << ALPHA64[x % 64]
  end
  out
end

def filler_bytes(n)
  x = FILLER_SEED
  out = +"".b
  n.times do
    x = filler_step(x)
    out << (x % 256).chr
  end
  out
end

# A table with large INCOMPRESSIBLE text + bytea values that must spill OUT-OF-LINE PLAIN to
# overflow pages (large-values.md §12). At page_size 256 the per-record cap is
# RECORD_MAX = (256-16-12)/2 = 114 (v7), so a value of ~600/300 bytes exceeds it: compression is
# attempted first (Slice B) but rejected by store-smaller (the filler is high-entropy), so the
# record holds a fixed 0x02 pointer (u32 first_page + u32 len) and the raw bytes live in a chain
# of page_type-4 slabs (240 bytes each). Row 1's text (600 B → 3 slabs) and bytea (300 B → 2
# slabs) both spill; row 2's values stay inline; row 3 is NULL/NULL. Exercises multi-page chains,
# multi-column spill, and the inline+external mix in one leaf. The PK stays i32.
OVERFLOW_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("body", "text"), col("blob", "bytea")],
  rows: [[1, filler_text(600), filler_bytes(300)], [2, "small", ["cafe"].pack("H*")], [3, nil, nil]]
}.freeze

# A table with large COMPRESSIBLE values exercising Slice B's forms (large-values.md §13,
# format.md "Large values", lz4.md). At page_size 256 (RECORD_MAX 114, C = 240, v7):
# row 1's 600-char "x" run compresses to a few bytes → 0x03 inline-compressed text — and its
# 200-byte 0xAB bytea run → 0x03 inline-compressed bytea (two compressed values, one record);
# row 2's 400-char half-filler/half-run text compresses to ~200 B — smaller than plain but still
# over RECORD_MAX → 0x04 external-compressed (a chain carrying the COMPRESSED block);
# row 3 stays fully inline-plain; row 4 is NULL/NULL. PK i32.
COMPRESSED_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("body", "text"), col("blob", "bytea")],
  rows: [[1, "x" * 600, (["ab"].pack("H*") * 200)],
         [2, filler_text(200) + ("y" * 200), nil],
         [3, "tiny", ["cafe"].pack("H*")],
         [4, nil, nil]]
}.freeze

# A table with SECONDARY INDEXES (v5 — indexes.md): pins the catalog reshape and the index
# trees. The PK list order (b, a) DIFFERS from declaration order (the lifted composite-PK
# narrowing — pk ordinals [1, 0]); index `i_u` covers a NULLABLE uuid column holding a NULL
# (the encoding.md §2.2 presence tag in stored index order — present values first, NULL
# last), and `t_a_b_idx` is the auto-named two-column index. Index records have EMPTY
# payloads (key only). Indexes listed in ascending lowercased-name order (the catalog
# order). The cores build this via
#   CREATE TABLE t (a i32, b i32, u uuid, PRIMARY KEY (b, a));
#   CREATE INDEX i_u ON t (u);  CREATE INDEX ON t (a, b);
# and insert rows in ascending storage-key order.
INDEX_TABLE = {
  name: "t",
  columns: [col("a", "i32", pk: true), col("b", "i32", pk: true), col("u", "uuid")],
  pk_order: [1, 0], # PRIMARY KEY (b, a)
  indexes: [
    { name: "i_u", cols: [2] },
    { name: "t_a_b_idx", cols: [0, 1] }
  ],
  rows: [[1, 10, "550e8400-e29b-41d4-a716-446655440000"],
         [2, 10, nil],
         [3, 20, "00000000-0000-0000-0000-000000000000"]]
}.freeze

# Degenerate interior fan-out (format.md "Interior node" / "Fan-out"; bplus-reshape.md §4.2): a
# secondary index over near-RECORD_MAX(0) text keys. Each entry key is 112 bytes (nullable-slot
# tag + the terminated 105-char text + the 4-byte i32 storage key ≤ RECORD_MAX(0) = 114), so an
# index leaf holds two entries, and two separators overflow an interior page (8·2 + 4 + 2·112 =
# 244 > C = 240): the second leaf split forces the pinned `N = 2 → m = 1` interior split, leaving
# a legal **N = 0 interior node** on disk under a 1-separator root. The table rows themselves
# externalize their text (the 116-byte inline record exceeds RECORD_MAX(2) = 98; filler64 is
# incompressible, so store-smaller rejects compression → external-plain chains), so the fixture
# also pins long-key index entries over spilled values. Values share the incompressible filler64
# tail behind a distinct leading letter, so keys are distinct and sort by that letter.
MAX_SEP_TABLE = {
  name: "m",
  columns: [col("id", "i32", pk: true), col("s", "text")],
  indexes: [{ name: "i_s", cols: [1] }],
  rows: (0...6).map { |i| [i + 1, ("A".ord + i).chr + filler_text(104)] }
}.freeze

# A table with UNIQUE indexes (v6 — indexes.md §8, constraints.md §5): pins the per-index
# flags byte. `t_v_key` is the UNIQUE constraint's auto-named index over a NULLABLE column
# holding two NULLs (NULLS DISTINCT — both entries stored, side by side after the present
# value); `wv` is a named two-column UNIQUE constraint; `uq` is a CREATE UNIQUE INDEX; `nu`
# is a plain index over the same column as `t_v_key` (flags 0 beside flags 1). The cores
# build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32,
#                   UNIQUE (v), CONSTRAINT wv UNIQUE (w, v));
#   CREATE INDEX nu ON t (v);  CREATE UNIQUE INDEX uq ON t (w);
UNIQUE_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("v", "i32"), col("w", "i32")],
  indexes: [
    { name: "nu", cols: [1] },
    { name: "t_v_key", cols: [1], unique: true, constraint: true },
    { name: "uq", cols: [2], unique: true },
    { name: "wv", cols: [2, 1], unique: true, constraint: true }
  ],
  rows: [[1, 10, 100], [2, nil, 200], [3, nil, 300]]
}.freeze

# A table with EXPRESSION index keys (v26 — indexes.md §1/§6): pins the per-index key-element
# encoding — a plain column ordinal (`t_email_idx`, cols [1]) beside an expression element (the
# 0xFFFF sentinel + the canonical text "lower ( email )" for the UNIQUE `t_lower_idx`). The table
# is EMPTY (no rows), so both index trees are empty (root 0) and no per-row evaluation is needed —
# the fixture isolates the v26 catalog change (the entry bytes are covered by text_pk/collation
# fixtures). Indexes in ascending lowercased-name order. The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, email text);
#   CREATE INDEX ON t (email);  CREATE UNIQUE INDEX ON t (lower(email));
EXPR_INDEX_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("email", "text")],
  indexes: [
    { name: "t_email_idx", cols: [1] },
    { name: "t_lower_idx", cols: [{ expr: "lower ( email )" }], unique: true }
  ],
  rows: []
}.freeze

# A table with PARTIAL index predicates (v27 — indexes.md §9): pins the index_flags bit1 + the
# canonical predicate text after index_root_page. A plain partial index (`t_amt_idx`, cols [2],
# WHERE status = 'active') beside a UNIQUE partial index (`t_uact`, cols [2], unique, same predicate)
# and a NON-partial index (`t_status_idx`, cols [1], bit1 clear — byte-identical to v26). The table is
# EMPTY (no rows), so all three trees are empty (root 0) — the fixture isolates the v27 catalog change.
# Indexes in ascending lowercased-name order (t_amt_idx < t_status_idx < t_uact). The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, status text, amt i32);
#   CREATE INDEX ON t (amt) WHERE status = 'active';
#   CREATE UNIQUE INDEX t_uact ON t (amt) WHERE status = 'active';
#   CREATE INDEX ON t (status);
# A table with ORDERED index keys (v46 — indexes.md §11, encoding.md §2.16): pins index_flags bit4 +
# the per-element option bytes and the ordered entry slots. `t_v_desc` is DESC (NULLS FIRST by
# default: present tag 0xFF + the inverted body, NULL 0xFE) over a variable-width text column — the
# inversion of a terminated body; `t_n_nf` is ASC NULLS FIRST (NULL 0x00, present 0x01 + the body);
# `t_mix` mixes DESC NULLS LAST (present 0xFE + inverted body, NULL 0xFF) on a decimal with a
# default ASC element (the option list still names it); `t_plain` is an ordinary index beside them
# (no option bytes). The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, v text, n i32, d decimal)
#   CREATE INDEX t_v_desc ON t (v DESC);  CREATE INDEX t_n_nf ON t (n NULLS FIRST)
#   CREATE UNIQUE INDEX t_mix ON t (d DESC NULLS LAST, n);  CREATE INDEX t_plain ON t (n)
#   INSERT (1, 'b', 10, 1.5); (2, NULL, NULL, NULL); (3, 'a', 20, -2); (4, 'ab', NULL, 100)
ORDERED_INDEX_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("v", "text"), col("n", "i32"), col("d", "decimal")],
  indexes: [
    { name: "t_mix", cols: [3, 2], unique: true, orders: %i[desc_nulls_last asc] },
    { name: "t_n_nf", cols: [2], orders: [:asc_nulls_first] },
    { name: "t_plain", cols: [2] },
    { name: "t_v_desc", cols: [1], orders: [:desc] }
  ],
  rows: [[1, "b", 10, "1.5"], [2, nil, nil, nil], [3, "a", 20, "-2"], [4, "ab", nil, "100"]]
}.freeze

PARTIAL_INDEX_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("status", "text"), col("amt", "i32")],
  indexes: [
    { name: "t_amt_idx", cols: [2], predicate: "status = 'active'" },
    { name: "t_status_idx", cols: [1] },
    { name: "t_uact", cols: [2], unique: true, predicate: "status = 'active'" }
  ],
  rows: []
}.freeze

# A table with ARRAY (T[]) columns (v10 — spec/design/array.md): pins the catalog array-column
# entry (type_code 15 + the element-type descriptor, §3) and the compact value body (§4). Two
# array columns — an i32[] (fixed-width elements: NO per-element length prefix) and a text[]
# (variable-width). Row 1 is a plain int + text array; row 2 has an EMPTY array (ndim=0) and an
# empty text array; row 3 has a NULL element (the HAS_NULLS bitmap branch) and a whole-value NULL
# array (the lone 0x01 tag — distinct from an array OF a NULL element). Row 4 pins the §12 shapes:
# a 2-D i32[] (ndim=2, dims [2,2]) and a custom-lower-bound text[] ([2:3], so the lb i32 field is
# exercised). The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, xs i32[], tags text[])
#   INSERT (1, ARRAY[10,20,30], ARRAY['a','b']); (2, '{40,50}', '{}'); (3, ARRAY[1,NULL,3], NULL)
#   INSERT (4, ARRAY[ARRAY[10,20],ARRAY[30,40]], '[2:3]={x,y}')
# A table with a HOST-FUNCTION index dependency (v31 — extensibility.md §8.1): pins the index_flags
# bit2 has_host_deps + the persisted dependency list (name + signature + component id + semantic
# version) after index_root_page. The index `t_geo_idx` is on the expression `geo_hash(a)`, where
# geo_hash is a host scalar function (i64 -> i64), component "com.example/geo_hash" at semantic
# version 1. The table is EMPTY, so the tree is empty (root 0) — the fixture isolates the v31 catalog
# change. The cores build this with a registered `geo_hash` host function (Immutable + that component
# id + version) via
#   CREATE TABLE t (id i64 PRIMARY KEY, a i64)
#   CREATE INDEX t_geo_idx ON t (geo_hash(a))
# A v32 expression + partial index pins two exact names, including an alias, in sorted order.
TIMEZONE_INDEX_TABLE = {
  name: "t", columns: [col("id", "i32", pk: true), col("ts", "timestamptz")],
  indexes: [{ name: "t_zone_idx", cols: [{expr: "ts at time zone 'US/Eastern'"}, {expr: "ts at time zone 'America/New_York'"}],
    predicate: "ts is not null", timezone_deps: { dynamic: false,
      zones: %w[America/New_York US/Eastern].map { |name| { name: name, version: "2026a",
        checksum: Zlib.crc32(File.binread(File.join(__dir__, "../tz/2026a/zones/America/New_York"))) } } } }],
  rows: []
}.freeze

# A table with a HOST-FUNCTION check dependency (v35 — extensibility.md §7.1.3): pins the per-check
# check_flags byte both clear (t_a_check, no host call) and set (t_a_check1, which calls the host
# function geo_hash (i64 -> i64), component "com.example/geo_hash" at semantic version 1), and the
# dependency list that follows it. EMPTY table — the fixture isolates the v35 catalog change. The
# cores build this with a registered `geo_hash` host function via
#   CREATE TABLE t (id i64 PRIMARY KEY, a i64 CHECK (a <> 0), CHECK (geo_hash(a) > 0))
HOSTFUNC_CHECK_TABLE = {
  name: "t",
  columns: [col("id", "i64", pk: true), col("a", "i64")],
  checks: [
    { name: "t_a_check", expr: "a <> 0" },
    { name: "t_a_check1", expr: "geo_hash ( a ) > 0",
      host_deps: [{ name: "geo_hash", arg_types: [3], result: 3,
                    component_id: "com.example/geo_hash", semantic_version: 1 }] }
  ],
  rows: []
}.freeze

HOSTFUNC_INDEX_TABLE = {
  name: "t",
  columns: [col("id", "i64", pk: true), col("a", "i64")],
  indexes: [
    { name: "t_geo_idx", cols: [{ expr: "geo_hash ( a )" }],
      host_deps: [{ name: "geo_hash", arg_types: [3], result: 3,
                    component_id: "com.example/geo_hash", semantic_version: 1 }] }
  ],
  rows: []
}.freeze

ARRAY_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("xs", "i32[]"), col("tags", "text[]")],
  rows: [[1, [10, 20, 30], %w[a b]],
         [2, [40, 50], []],
         [3, [1, nil, 3], nil],
         [4, { dims: [2, 2], lbounds: [1, 1], elements: [10, 20, 30, 40] },
          { dims: [2], lbounds: [2], elements: %w[x y] }]]
}.freeze

# A table with RANGE columns (v15 — spec/design/ranges.md): pins the catalog range-column entry
# (type_code 17 + the one-byte element descriptor, §3) and the compact value body (the flags byte +
# present bound bodies, §4). Two discrete range columns — an i32range (element code 2) and an
# i64range (element code 3) — over fixed-width bounds. The rows exercise every flags bit and the
# canonical-`[)` storage: row 1 a plain finite `[)`; row 2 an inclusive-upper literal that
# canonicalizes (`[1,5]` → `[1,6)`) and a NULL range (the lone 0x01 tag); row 3 the EMPTY range (lone
# 0x01 flags) and an infinite-lower range (LB_INF); row 4 a both-infinite range (LB_INF|UB_INF, no
# bound bodies) and an exclusive-lower literal that canonicalizes with an infinite upper (`(5,)` →
# `[6,)`, LB_INC|UB_INF); row 5 a NULL range and a singleton that canonicalizes (`[1,1]` → `[1,2)`).
# The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range)
#   INSERT (1,'[1,5)','[10,20)'); (2,'[1,5]',NULL); (3,'empty','(,100)')
#   INSERT (4,'(,)','(5,)'); (5,NULL,'[1,1]')
RANGE_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("r", "i32range"), col("br", "i64range")],
  rows: [[1, { lower: 1, upper: 5, lower_inc: true, upper_inc: false },
          { lower: 10, upper: 20, lower_inc: true, upper_inc: false }],
         [2, { lower: 1, upper: 6, lower_inc: true, upper_inc: false }, nil],
         [3, :empty, { lower: nil, upper: 100, lower_inc: false, upper_inc: false }],
         [4, { lower: nil, upper: nil, lower_inc: false, upper_inc: false },
          { lower: 6, upper: nil, lower_inc: true, upper_inc: false }],
         [5, nil, { lower: 1, upper: 2, lower_inc: true, upper_inc: false }]]
}.freeze

# A table with an i32range PRIMARY KEY (the range-PK slice — range is the first CONTAINER key,
# encoding.md §2.11). Pins the recursive range-bounds key in the key slot: the empty range (key
# 0x00), an unbounded-lower range, a fully-unbounded range, and finite-bound ranges — listed in
# ASCENDING range_total_cmp (= byte) order (the builder inserts in key order). The `r` value bodies
# travel through the range value codec too (type_code 17). PK is the range; `v` is an ordinary i32.
RANGE_PK_TABLE = {
  name: "t",
  columns: [col("k", "i32range", pk: true), col("v", "i32")],
  rows: [[:empty, 0],
         [{ lower: nil, upper: 5, lower_inc: false, upper_inc: false }, 1],   # (,5)
         [{ lower: nil, upper: nil, lower_inc: false, upper_inc: false }, 2], # (,)
         [{ lower: 1, upper: 5, lower_inc: true, upper_inc: false }, 3],      # [1,5)
         [{ lower: 2, upper: 4, lower_inc: true, upper_inc: false }, 4],      # [2,4)
         [{ lower: 2, upper: nil, lower_inc: true, upper_inc: false }, 5]]    # [2,)
}.freeze

# A table with an i32[] PRIMARY KEY (the array-PK slice — array is the SECOND container key and the
# first whose key length varies with the element count; encoding.md §2.14, array-elements-terminated).
# Pins the recursive array key in the key slot: the empty array (key 0x00 0x00), shorter-prefix
# arrays, a value with a NULL element (the 0x02 NULL marker, sorting after present elements), all in
# ASCENDING array_total_cmp (= byte) order (the builder inserts in key order). The `key` value bodies
# travel through the array value codec too (type_code 15). PK is the array; `v` is an ordinary i32.
# The cores build this via
#   CREATE TABLE k (key i32[] PRIMARY KEY, v i32)
#   INSERT ('{}',40); ('{1,2}',20); ('{1,2,3}',10); ('{1,NULL}',50); ('{2}',60)
ARRAY_PK_TABLE = {
  name: "k",
  columns: [col("key", "i32[]", pk: true), col("v", "i32")],
  rows: [[[], 40],          # {} (empty array sorts first)
         [[1, 2], 20],      # {1,2}
         [[1, 2, 3], 10],   # {1,2,3} (shorter-prefix before)
         [[1, nil], 50],    # {1,NULL} (NULL element sorts after every present element)
         [[2], 60]]         # {2} (a larger first element sorts last)
}.freeze

# A table with a json column (v19 — spec/design/json.md §4): pins the catalog json-column entry
# (type_code 18, a plain scalar — no extra descriptor) and the VERBATIM text body, length-prefixed
# exactly like text. The stored bytes are the input text exactly (whitespace + key order preserved).
# The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, j json)
#   INSERT (1, '{"a": 1}'); (2, '[1, 2, 3]'); (3, NULL)
JSON_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("j", "json")],
  rows: [[1, '{"a": 1}'],
         [2, "[1, 2, 3]"],
         [3, nil]]
}.freeze

# A table with a jsonb column (v19 — spec/design/json.md §2): pins the catalog jsonb-column entry
# (type_code 19, a plain scalar — no extra descriptor) and the self-delimiting tagged-node value
# body. The rows exercise every node tag: row 1 an OBJECT (canonical key order a,b) with a NUMBER and
# a nested ARRAY of a boolean TRUE + JSON NULL; row 2 a bare STRING; row 3 a bare NUMBER; row 4 a SQL
# NULL (the lone 0x01 tag, distinct from a JSON null node). The fixture jsonb value is the
# pre-canonicalized tagged node tree. The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)
#   INSERT (1, '{"a": 1, "b": [true, null]}'); (2, '"hello"'); (3, '42'); (4, NULL)
JSONB_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("j", "jsonb")],
  rows: [[1, [:obj, [["a", [:num, "1"]], ["b", [:arr, [[:bool, true], [:null]]]]]]],
         [2, [:str, "hello"]],
         [3, [:num, "42"]],
         [4, nil]]
}.freeze

# A table with a GIN inverted index (v13 — spec/design/gin.md): pins the per-index index_kind byte
# (0 = ordered B-tree, 1 = GIN) and a GIN index tree (entries are encode(element)‖storage-key, empty
# payload — §4). `i_nums_gin` is a USING gin index over an i32[] column; `i_n` is an ordinary
# ordered index over a scalar column in the same catalog (kind 0 beside kind 1 — a btree index
# cannot sit on the array column). Rows exercise term DEDUP (row 2's duplicate 20 → one entry), an
# EMPTY array and a NULL whole-value array (rows 3/4 → no GIN entries), and a NULL element (row 5 →
# the null is dropped, terms {10,50}). The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, nums i32[], n i32)
#   INSERT (1,'{10,20,30}',1); (2,'{20,20,40}',2); (3,'{}',3); (4,NULL,4); (5,'{10,NULL,50}',5)
#   CREATE INDEX i_n ON t (n);  CREATE INDEX i_nums_gin ON t USING gin (nums)
GIN_ARRAY_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("nums", "i32[]"), col("n", "i32")],
  indexes: [
    { name: "i_n", cols: [2] },
    { name: "i_nums_gin", cols: [1], kind: "gin" }
  ],
  rows: [[1, [10, 20, 30], 1],
         [2, [20, 20, 40], 2],
         [3, [], 3],
         [4, nil, 4],
         [5, [10, nil, 50], 5]]
}.freeze

# A GIN index over a uuid[] column (the non-integer GIN element-type golden, spec/design/gin.md §3/§4):
# each GIN term is the element's 16-byte uuid-raw16 key encoding, so the index entries are
# encode_uuid(term) ‖ storage_key (empty payload) — pinning that a uuid-element GIN serializes
# byte-identically across cores. The shape mirrors GIN_ARRAY_TABLE (an i_n ordered index beside the
# GIN; term dedup in row 2's duplicate bb; an empty + a NULL whole-value array in rows 3/4; a NULL
# element in row 5), with uuid terms in place of integers. No format_version bump (uuid is a
# fixed-width key encoding already on disk).
GIN_UUID_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("tags", "uuid[]"), col("n", "i32")],
  indexes: [
    { name: "i_n", cols: [2] },
    { name: "i_tags_gin", cols: [1], kind: "gin" }
  ],
  rows: [[1, ["00000000-0000-0000-0000-0000000000aa", "00000000-0000-0000-0000-0000000000bb", "00000000-0000-0000-0000-0000000000cc"], 1],
         [2, ["00000000-0000-0000-0000-0000000000bb", "00000000-0000-0000-0000-0000000000bb", "00000000-0000-0000-0000-0000000000dd"], 2],
         [3, [], 3],
         [4, nil, 4],
         [5, ["00000000-0000-0000-0000-0000000000aa", nil, "00000000-0000-0000-0000-0000000000ee"], 5]]
}.freeze

# A table with a GiST index over an i32range column (v20 — spec/design/gist.md GX1). Pins the
# per-index index_kind = 2 byte AND a persisted R-tree (page types 5 = GiST leaf, 6 = GiST interior):
# 6 bounded canonical [) ranges + one empty range (7 leaf entries) force one median split at
# GIST_FANOUT = 4, so the tree is two levels (an interior root over two leaves) — exercising BOTH
# page types, the leaf entry layout (bound_len ‖ encode_range_body ‖ skey_len ‖ skey), the interior
# entry layout (union bound ‖ child_page), least-enlargement choose-subtree (the 6th/7th inserts), and
# post-order page allocation. Row 8's NULL range is NOT indexed (no leaf entry). The cores build this
# via
#   CREATE TABLE t (id i32 PRIMARY KEY, r i32range)
#   INSERT (1,'[1,5)'); (2,'[10,20)'); (3,'[3,8)'); (4,'[100,200)'); (5,'[50,60)'); (6,'[15,25)');
#          (7,'empty'); (8,NULL)
#   CREATE INDEX t_r_gist ON t USING gist (r)
def rng(lo, hi) = { lower: lo, upper: hi, lower_inc: true, upper_inc: false }
GIST_RANGE_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("r", "i32range")],
  indexes: [{ name: "t_r_gist", cols: [1], kind: "gist" }],
  rows: [[1, rng(1, 5)], [2, rng(10, 20)], [3, rng(3, 8)], [4, rng(100, 200)],
         [5, rng(50, 60)], [6, rng(15, 25)], [7, :empty], [8, nil]]
}.freeze

# A GiST index over a SCALAR i32 column — the scalar `=` opclass (v20 — spec/design/gist.md GX2).
# Pins the index_kind = 2 byte AND a persisted R-tree whose bounds are `[min,max]` over the
# order-preserving KEY encoding (NOT a range body — distinguished from GIST_RANGE_TABLE only by the
# indexed column's catalog type). 8 rows with duplicate room numbers (> GIST_FANOUT = 4) force a
# median split, so the tree is two levels, exercising both page types + the scalar bound codec. Row
# 9's NULL room is not indexed. The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, room i32)
#   INSERT INTO t VALUES (1,10),(2,20),(3,10),(4,30),(5,20),(6,40),(7,10),(8,50),(9,NULL)
#   CREATE INDEX t_room_gist ON t USING gist (room)
GIST_SCALAR_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("room", "i32")],
  indexes: [{ name: "t_room_gist", cols: [1], kind: "gist" }],
  rows: [[1, 10], [2, 20], [3, 10], [4, 30], [5, 20], [6, 40], [7, 10], [8, 50], [9, nil]]
}.freeze

# An EXCLUDE constraint (v21 — spec/design/gist.md §7/§8, GX3): pins the per-table exclusion catalog
# list (name + backing GiST index name + the (column, operator) element vector) AND a MULTI-COLUMN
# GiST index whose leaf bound is a scalar `[min,max]` room component concatenated with a range during
# component. The backing index `booking_room_during_excl` (cols [1, 2], kind gist) enforces
# `EXCLUDE USING gist (room WITH =, during WITH &&)`. 7 indexed rows (> GIST_FANOUT = 4) force a
# median split, so the R-tree is two levels, exercising both page types with a two-component bound.
# Row 8's NULL room is exempt (not indexed). The cores build this via
#   CREATE TABLE booking (id i32 PRIMARY KEY, room i32, during i32range,
#     EXCLUDE USING gist (room WITH =, during WITH &&))
#   INSERT INTO booking VALUES (1,101,'[10,20)'),(2,101,'[20,30)'),(3,102,'[10,20)'),
#     (4,102,'[30,40)'),(5,103,'[10,20)'),(6,104,'[50,60)'),(7,105,'[1,5)'),(8,NULL,'[10,20)')
GIST_EXCLUDE_TABLE = {
  name: "booking",
  columns: [col("id", "i32", pk: true), col("room", "i32"), col("during", "i32range")],
  indexes: [{ name: "booking_room_during_excl", cols: [1, 2], kind: "gist" }],
  exclusions: [{ name: "booking_room_during_excl", index: "booking_room_during_excl",
                 elements: [[1, "="], [2, "&&"]] }],
  rows: [[1, 101, rng(10, 20)], [2, 101, rng(20, 30)], [3, 102, rng(10, 20)],
         [4, 102, rng(30, 40)], [5, 103, rng(10, 20)], [6, 104, rng(50, 60)],
         [7, 105, rng(1, 5)], [8, nil, rng(10, 20)]]
}.freeze

# The v45 exclusion follow-ons (spec/design/gist.md §7.1/§7.2): an `EXCLUDE USING btree (a WITH =,
# c WITH =)` whose backing index `t_a_c_excl` is an ORDERED, NON-unique two-column B-tree (index_kind
# 0, index_flags 0 — every row has an entry, NULL slots included) beside a partial `EXCLUDE USING gist
# (span WITH &&) WHERE (b > 0)` whose backing GiST index `t_span_excl` carries the v27 predicate bit +
# text and holds only the rows with b > 0 (rows 1 and 4). The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, a i32, c text, span i32range, b i32,
#     EXCLUDE USING btree (a WITH =, c WITH =), EXCLUDE USING gist (span WITH &&) WHERE (b > 0))
#   INSERT INTO t VALUES (1, 1, 'x', '[1,5)', 1), (2, 1, 'y', '[1,5)', 0), (3, 2, 'x', '[3,8)', NULL),
#     (4, NULL, 'x', '[10,20)', 2), (5, 1, NULL, '[2,4)', -1)
EXCLUDE_FOLLOWON_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("a", "i32"), col("c", "text"), col("span", "i32range"),
            col("b", "i32")],
  indexes: [{ name: "t_a_c_excl", cols: [1, 2] },
            { name: "t_span_excl", cols: [3], kind: "gist", predicate: "b > 0",
              where: ->(row) { !row[4].nil? && row[4] > 0 } }],
  exclusions: [{ name: "t_a_c_excl", index: "t_a_c_excl", elements: [[1, "="], [2, "="]] },
               { name: "t_span_excl", index: "t_span_excl", elements: [[3, "&&"]] }],
  rows: [[1, 1, "x", rng(1, 5), 1], [2, 1, "y", rng(1, 5), 0], [3, 2, "x", rng(3, 8), nil],
         [4, nil, "x", rng(10, 20), 2], [5, 1, nil, rng(2, 4), -1]]
}.freeze

# GIN indexes over f64[] and f32[] columns (the float GIN element golden, spec/design/gin.md §2/§4):
# each GIN term is the element's canonicalizing float-order-preserving key (encoding.md §2.8), so
# -0 and +0 share a term (row 1's tags), every NaN is ONE term (row 2's two NaNs; row 2's r), and
# the 8-byte (f64) / 4-byte (f32) terms self-delimit before the storage key. The stored VALUES keep
# -0's sign bit (row 1) — only the index terms canonicalize. Rows 3/4 hold an empty and a NULL
# whole-value array (no entries) and row 5 NULL elements + a duplicate. In-contract literals only.
GIN_FLOAT_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("tags", "f64[]"), col("r", "f32[]"), col("n", "i32")],
  indexes: [
    { name: "i_n", cols: [3] },
    { name: "i_r_gin", cols: [2], kind: "gin" },
    { name: "i_tags_gin", cols: [1], kind: "gin" }
  ],
  rows: [[1, [1.5, -0.0, 0.0], [-0.0, 0.5], 1],
         [2, [Float::NAN, Float::INFINITY, Float::NAN], [Float::NAN], 2],
         [3, [], nil, 3],
         [4, nil, [], 4],
         [5, [-Float::INFINITY, nil, -2.5], [100.25, nil, 100.25], 5]]
}.freeze

# A GIN index over a text[] column (the text GIN element golden, spec/design/gin.md §4.2): each term
# is the element's C text key (text-terminated-escape over its UTF-8). Row 1 holds a duplicate (one
# entry); row 2 the empty string, a two-byte character, and a NULL element (no term); rows 3/4 an
# empty and a NULL whole-value array (no entries); row 5 an element with a space beside row 1's
# "red" (one term in two rows). The over-128-byte cut cannot fit this page's record limit; the
# query/gin_text_elements.test costs pin it cross-core. The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, tags text[], n i32)
#   INSERT (1, '{red,blue,red}', 1), (2, '{"",é,NULL}', 2), (3, '{}', 3), (4, NULL, 4),
#          (5, '{"a b",red}', 5)
#   CREATE INDEX i_n ON t (n);  CREATE INDEX i_tags_gin ON t USING gin (tags)
GIN_TEXT_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("tags", "text[]"), col("n", "i32")],
  indexes: [
    { name: "i_n", cols: [2] },
    { name: "i_tags_gin", cols: [1], kind: "gin" }
  ],
  rows: [[1, %w[red blue red], 1],
         [2, ["", "é", nil], 2],
         [3, [], 3],
         [4, nil, 4],
         [5, ["a b", "red"], 5]]
}.freeze

# GIN indexes over a decimal[] and a bytea[] column (the decimal / bytea GIN element golden,
# spec/design/gin.md §4.3): a decimal term is the element's value-based decimal-order-preserving key,
# a bytea term its bytea-terminated-escape key. Row 1 holds 1.5 and 1.50 (ONE entry; the stored values
# keep their scales) and a duplicate bytea (one entry); row 2 a negative, a zero at scale 2, an embedded
# 0x00 byte, the empty bytea, and NULL elements (no term); rows 3/4 empty and NULL whole-value arrays
# (no entries); row 5 shares row 1's 1.5 and has a bytea of two zero bytes. The over-128-byte cut cannot
# fit this page's record limit; the query/gin_decimal_elements.test and query/gin_bytea_elements.test
# costs pin it cross-core. The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, d decimal[], b bytea[], n i32)
#   INSERT (1, '{1.5,2,1.50}', ARRAY['\x61'::bytea, '\x61'::bytea], 1),
#          (2, '{-7,0.00,NULL}', ARRAY['\x00ff'::bytea, '\x'::bytea, NULL], 2),
#          (3, '{}', '{}', 3), (4, NULL, NULL, 4), (5, '{100,1.5}', ARRAY['\x0000'::bytea], 5)
#   CREATE INDEX i_n ON t (n);  CREATE INDEX i_d_gin ON t USING gin (d)
#   CREATE INDEX i_b_gin ON t USING gin (b)
GIN_DECIMAL_BYTEA_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("d", "decimal[]"), col("b", "bytea[]"), col("n", "i32")],
  indexes: [
    { name: "i_b_gin", cols: [2], kind: "gin" },
    { name: "i_d_gin", cols: [1], kind: "gin" },
    { name: "i_n", cols: [3] }
  ],
  rows: [[1, %w[1.5 2 1.50], ["a", "a"], 1],
         [2, ["-7", "0.00", nil], ["\x00\xff".b, "".b, nil], 2],
         [3, [], [], 3],
         [4, nil, nil, 4],
         [5, %w[100 1.5], ["\x00\x00".b], 5]]
}.freeze

# A GIN index over a jsonb column (the jsonb_ops golden, spec/design/gin.md §2.1/§4.1): row 1 holds
# keys at two levels, an array string element (a KEY term) beside a member string value (a STRING
# term), a boolean and a JSON null; row 2 a top-level array whose 1.5 and 1.50 are ONE number term
# and whose "x" duplicates row 1's key term "x" (one entry per row); row 3 a top-level string (a KEY
# term); rows 4/5 `{}` and a SQL NULL (no entries); row 6 a negative number. The truncated-term form
# (over 128 bytes) cannot fit this 256-byte page's record limit; spec/encoding/gin_jsonb_terms.toml
# pins it. The cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, doc jsonb, n i32)
#   CREATE INDEX i_n ON t (n);  CREATE INDEX i_doc_gin ON t USING gin (doc)
#   INSERT (1, '{"a": 1, "b": [true, null, "x"], "c": {"d": "e"}}', 1),
#          (2, '["x", 1.5, 1.50, false, {"a": 2}]', 2), (3, '"x"', 3), (4, '{}', 4), (5, NULL, 5),
#          (6, '-0.001', 6)
# A MULTI-column GIN index (v47, the gin_multi golden, spec/design/gin.md §4.4) over an i32[] and a
# jsonb column: every entry is column_tag ‖ term ‖ storage key, tag 0 for `nums`, 1 for `doc`, so all
# `nums` entries sort before all `doc` entries and the two columns' posting lists never mix. Row 2's
# duplicate element is one entry; a NULL or empty value contributes no entry for its column only. The
# cores build this via
#   CREATE TABLE t (id i32 PRIMARY KEY, nums i32[], doc jsonb)
#   INSERT (1, '{1,2}', '{"a": 1}'), (2, '{2,2}', NULL), (3, NULL, '["x"]'), (4, '{}', '{}')
#   CREATE INDEX i_multi_gin ON t USING gin (nums, doc)
GIN_MULTI_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("nums", "i32[]"), col("doc", "jsonb")],
  indexes: [
    { name: "i_multi_gin", cols: [1, 2], kind: "gin" }
  ],
  rows: [[1, [1, 2], [:obj, [["a", [:num, "1"]]]]],
         [2, [2, 2], nil],
         [3, nil, [:arr, [[:str, "x"]]]],
         [4, [], [:obj, []]]]
}.freeze

GIN_JSONB_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("doc", "jsonb"), col("n", "i32")],
  indexes: [
    { name: "i_doc_gin", cols: [1], kind: "gin" },
    { name: "i_n", cols: [2] }
  ],
  rows: [[1, [:obj, [["a", [:num, "1"]], ["b", [:arr, [[:bool, true], [:null], [:str, "x"]]]],
                     ["c", [:obj, [["d", [:str, "e"]]]]]]], 1],
         [2, [:arr, [[:str, "x"], [:num, "1.5"], [:num, "1.50"], [:bool, false],
                     [:obj, [["a", [:num, "2"]]]]]], 2],
         [3, [:str, "x"], 3],
         [4, [:obj, []], 4],
         [5, nil, 5],
         [6, [:num, "-0.001"], 6]]
}.freeze

# A GIN index over an interval[] column (the interval GIN element golden, spec/design/gin.md §2/§4):
# each GIN term is the element's 16-byte interval-span-i128 key (encoding.md §2.10), so the
# span-equal `1 mon` / `30 days` / `720:00:00` of row 1 are ONE term and row 2's duplicate `-1 day`
# one more, while the stored VALUES keep each interval's own [months, days, micros] fields. Rows 3/4
# hold an empty and a NULL whole-value array (no entries) and row 5 a NULL element. Each element is
# the [months, days, micros] triple.
GIN_INTERVAL_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("v", "interval[]"), col("n", "i32")],
  indexes: [
    { name: "i_n", cols: [2] },
    { name: "i_v_gin", cols: [1], kind: "gin" }
  ],
  rows: [[1, [[1, 0, 0], [0, 30, 0], [0, 0, 2_592_000_000_000]], 1],
         [2, [[0, -1, 0], [0, 0, 1_000_000], [0, -1, 0]], 2],
         [3, [], 3],
         [4, nil, 4],
         [5, [[1200, 0, 0], nil, [-1, 0, 0]], 5]]
}.freeze

# A composite type used as an ARRAY ELEMENT type (v10 — array-of-composite, spec/design/array.md §12
# AC1): pins the catalog array-column entry with a COMPOSITE element descriptor (type_code 15, then
# the element descriptor element_type_code 14 + name "addr", §3) AND the recursive value body — an
# array body (ndim/flags/dims) whose element bodies are composite bodies (null-bitmap + present
# fields, §4). No format_version bump (still 10); the composite-type entry is identical to
# composite_type_table's. Row 1: two full composite elements. Row 2: one element with a NULL `zip`
# FIELD (the composite null-bitmap branch, inside an array element). Row 3: a present composite
# element AND a NULL ELEMENT (the array HAS_NULLS bitmap). Row 4: the empty array (ndim 0). Row 5: a
# whole-value NULL array (the lone 0x01 tag). The cores build this via
#   CREATE TYPE addr AS (street text NOT NULL, zip i32)
#   CREATE TABLE t (id i32 PRIMARY KEY, items addr[])
#   INSERT (1, '{"(Main,90210)","(Side,5)"}'); (2, '{"(Oak,)"}'); (3, '{"(A,1)",NULL}'); (4, '{}'); (5, NULL)
ARRAY_COMPOSITE_TABLE = {
  types: [ctype("addr", [field("street", "text", not_null: true), field("zip", "i32")])],
  tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("items", "addr[]")],
             rows: [[1, [["Main", 90210], ["Side", 5]]],
                    [2, [["Oak", nil]]],
                    [3, [["A", 1], nil]],
                    [4, []],
                    [5, nil]] }]
}.freeze

# A composite type with an ARRAY-typed field (v10 — spec/design/array.md §12, the mirror of
# array-of-composite AC1): pins the catalog composite-type entry with a code-15 array field
# (field_type_code 15 + the inline element descriptor element_type_code 2 = i32, format.md
# *Composite-type entry*) AND the recursive value body — a composite body (null-bitmap + present
# fields, §4) whose `pts` field is an array body (ndim/flags/dims + element bodies). No
# format_version bump (still 10). Row 1: both fields present (a text name + a 3-element i32[]).
# Row 2: an EMPTY array field {} (ndim 0). Row 3: a NULL array field (the composite null-bitmap
# marks field 1 NULL — distinct from an empty array). The cores build this via
#   CREATE TYPE poly AS (name text, pts i32[])
#   CREATE TABLE t (id i32 PRIMARY KEY, p poly)
#   INSERT (1, ROW('a', '{10,20,30}')); (2, ROW('b', '{}')); (3, ROW('c', NULL))
COMPOSITE_ARRAY_FIELD_TABLE = {
  types: [ctype("poly", [field("name", "text"), field("pts", "i32[]")])],
  tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("p", "poly")],
             rows: [[1, ["a", [10, 20, 30]]],
                    [2, ["b", []]],
                    [3, ["c", nil]]] }]
}.freeze

# A composite type with RANGE-typed fields (v41 — spec/design/composite.md §3): pins the catalog
# composite-type entry with code-17 range fields (field_type_code 17 + the one-byte range element
# descriptor — element_type_code 2 = i32 for `span`, 6 = decimal for `amt`, format.md *Composite-type
# entry*) AND the recursive value body — a composite body (null-bitmap + present fields, §4) whose
# range fields are range bodies (flags byte + present bound bodies). Row 1: every field present (a
# discrete [) range + a continuous [] numrange). Row 2: the EMPTY range (the lone 0x01 flags byte)
# and a NULL numrange field (the composite null-bitmap marks field 2). Row 3: a NULL i32range field
# and an unbounded-lower numrange (LB_INF, no lower body). The cores build this via
#   CREATE TYPE booking AS (room text, span i32range, amt numrange)
#   CREATE TABLE t (id i32 PRIMARY KEY, b booking)
#   INSERT (1, ROW('a', '[1,5)', '[1.5,2.25]')); (2, ROW('b', 'empty', NULL)); (3, ROW('c', NULL, '(,10)'))
COMPOSITE_RANGE_FIELD_TABLE = {
  types: [ctype("booking", [field("room", "text"), field("span", "i32range"), field("amt", "numrange")])],
  tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("b", "booking")],
             rows: [[1, ["a", { lower: 1, upper: 5, lower_inc: true, upper_inc: false },
                         { lower: "1.5", upper: "2.25", lower_inc: true, upper_inc: true }]],
                    [2, ["b", :empty, nil]],
                    [3, ["c", nil, { lower: nil, upper: "10", lower_inc: false, upper_inc: false }]]] }]
}.freeze

# FOREIGN KEY constraints (v11 — spec/design/constraints.md §6): pins the catalog foreign-key list
# (fk_count + per FK: name, local ordinals, ref table, ref ordinals, actions byte). Two tables —
# parent `p` (a PK + two UNIQUE constraints, the FK targets) and child `c` carrying four FKs that
# cover every shape: `c_code_fk` (named, references the UNIQUE column code), `c_mgr_fkey` (a
# self-reference to c's own PK), `c_pid_fkey` (auto-named, references the PK), and `c_x_y_fkey`
# (auto-named, COMPOSITE — references the two-column UNIQUE (a,b), MATCH FULL). Together their actions
# bytes pin every v30 action code and the v40 match_full bit. FKs are emitted in ascending lowercased-name order; an FK owns no B-tree.
# The cores build this via
#   CREATE TABLE p (pid i32 PRIMARY KEY, code i32 UNIQUE, a i32, b i32, UNIQUE (a, b))
#   INSERT INTO p VALUES (1, 100, 10, 20), (2, 200, 30, 40)
#   CREATE TABLE c (id i32 PRIMARY KEY, pid i32, pcode i32, x i32, y i32, mgr i32,
#     FOREIGN KEY (pid) REFERENCES p (pid) ON DELETE RESTRICT,
#     CONSTRAINT c_code_fk FOREIGN KEY (pcode) REFERENCES p (code) ON DELETE SET NULL,
#     FOREIGN KEY (x, y) REFERENCES p (a, b) MATCH FULL ON DELETE CASCADE ON UPDATE SET DEFAULT,
#     FOREIGN KEY (mgr) REFERENCES c (id))
#   INSERT INTO c VALUES (10, 1, 100, 10, 20, NULL), (11, 2, 200, 30, 40, 10)
FK_TABLE = {
  tables: [
    { name: "p",
      columns: [col("pid", "i32", pk: true), col("code", "i32"),
                col("a", "i32"), col("b", "i32")],
      indexes: [
        { name: "p_a_b_key", cols: [2, 3], unique: true, constraint: true },
        { name: "p_code_key", cols: [1], unique: true, constraint: true }
      ],
      rows: [[1, 100, 10, 20], [2, 200, 30, 40]] },
    { name: "c",
      columns: [col("id", "i32", pk: true), col("pid", "i32"), col("pcode", "i32"),
                col("x", "i32"), col("y", "i32"), col("mgr", "i32")],
      fks: [
        { name: "c_code_fk", local: [2], ref_table: "p", ref: [1], actions: 3 },
        { name: "c_mgr_fkey", local: [5], ref_table: "c", ref: [0] },
        { name: "c_pid_fkey", local: [1], ref_table: "p", ref: [0], actions: 1 },
        { name: "c_x_y_fkey", local: [3, 4], ref_table: "p", ref: [2, 3], actions: 98 }
      ],
      rows: [[10, 1, 100, 10, 20, nil], [11, 2, 200, 30, 40, 10]] }
  ]
}.freeze

# SEQUENCES (v12 — spec/design/sequences.md §3): pins the sequence catalog entry (entry_kind 2 +
# name + six i64 fields + the flags byte) AND the emission order — sequence entries (kind 2) before
# table entries (kind 0). Two sequences: `s1` is an ASCENDING default sequence advanced three times
# (is_called true, last_value 3, default MAXVALUE i64::MAX — pins a large positive i64) and `s2` is a
# DESCENDING fresh one (is_called false, NEGATIVE increment/min/max/start — pins negative two's-
# complement i64 — plus a non-default CACHE 5 and CYCLE, so both flag bits and the cache field are
# exercised). A one-row table `t` follows, proving sequences and tables coexist in catalog order. The
# cores build this via
#   CREATE SEQUENCE s1; SELECT nextval('s1') [×3];
#   CREATE SEQUENCE s2 INCREMENT BY -2 MINVALUE -100 MAXVALUE -1 CACHE 5 CYCLE;
#   CREATE TABLE t (id i32 PRIMARY KEY, v i32); INSERT INTO t VALUES (1, 10)
SEQUENCE_TABLE = {
  sequences: [
    seq("s1", increment: 1, min_value: 1, max_value: 9_223_372_036_854_775_807, start: 1,
        cache: 1, cycle: false, last_value: 3, is_called: true),
    seq("s2", increment: -2, min_value: -100, max_value: -1, start: -1,
        cache: 5, cycle: true, last_value: -1, is_called: false)
  ],
  tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("v", "i32")],
             rows: [[1, 10]] }]
}.freeze

# SERIAL (v14 — spec/design/sequences.md §12): pins the OWNED-sequence link (the has_owner flag bit
# + the owner table-name/column-ordinal tail). The `serial` column `id` desugars to an i32 column
# that is NOT NULL (via the PK) with an EXPRESSION DEFAULT `nextval ( 't_id_seq' )` (flags bit3), and
# an OWNED sequence `t_id_seq` (owned_by ["t", 0]) created alongside. One INSERT advances the sequence
# once (is_called true, last_value 1). The cores build this via
#   CREATE TABLE t (id serial PRIMARY KEY, v text); INSERT INTO t (v) VALUES ('hello')
SERIAL_TABLE = {
  sequences: [
    # serial → AS integer (S5, §14): max_value is the i32 ceiling 2_147_483_647, not 2^63-1.
    seq("t_id_seq", increment: 1, min_value: 1, max_value: 2_147_483_647, start: 1,
        cache: 1, cycle: false, last_value: 1, is_called: true, owned_by: ["t", 0])
  ],
  tables: [{ name: "t",
             columns: [col("id", "i32", pk: true, default_expr: "nextval ( 't_id_seq' )"),
                       col("v", "text")],
             rows: [[1, "hello"]] }]
}.freeze

# IDENTITY (v15 — spec/design/sequences.md §13): pins the two new column flag bits (bit4 is_identity,
# bit5 identity_always) for both identity kinds, atop the same serial-shaped OWNED-sequence bytes. The
# ALWAYS column `id` (flags bit1+bit3+bit4+bit5) and the BY DEFAULT column `n` (flags bit1+bit3+bit4)
# each get an owned AS-integer sequence (`t_id_seq` owned by col 0, `t_n_seq` owned by col 1) and an
# EXPRESSION DEFAULT `nextval ( '<seq>' )`. One INSERT advances both sequences once. The cores build
# this via
#   CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
#                   n int GENERATED BY DEFAULT AS IDENTITY, v text);
#   INSERT INTO t (v) VALUES ('hi')
IDENTITY_TABLE = {
  sequences: [
    # int GENERATED … AS IDENTITY → AS integer (S5, §14): max_value is the i32 ceiling 2_147_483_647.
    seq("t_id_seq", increment: 1, min_value: 1, max_value: 2_147_483_647, start: 1,
        cache: 1, cycle: false, last_value: 1, is_called: true, owned_by: ["t", 0]),
    seq("t_n_seq", increment: 1, min_value: 1, max_value: 2_147_483_647, start: 1,
        cache: 1, cycle: false, last_value: 1, is_called: true, owned_by: ["t", 1])
  ],
  tables: [{ name: "t",
             columns: [col("id", "i32", pk: true, default_expr: "nextval ( 't_id_seq' )",
                           identity: :always),
                       col("n", "i32", not_null: true, default_expr: "nextval ( 't_n_seq' )",
                           identity: :by_default),
                       col("v", "text")],
             rows: [[1, 1, "hi"]] }]
}.freeze

# VIEWS (v38 — spec/design/views.md §3, format.md *View entry*): pins the entry_kind 5 view entry and
# its emission LAST (after the table and statistics entries), in view-name byte order. A table t(a, b)
# with two rows and three views: `v` stores the frozen `*` expansion and depends on t's columns a, b;
# `w` renames its one output column through a column list and depends on the view v (a view dependency
# carries no columns); `n` depends on t with ZERO columns. v39 adds the check options: `v` CASCADED
# (2), `w` LOCAL (1), `n` none (0). The cores build this via
#   CREATE TABLE t (a i32 PRIMARY KEY, b text); INSERT INTO t VALUES (1, 'x'), (2, 'y');
#   CREATE VIEW v AS SELECT * FROM t WHERE a > 0 WITH CHECK OPTION;
#   CREATE VIEW w (x) AS SELECT a FROM v WITH LOCAL CHECK OPTION;
#   CREATE VIEW n AS SELECT count(*) AS c FROM t
VIEW_TABLE = {
  tables: [{ name: "t", columns: [col("a", "i32", pk: true), col("b", "text")],
             rows: [[1, "x"], [2, "y"]] }],
  views: [
    { name: "v", columns: %w[a b], definition: 'select "t" . "a" , "t" . "b" from t where a > 0',
      deps: [{ kind: 0, name: "t", columns: %w[a b] }], check_option: 2 },
    { name: "w", columns: %w[x], definition: "select a from v",
      deps: [{ kind: 1, name: "v", columns: [] }], check_option: 1 },
    { name: "n", columns: %w[c], definition: "select count ( * ) as c from t",
      deps: [{ kind: 0, name: "t", columns: [] }], check_option: 0 }
  ]
}.freeze

# VIEW OPTIONS (v48 — spec/design/views.md §6/§16, format.md *View entry* and *Catalog*): pins a
# view column default (the column flags byte + the u32-length text — a folded constant and an
# expression), the view flags byte (security_barrier, security_invoker), the sequence (2) and
# composite-type (3) dependency kinds in (name, kind) order, and a view entry longer than a catalog
# page spanning three chain pages flagged `continued`. The cores build this via
#   CREATE TYPE addr AS (street text, zip i32); CREATE SEQUENCE s;
#   CREATE TABLE t (a i32 PRIMARY KEY, b text);
#   CREATE VIEW d WITH (security_barrier, security_invoker) AS
#     SELECT a, b, nextval(s) AS n, NULL::addr AS x FROM t;
#   ALTER VIEW d ALTER b SET DEFAULT z, ALTER a SET DEFAULT 1 + 1;
#   CREATE VIEW long_v AS SELECT 1 AS c1, 2 AS c2, …, 30 AS c30
VIEW_OPTIONS_TABLE = {
  types: [ctype("addr", [field("street", "text"), field("zip", "i32")])],
  sequences: [seq("s", increment: 1, min_value: 1, max_value: 9_223_372_036_854_775_807, start: 1)],
  tables: [{ name: "t", columns: [col("a", "i32", pk: true), col("b", "text")], rows: [] }],
  views: [
    { name: "d", columns: %w[a b n x], defaults: { "a" => "1 + 1", "b" => "'z'" },
      definition: "select a , b , nextval ( 's' ) as n , null :: addr as x from t",
      deps: [{ kind: 3, name: "addr", columns: [] }, { kind: 2, name: "s", columns: [] },
             { kind: 0, name: "t", columns: %w[a b] }],
      check_option: 0, security_barrier: true, security_invoker: true },
    { name: "long_v", columns: (1..30).map { |i| "c#{i}" },
      definition: "select 1 as c1 , 2 as c2 , 3 as c3 , 4 as c4 , 5 as c5 , 6 as c6 , 7 as c7 , 8 as c8 , 9 as c9 , 10 as c10 , 11 as c11 , 12 as c12 , 13 as c13 , 14 as c14 , 15 as c15 , 16 as c16 , 17 as c17 , 18 as c18 , 19 as c19 , 20 as c20 , 21 as c21 , 22 as c22 , 23 as c23 , 24 as c24 , 25 as c25 , 26 as c26 , 27 as c27 , 28 as c28 , 29 as c29 , 30 as c30", deps: [], check_option: 0 }
  ]
}.freeze

# collation_table (v18): a reference-only COLLATION entry (entry_kind 3) + per-column collations. The
# real `unicode` collation (the version-pinned CLDR-DUCET root, UCA/UCD 17.0.0) is referenced and set
# as the per-database default (the is_default flag); the entry is metadata only — name + version pin +
# description, NO table (the table is vendored into the binary, spec/design/collation.md §2/§5/§9).
# `unicode`'s metadata comes from its @version record: unicode "17.0.0", no cldr record, no
# description. `name` carries an explicit COLLATE "unicode" (flags bit6 + name), `plain` is
# un-annotated and inherits the default (also unicode, frozen), and `byteorder` carries explicit
# COLLATE "C" → no collation (bit6 clear). Must match the cores' collation_table_db.
COLLATION_TABLE = {
  collations: [
    { name: "unicode", default: true, unicode: "17.0.0", cldr: "", desc: "" }
  ],
  tables: [{ name: "t",
             columns: [col("id", "i32", pk: true),
                       col("name", "text", collation: "unicode"),
                       col("plain", "text", collation: "unicode"),
                       col("byteorder", "text", collation: nil)],
             rows: [[1, "a", "b", "z"], [2, "z", "a", "a"]] }]
}.freeze

# A collated text PRIMARY KEY + a collated secondary index (slice 1e, encoding.md §2.12): both keys
# store the `unicode` UCA SORT KEY, not the raw UTF-8, so the B-tree iterates in COLLATION order. The
# reference-only `unicode` metadata entry travels with the file (entry_kind 3). The key sort-key bytes
# are the pinned spec/collation/vectors/sortkey.toml vectors (collated_sort_key). Must match the cores'
# collation_pk_table_db (spec/design/collation.md §8).
COLLATION_PK_TABLE = {
  collations: [
    { name: "unicode", default: false, unicode: "17.0.0", cldr: "", desc: "" }
  ],
  tables: [{ name: "t",
             columns: [col("name", "text", pk: true, collation: "unicode"),
                       col("tag", "text", collation: "unicode")],
             indexes: [{ name: "t_tag_idx", cols: [1] }],
             rows: [["a", "b"], ["z", "a"]] }]
}.freeze

# collation_array_table (v36): a collated text[] PRIMARY KEY + a collated text[] secondary index
# (collation.md §1, encoding.md §2.14): each present element of both keys is the `unicode` UCA SORT
# KEY (the pinned sortkey.toml vectors), so the B-tree iterates in collation order — {a} < {a,Z} <
# {A} < {b}. Must match the cores' collation_array_table_db.
COLLATION_ARRAY_TABLE = {
  collations: [
    { name: "unicode", default: false, unicode: "17.0.0", cldr: "", desc: "" }
  ],
  tables: [{ name: "t",
             columns: [col("k", "text[]", pk: true, collation: "unicode"),
                       col("tags", "text[]", collation: "unicode")],
             indexes: [{ name: "t_tags_idx", cols: [1] }],
             rows: [[["A"], ["b", nil]], [["a", "Z"], ["a"]], [["b"], ["z"]], [["a"], nil]] }]
}.freeze

# collation_skew_corrupt (v18): a version-SKEWED collated table — the read-safety regression fixture
# for the deferred collated-index pushdown follow-on (spec/design/collation.md §8/§12/§14). The
# `unicode` reference entry is pinned to a BOGUS 9999.0.0 (≠ the loaded 17.0.0 bundle), so on open the
# collation's verdict is Skewed and `t` is READ-ONLY (XX002 on any write — collation.md §12). Its
# secondary index `t_name_idx` over the COLLATE "unicode" column is deliberately authored with C /
# BYTE-ORDER keys (key_collation: "C") — i.e. WRONG for the loaded unicode order. Today every collated
# operation heap-scans + recomputes against the LOADED table (collated keys never push down, §8), so
# the index is never consulted and reads are correct DESPITE the wrong index. The day collated-index
# pushdown lands (§14, "the obvious follow-on"), a query that would use this index MUST bypass/rebuild
# a skewed one or it returns wrong rows — suites/collation/skew.test is the tripwire (green today, red
# the instant a skewed index is trusted). Rows pinned so unicode-order (a < ä < b < … < z < Z) and
# byte-order (Z < a < b < ä) disagree on the SET, not just the order, of `name >= 'b'`.
COLLATION_SKEW_CORRUPT_TABLE = {
  collations: [
    { name: "unicode", default: false, unicode: "9999.0.0", cldr: "", desc: "" }
  ],
  tables: [{ name: "t",
             columns: [col("id", "i32", pk: true),
                       col("name", "text", collation: "unicode")],
             indexes: [{ name: "t_name_idx", cols: [1], key_collation: "C" }],
             rows: [[1, "a"], [2, "Z"], [3, "ä"], [4, "b"]] }]
}.freeze

# collation_skew_twin (v18): the NON-skewed twin of COLLATION_SKEW_CORRUPT — identical schema + rows,
# but the `unicode` pin MATCHES the loaded bundle (17.0.0 → verdict Full, read-write) and `t_name_idx`
# stores the CORRECT unicode UCA sort keys. The same query is correct today (full scan) AND stays
# correct once collated-index pushdown uses this index. The contrast is the point: a CORRECT collated
# index is safe to push down to; a SKEWED one is not. suites/collation/skew_full.test asserts it.
COLLATION_SKEW_TWIN_TABLE = {
  collations: [
    { name: "unicode", default: false, unicode: "17.0.0", cldr: "", desc: "" }
  ],
  tables: [{ name: "t",
             columns: [col("id", "i32", pk: true),
                       col("name", "text", collation: "unicode")],
             indexes: [{ name: "t_name_idx", cols: [1] }],
             rows: [[1, "a"], [2, "Z"], [3, "ä"], [4, "b"]] }]
}.freeze

# unparsable_definitions: a file a newer jed wrote, holding one stored definition of each kind whose
# text this binary cannot parse (spec/design/unparsable-definitions.md). Every core must open it, read
# around the unparsed objects, refuse with XX003 exactly the statements that need one, and drop each.
# The text uses `is future syntax`, which no jed parser accepts. The expression and partial indexes are
# forged EMPTY over tables that have rows (`forged_empty`), so a planner that trusts either loses rows.
# suites/storage/unparsable_definitions.test asserts the behavior. Tables:
#   t — views over it: `ok` parses, `u` and `r` do not, and `w` reads `u`;
#   c — a parsable CHECK c_ok (v > 0) and an unparsed c_v_check;
#   d — an unparsed expression DEFAULT on v and a parsable one (1 + 1) on k;
#   x — an unparsed expression index x_future_idx beside a plain index x_v_idx on v;
#   p — a UNIQUE partial index p_future_idx on v whose predicate is unparsed;
#   e — a partial exclusion e_r_excl (EXCLUDE USING gist (r WITH &&) WHERE (…), v45) whose predicate
#       is unparsed, its GiST index forged empty over two overlapping rows.
UNPARSABLE_DEFINITIONS = {
  tables: [
    { name: "t", columns: [col("id", "i32", pk: true), col("v", "i32")],
      rows: [[1, 1], [2, 2], [3, -3]] },
    { name: "c", columns: [col("id", "i32", pk: true), col("v", "i32")],
      checks: [{ name: "c_ok", expr: "v > 0" }, { name: "c_v_check", expr: "v is future syntax" }],
      rows: [[1, 10], [2, 20]] },
    { name: "d", columns: [col("id", "i32", pk: true), col("v", "i32", default_expr: "1 is future syntax"),
                           col("k", "i32", default_expr: "1 + 1")],
      rows: [[1, 1, 1]] },
    { name: "x", columns: [col("id", "i32", pk: true), col("v", "i32")],
      indexes: [{ name: "x_future_idx", cols: [{ expr: "v is future syntax" }], forged_empty: true },
                { name: "x_v_idx", cols: [1] }],
      rows: [[1, 1], [2, 2]] },
    { name: "p", columns: [col("id", "i32", pk: true), col("v", "i32")],
      indexes: [{ name: "p_future_idx", cols: [1], unique: true, predicate: "v is future syntax",
                  forged_empty: true }],
      rows: [[1, 1], [2, 2]] },
    { name: "e", columns: [col("id", "i32", pk: true), col("r", "i32range")],
      indexes: [{ name: "e_r_excl", cols: [1], kind: "gist", predicate: "r is future syntax",
                  forged_empty: true }],
      exclusions: [{ name: "e_r_excl", index: "e_r_excl", elements: [[1, "&&"]] }],
      rows: [[1, rng(1, 5)], [2, rng(3, 8)]] }
  ],
  views: [
    { name: "ok", columns: %w[id v], definition: "select id , v from t where v > 0",
      deps: [{ kind: 0, name: "t", columns: %w[id v] }], check_option: 0 },
    { name: "u", columns: %w[id v], definition: "select id , v from t where v is future syntax",
      deps: [{ kind: 0, name: "t", columns: %w[id v] }], check_option: 0 },
    { name: "w", columns: %w[id], definition: "select id from u",
      deps: [{ kind: 1, name: "u", columns: [] }], check_option: 0 },
    { name: "r", columns: %w[id], definition: "select id from t where id is future syntax",
      deps: [{ kind: 0, name: "t", columns: %w[id] }], check_option: 0 }
  ]
}.freeze

COW_OLD_TABLE = { name: "t", columns: [col("id", "i32", pk: true), col("v", "i32")],
                  rows: [[1, 10]] }.freeze
COW_NEW_TABLE = COW_OLD_TABLE.merge(rows: [[1, 20]]).freeze

# These encode commit history, not a from-scratch image. Body pages 2/3 retain the
# preceding root; the new leaf/catalog are at 4/5. Extra *orphan* writes deliberately
# exercise dependencies that a root-reachability walk alone cannot protect.
COW_FIXTURES = [
  ["inline", :valid, 0],
  ["overflow", :valid, 37],
  ["empty_tail", :empty_tail, 15],
  ["torn_body", :torn_body, 0],
  ["stale_body", :stale_body, 0],
  ["stale_orphan", :stale_orphan, 37],
  ["torn_meta", :torn_meta, 0],
  ["torn_manifest", :torn_manifest, 37],
  ["wrong_manifest_txid", :wrong_manifest_txid, 37],
  ["manifest_cycle", :manifest_cycle, 37],
  ["duplicate_entry", :duplicate_entry, 0],
  ["out_of_bounds", :out_of_bounds, 0],
  ["missing_root", :missing_root, 0],
  ["root_manifest", :root_manifest, 37],
  ["free_head_manifest", :free_head_manifest, 37],
  ["invalid_free_list", :invalid_free_list, 0],
  ["bad_padding", :bad_padding, 0]
].map do |name, mutation, orphans|
  accepted = [:valid, :empty_tail].include?(mutation)
  { file: "cow_#{name}.jed", page_size: 256, tables: [accepted ? COW_NEW_TABLE : COW_OLD_TABLE],
    cow: mutation, cow_orphans: orphans, expected_txid: accepted ? 2 : 1,
    expected_open_error: mutation == :invalid_free_list ? "invalid free-list entry" : nil }
end.freeze

# Malformed-page fixtures (../design/storage.md §6): images whose pages are CRC-valid, so they
# pass the checksum, but whose structure lies — the shapes a misdirected or stale page presents.
# Each is a clean image plus one forged mutation (malformed_fixture_image), page CRCs resealed so
# the rejection is structural rather than a checksum mismatch. The cores must answer every one with
# XX001 (suites/storage/malformed_*.test), never a crash, hang, or wrong rows; the decoder below
# rejects the ones whose lie it can see (`reject:`). `wrong_tree_leaf` is the simulator's case: a
# child pointer into another table's leaf, whose column layout differs.
MALFORMED_SHAPE_TABLE = {
  name: "u",
  columns: [col("id", "i32", pk: true), col("a", "text"), col("b", "text"), col("c", "boolean")],
  rows: [[1, "x", "y", true], [2, "z", nil, false]]
}.freeze

MALFORMED_FK_TYPE_TABLES = [
  { name: "c", columns: [col("id", "i32", pk: true), col("pid", "text")],
    fks: [{ name: "c_pid_fkey", local: [1], ref_table: "p", ref: [0] }], rows: [] },
  { name: "p", columns: [col("pid", "i32", pk: true)], rows: [] }
].freeze

MALFORMED_FIXTURES = [
  { file: "malformed_leaf_directory.jed", page_size: 256, tables: [PK_TABLE],
    malformed: :leaf_directory, reject: true },
  { file: "malformed_wrong_tree_leaf.jed", page_size: 256, tables: [PK_TABLE, MALFORMED_SHAPE_TABLE],
    malformed: :wrong_tree_leaf, reject: true },
  { file: "malformed_interior_cycle.jed", page_size: 256, tables: [PK_TABLE],
    malformed: :interior_cycle, reject: true },
  { file: "malformed_decimal_group.jed", page_size: 256, tables: [DECIMAL_TABLE],
    malformed: :decimal_group, reject: true },
  { file: "malformed_overflow_cycle.jed", page_size: 256, tables: [OVERFLOW_TABLE],
    malformed: :overflow_cycle, reject: true },
  # The catalog claims 25 rows over a 20-row tree: invisible until a write's count disagrees with it.
  { file: "malformed_row_count.jed", page_size: 256, tables: [PK_TABLE.merge(row_count: 25)],
    malformed: :none, reject: true },
  # Semantic lies the decoder does not model: an FK pairing text with an i32 key, a column whose
  # composite type is not in the catalog.
  { file: "malformed_fk_type.jed", page_size: 256, tables: MALFORMED_FK_TYPE_TABLES, malformed: :none },
  { file: "malformed_missing_type.jed", page_size: 256, types: [ctype("ghost", [field("x", "i32")])],
    tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("home", "ghost")], rows: [] }],
    malformed: :drop_types }
].freeze

# --- encrypted-variant fixtures (spec/fileformat/encrypted.md), page size 1024 --------------------
# Each is the from-scratch image under the goldens' host key ENC_KEY with the key-entropy stream
# seeded at ENC_SEED: the stream's first draws are the DEK, KDF salt and wrap nonce, then one 24-byte
# nonce per sealed page in seal order (§7, §8). The cores build each via SQL in an encrypted database
# (so the tree splits at the encrypted capacity C = 968) and serialize it the same way.
#
# `encrypted_pk_table`: 300 rows of (id i32 PRIMARY KEY, v i16), id 3's v NULL — an interior root over
# several leaves, so interior child pointers carry tags.
ENC_PK_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("v", "i16")],
  rows: (1..300).map { |i| [i, i == 3 ? nil : i * 10] }
}.freeze

# `encrypted_overflow_table`: incompressible values that spill into multi-page chains (slab C − 16 =
# 952 bytes after the successor's tag), so external pointers and chain links carry tags.
ENC_OVERFLOW_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("body", "text"), col("blob", "bytea")],
  rows: [[1, filler_text(2500), filler_bytes(1200)], [2, "small", ["cafe"].pack("H*")], [3, nil, nil]]
}.freeze

# `encrypted_index_table`: a B-tree index whose tree has an interior root, and a GiST R-tree over
# 150 ranges (several levels), so both kinds of index pointer carry tags. Built via
#   CREATE TABLE t (a i32 PRIMARY KEY, v i64, r i32range)
#   CREATE INDEX t_r ON t USING gist (r);  CREATE INDEX t_v ON t (v)
#   INSERT INTO t VALUES (a, a * 3, i32range(a, a + 5)) for a in 1..150, ascending.
ENC_INDEX_TABLE = {
  name: "t",
  columns: [col("a", "i32", pk: true), col("v", "i64"), col("r", "i32range")],
  indexes: [{ name: "t_r", cols: [2], kind: "gist" }, { name: "t_v", cols: [1] }],
  rows: (1..150).map { |a| [a, a * 3, rng(a, a + 5)] }
}.freeze

# `encrypted_catalog`: 30 empty tables t00 … t29 of (id i32 PRIMARY KEY, a text, b i64), whose
# entries span three catalog pages, so the catalog chain is sealed last page first.
ENC_CATALOG_TABLES = (0...30).map do |i|
  { name: format("t%02d", i), columns: [col("id", "i32", pk: true), col("a", "text"), col("b", "i64")], rows: [] }
end.freeze

FIXTURES = ([
  { file: "encrypted_empty_db.jed", page_size: 1024, encrypted: true, tables: [] },
  { file: "encrypted_pk_table.jed", page_size: 1024, encrypted: true, tables: [ENC_PK_TABLE] },
  { file: "encrypted_overflow_table.jed", page_size: 1024, encrypted: true, tables: [ENC_OVERFLOW_TABLE] },
  { file: "encrypted_index_table.jed", page_size: 1024, encrypted: true, tables: [ENC_INDEX_TABLE] },
  { file: "encrypted_catalog.jed", page_size: 1024, encrypted: true, tables: ENC_CATALOG_TABLES },
  # A plaintext file holding ENC_INDEX_TABLE's rows (create, CREATE TABLE, two CREATE INDEX, 150
  # inserts: txid 154), encrypted by a rekey (txid 155): every tree rebuilt by ascending insertion at
  # the encrypted capacity, and the database's first key-entropy draws the key block (encrypted.md §12).
  { file: "encrypted_rekey_index_table.jed", page_size: 1024, encrypted: true, tables: [ENC_INDEX_TABLE],
    txid: 155 },
  { file: "empty_db.jed",        page_size: 256, tables: [] },
  { file: "overflow_table.jed",  page_size: 256, tables: [OVERFLOW_TABLE] },
  { file: "compressed_table.jed", page_size: 256, tables: [COMPRESSED_TABLE] },
  { file: "one_table_empty.jed", page_size: 256,
    tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("v", "i16")], rows: [] }] },
  { file: "row_count_table.jed", page_size: 256, tables: [ROW_COUNT_TABLE] },
  { file: "statistics_table.jed", page_size: 256, tables: STATISTICS_TABLES },
  { file: "statistics_terms_table.jed", page_size: 256, tables: [STATISTICS_TERMS_TABLE] },
  { file: "pk_table.jed",        page_size: 256, tables: [PK_TABLE] },
  { file: "text_table.jed",      page_size: 256, tables: [TEXT_TABLE] },
  { file: "varchar_table.jed",   page_size: 256, tables: [VARCHAR_TABLE] },
  { file: "bool_table.jed",      page_size: 256, tables: [BOOL_TABLE] },
  { file: "bool_pk_table.jed",   page_size: 256, tables: [BOOL_PK_TABLE] },
  { file: "decimal_table.jed",   page_size: 256, tables: [DECIMAL_TABLE] },
  { file: "bytea_table.jed",     page_size: 256, tables: [BYTEA_TABLE] },
  { file: "text_pk_table.jed",   page_size: 256, tables: [TEXT_PK_TABLE] },
  { file: "bytea_pk_table.jed",  page_size: 256, tables: [BYTEA_PK_TABLE] },
  { file: "decimal_pk_table.jed", page_size: 256, tables: [DECIMAL_PK_TABLE] },
  { file: "uuid_table.jed",      page_size: 256, tables: [UUID_TABLE] },
  { file: "default_table.jed",   page_size: 256, tables: [DEFAULT_TABLE] },
  { file: "default_expr_table.jed", page_size: 256, tables: [DEFAULT_EXPR_TABLE] },
  { file: "timestamp_table.jed",   page_size: 256, tables: [TIMESTAMP_TABLE] },
  { file: "timestamptz_table.jed", page_size: 256, tables: [TIMESTAMPTZ_TABLE] },
  { file: "interval_table.jed",    page_size: 256, tables: [INTERVAL_TABLE] },
  { file: "interval_pk_table.jed", page_size: 256, tables: [INTERVAL_PK_TABLE] },
  { file: "float64_table.jed",     page_size: 256, tables: [FLOAT64_TABLE] },
  { file: "float32_table.jed",     page_size: 256, tables: [FLOAT32_TABLE] },
  { file: "float64_pk_table.jed",  page_size: 256, tables: [FLOAT64_PK_TABLE] },
  { file: "float32_pk_table.jed",  page_size: 256, tables: [FLOAT32_PK_TABLE] },
  { file: "date_table.jed",        page_size: 256, tables: [DATE_TABLE] },
  { file: "nopk_table.jed",      page_size: 256,
    tables: [{ name: "r", columns: [col("a", "i16"), col("b", "i64")],
               rows: [[7, 70], [8, 80], [9, 90]] }] },
  { file: "composite_pk_table.jed", page_size: 256, tables: [COMPOSITE_PK_TABLE] },
  { file: "check_table.jed", page_size: 256, tables: [CHECK_TABLE] },
  { file: "index_table.jed", page_size: 256, tables: [INDEX_TABLE] },
  { file: "unique_table.jed", page_size: 256, tables: [UNIQUE_TABLE] },
  { file: "expr_index_table.jed", page_size: 256, tables: [EXPR_INDEX_TABLE] },
  { file: "partial_index_table.jed", page_size: 256, tables: [PARTIAL_INDEX_TABLE] },
  { file: "ordered_index_table.jed", page_size: 256, tables: [ORDERED_INDEX_TABLE] },
  { file: "timezone_index_table.jed", page_size: 256, tables: [TIMEZONE_INDEX_TABLE] },
  { file: "hostfunc_index_table.jed", page_size: 256, tables: [HOSTFUNC_INDEX_TABLE] },
  { file: "hostfunc_check_table.jed", page_size: 256, tables: [HOSTFUNC_CHECK_TABLE] },
  { file: "gin_array_table.jed", page_size: 256, tables: [GIN_ARRAY_TABLE] },
  { file: "gin_uuid_table.jed", page_size: 256, tables: [GIN_UUID_TABLE] },
  { file: "gin_float_table.jed", page_size: 256, tables: [GIN_FLOAT_TABLE] },
  { file: "gin_interval_table.jed", page_size: 256, tables: [GIN_INTERVAL_TABLE] },
  { file: "gin_jsonb_table.jed", page_size: 256, tables: [GIN_JSONB_TABLE] },
  { file: "gin_multi_table.jed", page_size: 256, tables: [GIN_MULTI_TABLE] },
  { file: "gin_text_table.jed", page_size: 256, tables: [GIN_TEXT_TABLE] },
  { file: "gin_decimal_bytea_table.jed", page_size: 256, tables: [GIN_DECIMAL_BYTEA_TABLE] },
  { file: "fk_table.jed", page_size: 256, tables: FK_TABLE[:tables] },
  { file: "array_table.jed", page_size: 256, tables: [ARRAY_TABLE] },
  { file: "range_table.jed", page_size: 256, tables: [RANGE_TABLE] },
  { file: "range_pk_table.jed", page_size: 256, tables: [RANGE_PK_TABLE] },
  { file: "array_pk_table.jed", page_size: 256, tables: [ARRAY_PK_TABLE] },
  { file: "gist_range_table.jed", page_size: 256, tables: [GIST_RANGE_TABLE] },
  { file: "gist_scalar_table.jed", page_size: 256, tables: [GIST_SCALAR_TABLE] },
  { file: "gist_exclude_table.jed", page_size: 256, tables: [GIST_EXCLUDE_TABLE] },
  { file: "exclude_followon_table.jed", page_size: 256, tables: [EXCLUDE_FOLLOWON_TABLE] },
  { file: "json_table.jed", page_size: 256, tables: [JSON_TABLE] },
  { file: "jsonb_table.jed", page_size: 256, tables: [JSONB_TABLE] },
  { file: "composite_type_table.jed", page_size: 256,
    types: COMPOSITE_TYPE_TABLE[:types], tables: COMPOSITE_TYPE_TABLE[:tables] },
  { file: "temporal_typmod_table.jed", page_size: 256,
    types: TEMPORAL_TYPMOD_TABLE[:types], tables: TEMPORAL_TYPMOD_TABLE[:tables] },
  { file: "composite_key_table.jed", page_size: 256,
    types: COMPOSITE_KEY_TABLE[:types], tables: COMPOSITE_KEY_TABLE[:tables] },
  { file: "array_composite_table.jed", page_size: 256,
    types: ARRAY_COMPOSITE_TABLE[:types], tables: ARRAY_COMPOSITE_TABLE[:tables] },
  { file: "composite_array_field_table.jed", page_size: 256,
    types: COMPOSITE_ARRAY_FIELD_TABLE[:types], tables: COMPOSITE_ARRAY_FIELD_TABLE[:tables] },
  { file: "composite_range_field_table.jed", page_size: 256,
    types: COMPOSITE_RANGE_FIELD_TABLE[:types], tables: COMPOSITE_RANGE_FIELD_TABLE[:tables] },
  { file: "nested_composite_table.jed", page_size: 256,
    types: NESTED_COMPOSITE_TABLE[:types], tables: NESTED_COMPOSITE_TABLE[:tables] },
  { file: "sequence_table.jed", page_size: 256,
    sequences: SEQUENCE_TABLE[:sequences], tables: SEQUENCE_TABLE[:tables] },
  { file: "serial_table.jed", page_size: 256,
    sequences: SERIAL_TABLE[:sequences], tables: SERIAL_TABLE[:tables] },
  { file: "identity_table.jed", page_size: 256,
    sequences: IDENTITY_TABLE[:sequences], tables: IDENTITY_TABLE[:tables] },
  { file: "view_table.jed", page_size: 256, tables: VIEW_TABLE[:tables], views: VIEW_TABLE[:views] },
  { file: "view_options_table.jed", page_size: 256, types: VIEW_OPTIONS_TABLE[:types],
    sequences: VIEW_OPTIONS_TABLE[:sequences], tables: VIEW_OPTIONS_TABLE[:tables],
    views: VIEW_OPTIONS_TABLE[:views] },
  { file: "collation_table.jed", page_size: 256,
    collations: COLLATION_TABLE[:collations], tables: COLLATION_TABLE[:tables] },
  { file: "collation_pk_table.jed", page_size: 256,
    collations: COLLATION_PK_TABLE[:collations], tables: COLLATION_PK_TABLE[:tables] },
  { file: "collation_array_table.jed", page_size: 256,
    collations: COLLATION_ARRAY_TABLE[:collations], tables: COLLATION_ARRAY_TABLE[:tables] },
  { file: "collation_skew_corrupt.jed", page_size: 256,
    collations: COLLATION_SKEW_CORRUPT_TABLE[:collations], tables: COLLATION_SKEW_CORRUPT_TABLE[:tables] },
  { file: "unparsable_definitions.jed", page_size: 256,
    tables: UNPARSABLE_DEFINITIONS[:tables], views: UNPARSABLE_DEFINITIONS[:views] },
  { file: "collation_skew_twin.jed", page_size: 256,
    collations: COLLATION_SKEW_TWIN_TABLE[:collations], tables: COLLATION_SKEW_TWIN_TABLE[:tables] },
  { file: "tall_tree.jed",       page_size: 256, tables: [TALL_TREE] },
  { file: "max_sep_table.jed",   page_size: 256, tables: [MAX_SEP_TABLE] },
  # Torn-write fallback: same image as pk_table, with one meta slot's CRC smashed.
  { file: "torn_meta_slot0.jed", page_size: 256, tables: [PK_TABLE], corrupt_slot: 0 },
  { file: "torn_meta_slot1.jed", page_size: 256, tables: [PK_TABLE], corrupt_slot: 1 }
] + COW_FIXTURES + MALFORMED_FIXTURES).freeze

# --- primitives -------------------------------------------------------------

def u16(v) = [v].pack("n")
def u32(v) = [v].pack("N")
def u64(v) = [v].pack("Q>")

# CRC-32/IEEE (reflected, poly 0xEDB88320). crc32("123456789") == 0xCBF43926.
def crc32(data)
  crc = 0xFFFFFFFF
  data.each_byte do |b|
    crc ^= b
    8.times do
      mask = (crc & 1).zero? ? 0 : 0xFFFFFFFF
      crc = (crc >> 1) ^ (0xEDB88320 & mask)
    end
  end
  crc ^ 0xFFFFFFFF
end

# The per-page checksum (v7, format.md *Page header*): CRC-32/IEEE over a body page's bytes
# EXCLUDING its own 4-byte crc32 field at [12,16) — i.e. [0,12) then [16,ps). crc32 is linear over
# the byte stream, so checksumming the concatenation matches the cores' streaming page_crc exactly.
def page_crc(page)
  crc32(page.byteslice(0, 12) + page.byteslice(PAGE_HEADER, page.bytesize - PAGE_HEADER))
end

def meta_crc(page)
  crc32(page.byteslice(0, 32) + page.byteslice(36, page.bytesize - 36))
end

# CRC-64/ECMA-182: unreflected, init = xorout = 0. Deliberately independent
# bit-at-a-time reference; production cores may use byte tables or acceleration.
def crc64(data, crc = 0)
  data.each_byte do |byte|
    crc ^= byte << 56
    8.times do
      top = crc & 0x8000000000000000
      crc = (crc << 1) & 0xFFFFFFFFFFFFFFFF
      crc ^= 0x42F0E1EBA9EA3693 unless top.zero?
    end
  end
  crc
end

# int-be-signflip: add bias 2^(bits-1), emit unsigned big-endian (encoding.md).
def encode_int(width, value)
  bias = 1 << (width * 8 - 1)
  u = value + bias
  Array.new(width) { |i| (u >> (8 * (width - 1 - i))) & 0xFF }.pack("C*")
end

def decode_int(width, bytes)
  bytes.bytes.reduce(0) { |acc, b| (acc << 8) | b } - (1 << (width * 8 - 1))
end

# text-terminated-escape / bytea-terminated-escape (encoding.md §2.4/§2.6): escape every 0x00 to
# 0x00 0xFF, terminate with 0x00 0x01 — variable-width and self-delimiting. `content` is the value's
# raw bytes (UTF-8 for text, raw for bytea).
def enc_terminated(content)
  out = +"".b
  content.each_byte do |b|
    out << b
    out << 0xFF if b.zero?
  end
  out << 0x00 << 0x01
  out
end

# The BARE order-preserving KEY body for a decimal (decimal-order-preserving, encoding.md §2.5):
# class byte (03 neg / 04 zero / 05 pos), the 4-byte int-be-signflip exponent E, the base-100
# mantissa pairs (pair+1 ∈ [01,64]), and a 00 terminator; negatives complement E+mantissa+
# terminator. Independent of display scale — 1.5 and 1.50 coincide. `s` is the decimal string.
def encode_decimal_key(s)
  d = parse_decimal(s)
  return "\x04".b if d[:coeff].zero? # zero is the single class byte

  digits = d[:coeff].to_s             # significant digits (leading zeros already stripped)
  decpt = digits.length - d[:scale]   # value = 0.<digits> × 10^decpt
  digits = digits.sub(/0+\z/, "")     # drop trailing zero digits (decpt unchanged)
  e = (decpt + 1) / 2                  # Ruby integer / floors = ⌊(decpt+1)/2⌋
  grouped = decpt.odd? ? "0#{digits}" : digits.dup
  grouped << "0" if grouped.length.odd?
  body = +"".b
  body << encode_int(4, e)             # 4-byte order-preserving exponent
  grouped.chars.each_slice(2) { |a, b| body << ((a.to_i * 10 + b.to_i) + 1) }
  body << 0x00
  d[:neg] ? "\x03".b + body.bytes.map { |b| b ^ 0xFF }.pack("C*") : "\x05".b + body
end

# The BARE order-preserving KEY body for an interval (interval-span-i128, encoding.md §2.10): the
# 16-byte canonical span (span = (months·30 + days)·86_400_000_000 + micros), int-be-signflip at
# i128 width. `v` is the [months, days, micros] triple. Span-equal intervals coincide.
def encode_interval_key(v)
  m, d, us = v
  span = (m * 30 + d) * 86_400_000_000 + us
  encode_int(16, span)
end

# The BARE order-preserving KEY body for a range value (range-bounds, encoding.md §2.11) — the first
# container key. empty = 0x00 (the whole key); non-empty = 0x01 ‖ lower bound ‖ upper bound. Each
# bound is an infinity marker (−∞ 0x00 lower / +∞ 0x02 upper) or 0x01 ‖ the element's key_body ‖ an
# inclusivity byte (0x00 when inclusive == is_lower, else 0x01). `val` is :empty or the bound hash;
# `elem_type` names the element subtype (recursing into its key_body).
def encode_range_key(elem_type, val)
  return "\x00".b if val == :empty || (val.is_a?(Hash) && val[:empty])

  out = +"".b
  out << 0x01
  encode_range_key_bound(out, elem_type, val, :lower, true)
  encode_range_key_bound(out, elem_type, val, :upper, false)
  out
end

def encode_range_key_bound(out, elem_type, val, side, is_lower)
  v = val[side]
  if v.nil?
    out << (is_lower ? 0x00 : 0x02)
    return
  end
  out << 0x01
  out << key_body(elem_type, v)
  inc = val[is_lower ? :lower_inc : :upper_inc]
  out << ((inc == is_lower) ? 0x00 : 0x01)
end

# The BARE order-preserving KEY body for one present (non-NULL) value of `type` — no presence
# tag (callers add it for nullable index slots; a PK member is NOT NULL). uuid is the 16 raw
# bytes (uuid-raw16, §2.7), boolean a single bool-byte (0x00 false / 0x01 true, §2.9), text/bytea
# The collated text key body (text-collated-sortkey, encoding.md §2.12): the column collation's UCA
# sort key for `string`, whose memcmp order IS the collation order. Ruby has no collation compiler
# (the artifact is opaque, §collation_entry_bytes), so the reference reads the pinned cross-core
# sort-key bytes from spec/collation/vectors/sortkey.toml rather than recomputing them — the same
# bytes the cores emit (the sort key already appends the §2.4 C-key, so it is self-delimiting and
# total). A golden may only use (collation, string) pairs that vector pins.
def sortkey_vectors
  @sortkey_vectors ||= begin
    path = File.join(__dir__, "..", "collation", "vectors", "sortkey.toml")
    map = {}
    coll = nil
    str = nil
    File.foreach(path) do |line|
      if (m = line.match(/^coll_name\s*=\s*"(.*)"\s*$/))
        coll = m[1]
      elsif (m = line.match(/^string\s*=\s*"(.*)"\s*$/))
        str = m[1]
      elsif (m = line.match(/^sortkey_hex\s*=\s*"([0-9a-fA-F]*)"\s*$/))
        map[[coll, str]] = [m[1]].pack("H*").b
      end
    end
    map
  end
end

def collated_sort_key(coll_name, string)
  sortkey_vectors.fetch([coll_name, string.dup.force_encoding("UTF-8")]) do
    raise "no pinned sort-key vector for collation #{coll_name.inspect} string #{string.inspect}"
  end
end

# The BARE order-preserving KEY body for one keyable value: uuid the raw 16 bytes, text/bytea
# the variable-width …-terminated-escape body (§2.4/§2.6) — or, for a non-C collated text column,
# the UCA sort key (text-collated-sortkey §2.12) — decimal the decimal-order-preserving
# body (§2.5), interval the 16-byte interval-span-i128 span (§2.10), a range the recursive
# range-bounds container key (§2.11), every other keyable type the sign-flipped fixed-width int
# encoding (timestamps reuse the i64 rule). `collation` is the text column's frozen collation name
# (nil ⇒ C / non-text — the fast path).
def key_body(type, v, collation = nil)
  if (relem = range_elem(type))
    return encode_range_key(relem, v)
  end

  if (aelem = array_elem(type))
    return encode_array_key(aelem, v, collation)
  end

  if (cfields = composite_fields(type))
    return encode_composite_key(cfields, v)
  end

  return collated_sort_key(collation, v) if collation && type == "text"

  case type
  when "uuid" then uuid_to_bytes(v)
  when "boolean" then (v ? "\x01".b : "\x00".b)
  when "text" then enc_terminated(v.b)
  when "bytea" then enc_terminated(v.b)
  when "decimal" then encode_decimal_key(v)
  when "interval" then encode_interval_key(v)
  when "f64", "f32" then encode_float_key(type, v)
  else encode_int(WIDTH.fetch(type), v)
  end
end

# float-order-preserving (encoding.md §2.8): canonicalize the Ruby Float (-0 → +0, every NaN → one
# quiet pattern), take the IEEE bits as a big-endian unsigned integer, then if the sign bit is set
# flip ALL bits else flip just the sign bit — mapping the float TOTAL order onto unsigned byte
# order. f64 = 8 bytes, f32 = 4 (pack("g") rounds the value to binary32 first). This is the KEY
# form; the stored VALUE codec (encode_float64/encode_float32) preserves the bits verbatim (only
# NaN canonicalized) since a value never sorts.
def encode_float_key(type, f)
  if type == "f64"
    bits = f.nan? ? 0x7FF8000000000000 : (f == 0.0 ? 0 : [f].pack("G").unpack1("Q>"))
    bits ^= (bits >> 63) == 1 ? 0xFFFFFFFFFFFFFFFF : 0x8000000000000000
    [bits].pack("Q>")
  else # f32
    bits = f.nan? ? 0x7FC00000 : (f == 0.0 ? 0 : [f].pack("g").unpack1("N"))
    bits ^= (bits >> 31) == 1 ? 0xFFFFFFFF : 0x80000000
    [bits].pack("N")
  end
end

# value codec: presence tag + (when present) the type's body. 0x01 = NULL; 0x00 = present.
# Integers reuse the order-preserving int bytes; text and bytea diverge to a compact u16
# byte-length + bytes (text: UTF-8 collation-C bytes; bytea: raw bytes — byte-identical here,
# only the source encoding / read-side UTF-8 assertion differs); boolean is a single bool-byte
# 0x00 false / 0x01 true (format.md "Value codec").
# The composite types in scope for the codec, keyed by lowercased name → field list. Set per
# image by `build_image` (encode) and `decode_image` (decode) before any value is coded, so the
# recursive composite codec can resolve a column's / field's composite type by name. `{}` when no
# composite types are defined (every existing fixture).
$ctypes = {}

# The field list of the composite type named `type` (case-insensitive), or nil for a scalar type.
def composite_fields(type) = $ctypes[type.to_s]

def encode_value(type, v)
  return "\x01".b if v.nil?

  if (elem = array_elem(type))
    # An array value (spec/design/array.md §4): present tag, then the compact body.
    return "\x00".b + encode_array_body(elem, v)
  end

  if (relem = range_elem(type))
    # A range value (spec/design/ranges.md §4): present tag, then the flags byte + bound bodies.
    return "\x00".b + encode_range_body(relem, v)
  end

  if (fields = composite_fields(type))
    # A composite value (spec/design/composite.md §4): present tag, then the recursive body.
    return "\x00".b + encode_composite_body(fields, v)
  end

  case type
  when "text", "bytea", "json"
    # json stores the verbatim text body, length-prefixed exactly like text (spec/design/json.md §4).
    bytes = v.b
    "\x00".b + u16(bytes.bytesize) + bytes
  when "jsonb"
    # jsonb stores the present tag then the self-delimiting tagged-node tree (§2). The fixture value
    # is a pre-canonicalized node (tagged Ruby form — see encode_jsonb_body).
    "\x00".b + encode_jsonb_body(v)
  when "boolean"
    "\x00".b + (v ? "\x01".b : "\x00".b)
  when "decimal"
    "\x00".b + encode_decimal(v)
  when "uuid"
    "\x00".b + uuid_to_bytes(v) # fixed 16 bytes, NO length prefix
  when "interval"
    # fixed 16 bytes: i32 months, i32 days, i64 micros — big-endian two's-complement, no
    # sign-flip (a value codec, not a key). v is [months, days, micros].
    m, d, us = v
    "\x00".b + [m].pack("l>") + [d].pack("l>") + [us].pack("q>")
  when "f64"
    "\x00".b + encode_float64(v)
  when "f32"
    "\x00".b + encode_float32(v)
  else
    "\x00".b + encode_int(WIDTH.fetch(type), v)
  end
end

# An array value's BODY (after the 0x00 present tag, spec/design/array.md §4):
#   ndim u8 ‖ flags u8 ‖ per-dim (len u32 BE, lb i32 BE) ‖ [null bitmap if HAS_NULLS] ‖ element bodies
# An empty array is ndim=0 (no dims/bitmap/elements); otherwise ndim is the dimension count and each
# dimension records its length and lower bound (multidim + custom lower bounds — §12). The value is
# either a flat Array (1-D, lower bound 1) or a Hash {dims:, lbounds:, elements:} (a shaped value).
# The bitmap (MSB-first, like composite) is present iff any element is NULL (HAS_NULLS, flag bit 0);
# a NULL element contributes zero body bytes; a present element its value-codec body MINUS the tag.
def encode_array_body(elem_type, val)
  if val.is_a?(Hash)
    dims = val[:dims]
    lbounds = val[:lbounds]
    elems = val[:elements]
  else
    elems = val
    dims = elems.empty? ? [] : [elems.size]
    lbounds = elems.empty? ? [] : [1]
  end
  out = +"".b
  if elems.empty?
    out << [0].pack("C") << [0].pack("C") # ndim = 0 (empty array), flags 0
    return out
  end
  has_nulls = elems.any?(&:nil?)
  out << [dims.size].pack("C") # ndim
  out << [has_nulls ? 1 : 0].pack("C") # flags: bit 0 = HAS_NULLS
  dims.each_with_index { |d, i| out << u32(d) << [lbounds[i]].pack("l>") } # per-dim (len, lb i32 BE)
  if has_nulls
    nbytes = (elems.size + 7) / 8
    bitmap = Array.new(nbytes, 0)
    elems.each_with_index { |e, i| bitmap[i / 8] |= (0x80 >> (i % 8)) if e.nil? }
    out << bitmap.pack("C*")
  end
  elems.each { |e| out << encode_value(elem_type, e).byteslice(1..) unless e.nil? }
  out
end

# The order-preserving array-elements-terminated KEY (encoding.md §2.14 — the second container key,
# the first whose length varies with the element count): per flattened (row-major) element a marker
# (0x01 present ‖ the element's bare key_body, 0x02 NULL), then a 0x00 terminator, then the shape
# suffix (ndim, per-dim u32 BE length + i32 int-be-signflip lower bound). memcmp reproduces
# array_total_cmp (array.md §5). `val` is the array fixture value (a flat Array = 1-D lower bound 1,
# or a Hash {dims:, lbounds:, elements:}). The element is a key-encodable scalar (the DDL gate), so
# key_body encodes each present element — under `collation`, a collated text[] column's (v36), each
# text element's UCA sort key; nil is the C byte order.
def encode_array_key(elem, val, collation = nil)
  if val.is_a?(Hash)
    dims = val[:dims]
    lbounds = val[:lbounds]
    elems = val[:elements]
  else
    elems = val
    dims = elems.empty? ? [] : [elems.size]
    lbounds = elems.empty? ? [] : [1]
  end
  out = +"".b
  elems.each do |e|
    if e.nil?
      out << [0x02].pack("C") # NULL element — sorts after every present element
    else
      out << [0x01].pack("C") # present element marker
      out << key_body(elem, e, collation)
    end
  end
  out << [0x00].pack("C") # terminator — a shorter element list sorts before a longer one
  out << [dims.size].pack("C") # ndim
  dims.each_with_index { |d, i| out << u32(d) << encode_int(4, lbounds[i]) }
  out
end

# The composite-field-slots key for a composite value (encoding.md §2.15, the third container key).
# `fields` is the resolved field list ({name:, type:}), `vals` the per-field values (declaration
# order). Each field rides the ordinary §2.2 nullable slot — `0x00` present ‖ the field's own
# order-preserving key, or `0x01` NULL. A composite is FIXED-arity (field count known from the type),
# so it needs no terminator (unlike the variable-arity array above). Recurses through `key_body` for
# a nested composite / array / range field; a composite field carries no COLLATE, so the C byte order.
def encode_composite_key(fields, vals)
  out = +"".b
  fields.each_with_index do |f, i|
    if vals[i].nil?
      out << [0x01].pack("C") # NULL slot — sorts after every present field
    else
      out << [0x00].pack("C") # present slot
      out << key_body(f[:type], vals[i])
    end
  end
  out
end

# A range value's BODY (after the 0x00 present tag, spec/design/ranges.md §4):
#   flags u8 ‖ [lower bound body if !EMPTY && !LB_INF] ‖ [upper bound body if !EMPTY && !UB_INF]
# flags bits: 0 EMPTY, 1 LB_INF, 2 UB_INF, 3 LB_INC, 4 UB_INC (bits 5-7 reserved 0). An empty range
# is the lone flags byte 0x01 (no bounds). A present bound is the element's value-codec body MINUS
# the presence tag. The value is `:empty`, or a Hash {lower:, upper:, lower_inc:, upper_inc:} where a
# nil lower/upper is an infinite (unbounded) bound. The stored value is CANONICAL (the fixture
# carries the post-canonicalization `[)` form for discrete subtypes — §4).
def encode_range_body(elem_type, val)
  return "\x01".b if val == :empty || (val.is_a?(Hash) && val[:empty])

  flags = 0
  flags |= 0x02 if val[:lower].nil? # LB_INF
  flags |= 0x04 if val[:upper].nil? # UB_INF
  flags |= 0x08 if val[:lower_inc]  # LB_INC
  flags |= 0x10 if val[:upper_inc]  # UB_INC
  out = +"".b
  out << [flags].pack("C")
  out << encode_value(elem_type, val[:lower]).byteslice(1..) unless val[:lower].nil?
  out << encode_value(elem_type, val[:upper]).byteslice(1..) unless val[:upper].nil?
  out
end

# An unsigned LEB128 varint (7 bits/byte, high bit = continuation) — the jsonb count/length codec
# (spec/design/json.md §2.1).
def write_uvarint(v)
  out = +"".b
  loop do
    byte = v & 0x7f
    v >>= 7
    if v.zero?
      out << [byte].pack("C")
      return out
    end
    out << [byte | 0x80].pack("C")
  end
end

# A jsonb value's BODY (after the 0x00 present tag, spec/design/json.md §2.1): a self-delimiting
# depth-first tagged-node tree. The fixture node is a tagged Ruby form: [:null], [:bool, b],
# [:num, "canonical-decimal-string"], [:str, s], [:arr, [node, …]], [:obj, [[key, node], …]]
# (members ALREADY in canonical key order). Node tags (low nibble): 0 null, 1 false, 2 true,
# 3 number (the decimal body), 4 string (varint len ‖ utf8), 6 array (varint count ‖ children),
# 7 object (varint count ‖ members, each a string-node key ‖ value node).
def encode_jsonb_body(node)
  case node[0]
  when :null then "\x00".b
  when :bool then (node[1] ? "\x02".b : "\x01".b)
  when :num then "\x03".b + encode_decimal(node[1])
  when :str
    s = node[1].b
    "\x04".b + write_uvarint(s.bytesize) + s
  when :arr
    out = +"\x06".b
    out << write_uvarint(node[1].size)
    node[1].each { |child| out << encode_jsonb_body(child) }
    out
  when :obj
    out = +"\x07".b
    out << write_uvarint(node[1].size)
    node[1].each do |key, child|
      kb = key.b
      out << "\x04".b << write_uvarint(kb.bytesize) << kb
      out << encode_jsonb_body(child)
    end
    out
  else
    raise "bad jsonb node #{node.inspect}"
  end
end

# An array column's ELEMENT-TYPE descriptor (spec/design/array.md §3): the element's type code,
# then (for a composite element) its name. v1 elements are scalars. Mutates `out`.
def push_array_element_type(out, elem_type)
  if composite_fields(elem_type)
    out << [14].pack("C") << u16(elem_type.bytesize) << elem_type.b
  else
    out << [TYPECODE.fetch(elem_type)].pack("C")
  end
end

# Decode an array column's element-type descriptor (inverse of push_array_element_type) -> the
# element type STRING and the new cursor.
def read_array_element_type(buf, pos)
  cb, pos = take(buf, pos, 1)
  code = cb.getbyte(0)
  if code == 14
    nl, pos = take(buf, pos, 2)
    name, pos = take(buf, pos, nl.unpack1("n"))
    [name, pos]
  else
    [CODETYPE.fetch(code), pos]
  end
end

# A composite value's BODY (after the 0x00 present tag, spec/design/composite.md §4): a null bitmap
# of ceil(field_count/8) bytes (MSB-first — field i is bit 0x80>>(i%8) of byte i/8; a set bit =
# NULL), then each PRESENT field's value-codec body (no per-field tag) in declaration order. A NULL
# field contributes zero body bytes. `vals` is the field-value array (a nested composite is itself
# an array). Recurses for nested composites.
def encode_composite_body(fields, vals)
  nbytes = (fields.size + 7) / 8
  bitmap = Array.new(nbytes, 0)
  bodies = +"".b
  fields.each_with_index do |f, i|
    if vals[i].nil?
      bitmap[i / 8] |= (0x80 >> (i % 8))
    else
      bodies << encode_value(f[:type], vals[i]).byteslice(1..) # body, minus the presence tag
    end
  end
  bitmap.pack("C*").b + bodies
end

# Float value codec body (format.md code 12/13; spec/design/float.md §10): the IEEE bytes,
# big-endian, fixed width, no length prefix. Stored VERBATIM for every value except NaN — a -0.0
# keeps its sign bit (pack preserves it) and ±Inf/finite keep theirs — but a NaN is canonicalized
# to the single quiet pattern (0x7FF8…000 / 0x7FC00000), since a NaN's payload is core-specific and
# a stored NaN must be cross-core byte-identical. The -0→+0 collapse is a comparison/key concern,
# NOT applied here.
def encode_float64(f)
  return [0x7FF8000000000000].pack("Q>") if f.nan?

  [f].pack("G") # IEEE 754 double, big-endian
end

def encode_float32(f)
  return [0x7FC00000].pack("N") if f.nan?

  [f].pack("g") # IEEE 754 single, big-endian
end

# Parse a decimal STRING ("[-]int[.frac]") into (neg, scale, coefficient). The coefficient is a
# Ruby integer (arbitrary precision); scale is the fractional digit count. No negative zero.
def parse_decimal(s)
  neg = s.start_with?("-")
  body = neg ? s[1..] : s
  int, frac = body.split(".", 2)
  frac ||= ""
  coeff = (int + frac).to_i # leading zeros are harmless to_i
  { neg: neg && coeff != 0, scale: frac.length, coeff: coeff }
end

# Render (neg, scale, coefficient) to the canonical decimal string (spec/design/decimal.md §6).
def render_decimal(neg, scale, coeff)
  digits = coeff.to_s # "0" for zero, no leading zeros
  sign = neg ? "-" : ""
  return sign + digits if scale.zero?

  digits = digits.rjust(scale + 1, "0")
  point = digits.length - scale
  "#{sign}#{digits[0...point]}.#{digits[point..]}"
end

# Decimal value codec body (format.md): flags (bit0 sign), u16 scale, u16 ndigits, then that
# many big-endian base-10^4 coefficient groups, most-significant first. Zero carries no groups.
def encode_decimal(s)
  d = parse_decimal(s)
  groups = []
  c = d[:coeff]
  while c.positive?
    groups.unshift(c % 10_000)
    c /= 10_000
  end
  [d[:neg] ? 1 : 0].pack("C") + u16(d[:scale]) + u16(groups.size) + groups.map { |g| u16(g) }.join
end

# --- encoding (reference serializer) ----------------------------------------

# The primary-key member ordinals in KEY order (v5): an explicit pk_order when the fixture
# declares one (key order != declaration order), else the pk-flagged columns in declaration
# order. [] = no PK (synthetic rowid keys).
def pk_order(table)
  table[:pk_order] || table[:columns].each_index.select { |i| table[:columns][i][:pk] }
end

def table_entry_bytes(table, root_data_page, index_roots, row_count)
  out = +"".b
  out << u16(table[:name].bytesize) << table[:name].b
  out << u16(table[:columns].size)
  table[:columns].each do |c|
    out << u16(c[:name].bytesize) << c[:name].b
    # A composite column (v9): type_code 14, flags, then the type NAME in the typmod slot
    # (spec/design/composite.md §3). A composite DEFAULT is always an expression default (v44, flags
    # bit3): its expr-text follows the type name. Bit2 (a constant default) is never set.
    if composite_fields(c[:type])
      out << [14].pack("C")
      cflags = c[:not_null] ? 0b10 : 0
      cflags |= 0b1000 if c[:default_expr]
      out << [cflags].pack("C")
      out << u16(c[:type].bytesize) << c[:type].b
      out << u16(c[:default_expr].bytesize) << c[:default_expr].b if c[:default_expr]
      next
    end
    # An array column (v10): type_code 15, flags, then the element-type descriptor
    # (spec/design/array.md §3). Array columns carry no default this slice (bits 2/3 = 0). A text[]
    # column with a non-C collation sets bit6 has_collation (v36) and appends the name after the
    # element descriptor.
    if (elem = array_elem(c[:type]))
      out << [15].pack("C")
      aflags = c[:not_null] ? 0b10 : 0
      aflags |= 0b100_0000 if c[:collation]
      out << [aflags].pack("C")
      push_array_element_type(out, elem)
      out << u16(c[:collation].bytesize) << c[:collation].b if c[:collation]
      next
    end
    # A range column (v15): type_code 17, flags, then the element-type descriptor — one scalar
    # code (spec/design/ranges.md §3). Range columns carry no default this slice (bits 2/3 = 0).
    if (relem = range_elem(c[:type]))
      out << [17].pack("C")
      out << [c[:not_null] ? 0b10 : 0].pack("C")
      out << [TYPECODE.fetch(relem)].pack("C")
      next
    end
    out << [TYPECODE.fetch(c[:type])].pack("C")
    has_default = c[:default] != :none
    has_default_expr = !c[:default_expr].nil?
    # bit0 (primary_key through v4) is RETIRED in v5 — the pk ordinal list below is the
    # single authority; the bit is reserved, written 0 (format.md).
    flags = 0
    flags |= 0b10 if c[:not_null]
    flags |= 0b100 if has_default
    # bit3 default_is_expr (v8) — mutually exclusive with bit2 (format.md).
    flags |= 0b1000 if has_default_expr
    # bit4 is_identity + bit5 identity_always (v15) — an IDENTITY column (sequences.md §13). An
    # identity column also carries not_null (bit1) + the nextval expression default (bit3).
    if c[:identity]
      flags |= 0b1_0000
      flags |= 0b10_0000 if c[:identity] == :always
    end
    # bit6 has_collation (v17) — a text column with a non-C effective collation (collation.md §5);
    # the name is appended last.
    flags |= 0b100_0000 if c[:collation]
    out << [flags].pack("C")
    # A decimal column appends its typmod (precision, scale) — only for type_code 6, so
    # non-decimal entries are byte-unchanged. precision 0 = unconstrained numeric.
    out << u16(c[:precision] || 0) << u16(c[:scale] || 0) if c[:type] == "decimal"
    # A text column appends its varchar(n) max length (u32) — only for type_code 4 (v22). 0 =
    # unbounded (spec/design/types.md §15).
    out << u32(c[:varchar_len] || 0) if c[:type] == "text"
    # A timestamp/timestamptz/interval column appends its precision / qualifier typmod (v43).
    out << temporal_typmod_bytes(c[:type], c[:temporal])
    # A column with a constant DEFAULT (flags bit2) appends its pre-evaluated default value via
    # the same value codec rows use — AFTER the typmod, presence-gated. A DEFAULT NULL is one
    # 0x01. An EXPRESSION default (flags bit3, v8) instead appends its expr-text (u16 length +
    # UTF-8) there, the same token rendering a CHECK uses — bit2/bit3 are exclusive.
    if has_default
      out << encode_value(c[:type], c[:default])
    elsif has_default_expr
      out << u16(c[:default_expr].bytesize) << c[:default_expr].b
    end
    # The effective collation name (v17, flags bit6) — last in the per-column entry (collation.md §5).
    out << u16(c[:collation].bytesize) << c[:collation].b if c[:collation]
  end
  # The primary key (v5): count, then the member column ordinals in KEY order.
  pk = pk_order(table)
  out << u16(pk.size)
  pk.each { |i| out << u16(i) }
  # CHECK constraints (v4): count, then (name, expression text) per check, in the catalog's
  # evaluation order — ascending byte order of the lowercased name (constraints.md §4.4/§4.5).
  checks = table[:checks] || []
  out << u16(checks.size)
  checks.each do |ck|
    out << u16(ck[:name].bytesize) << ck[:name].b
    out << u16(ck[:expr].bytesize) << ck[:expr].b
    # v35: check_flags (bit0 has_host_deps), then the dependency list when bit0 is set.
    check_deps = ck[:host_deps] || []
    out << [check_deps.empty? ? 0 : 1].pack("C")
    write_host_deps(out, check_deps) unless check_deps.empty?
  end
  # Secondary indexes (v5): count, then per index the name, key-column ordinals (index-key
  # order), the v6 flags byte (bit0 unique — indexes.md §8), and the index tree's root
  # page — in ascending lowercased-name order (indexes.md).
  indexes = table[:indexes] || []
  out << u16(indexes.size)
  indexes.each_with_index do |ix, k|
    out << u16(ix[:name].bytesize) << ix[:name].b
    out << u16(ix[:cols].size)
    # Each key element is a column ordinal (u16) OR, for an EXPRESSION key (v26 — indexes.md §6),
    # the 0xFFFF sentinel + u16 length + canonical text. A fixture writes an expression element as
    # a Hash `{ expr: "<canonical text>" }`.
    ix[:cols].each do |c|
      if c.is_a?(Hash)
        out << u16(0xFFFF)
        out << u16(c[:expr].bytesize) << c[:expr].b
      else
        out << u16(c)
      end
    end
    # index_flags: bit0 unique (v6), bit1 has_predicate (v27 — a partial index, indexes.md §9),
    # bit2 has_host_deps (v31 — a host-function index dependency, extensibility.md §8.1).
    host_deps = ix[:host_deps] || []
    timezone_deps = ix[:timezone_deps]
    has_timezone_deps = timezone_deps && (timezone_deps[:dynamic] || !timezone_deps[:zones].empty?)
    orders = index_orders(ix)
    has_key_options = orders.any? { |o| o != [false, false] }
    # bit5 is_constraint (v50 — the unique index backs a UNIQUE constraint, constraints.md §5.5).
    out << [(ix[:unique] ? 1 : 0) | (ix[:predicate] ? 2 : 0) | (host_deps.empty? ? 0 : 4) | (has_timezone_deps ? 8 : 0) |
            (has_key_options ? 16 : 0) | (ix[:constraint] ? 32 : 0)].pack("C")

    # v13: index_kind byte (0 = btree, 1 = GIN); v20: 2 = GiST (gist.md §8).
    out << [{ "gin" => 1, "gist" => 2 }.fetch(ix[:kind], 0)].pack("C")
    out << ptr(index_roots[k])
    # v27: a partial index's predicate canonical text (u16 len + UTF-8) after index_root_page.
    out << u16(ix[:predicate].bytesize) << ix[:predicate].b if ix[:predicate]
    # v31: the host-function dependency list (extensibility.md §8.1) after the predicate — only when
    # bit2 is set. Already in ascending (name, arg-type codes) order (a fixture writes them sorted).
    write_host_deps(out, host_deps) unless host_deps.empty?
    if has_timezone_deps
      out << [timezone_deps[:dynamic] ? 1 : 0].pack("C") << u16(timezone_deps[:zones].size)
      timezone_deps[:zones].each do |dep|
        out << u16(dep[:name].bytesize) << dep[:name].b
        out << u16(dep[:version].bytesize) << dep[:version].b
        out << u32(dep[:checksum])
      end
    end
    # v46: one key-option byte per element (bit0 desc, bit1 nulls_first) — only when bit4 is set.
    orders.each { |desc, nf| out << [(desc ? 1 : 0) | (nf ? 2 : 0)].pack("C") } if has_key_options
  end
  # Foreign keys (v11): count, then per FK the name, the local-column ordinals (into THIS table,
  # list order), the referenced table name, the referenced-column ordinals (into the PARENT
  # table, list order), and the actions byte (v30: bits 0-2 on_delete, bits 3-5 on_update; 0 = NO
  # ACTION, 1 = RESTRICT, 2 = CASCADE, 3 = SET NULL, 4 = SET DEFAULT; v40: bit 6 match_full). In
  # ascending name byte order (constraints.md §6.9). An FK owns no B-tree, so no root page.
  fks = table[:fks] || []
  out << u16(fks.size)
  fks.each do |fk|
    out << u16(fk[:name].bytesize) << fk[:name].b
    out << u16(fk[:local].size)
    fk[:local].each { |i| out << u16(i) }
    out << u16(fk[:ref_table].bytesize) << fk[:ref_table].b
    out << u16(fk[:ref].size)
    fk[:ref].each { |i| out << u16(i) }
    out << [fk[:actions] || 0].pack("C")
  end
  # EXCLUDE constraints (v21): count, then per exclusion the name, the backing GiST index name, and
  # the (column ordinal u16, operator strategy u8) element vector (&& = 0, = 1), in ascending
  # lowercased-name order (spec/design/gist.md §7/§8). The backing index is stored like any GiST
  # index (in the index list above); this entry layers the operator vector the probe needs.
  exclusions = table[:exclusions] || []
  out << u16(exclusions.size)
  exclusions.each do |ex|
    out << u16(ex[:name].bytesize) << ex[:name].b
    out << u16(ex[:index].bytesize) << ex[:index].b
    out << u16(ex[:elements].size)
    ex[:elements].each do |col, op|
      out << u16(col) << [{ "&&" => 0, "=" => 1 }.fetch(op)].pack("C")
    end
  end
  out << ptr(root_data_page)
  raise "row_count must fit nonnegative i64" unless row_count.between?(0, (1 << 63) - 1)
  raise "root_data_page/row_count invariant violated" unless root_data_page.zero? == row_count.zero?

  out << [row_count].pack("q>")
  out
end

# Serialize one table's v29 kind-4 column-statistics groups. The caller already emits tables in name
# order; columns are sorted here, and each group is summary, MCV ordinals, then histogram ordinals.
def statistics_entries(table)
  table_statistics(table).sort_by { |s| s[:column] }.flat_map do |s|
    col = table[:columns].fetch(s[:column])
    distribution = !s[:distinct_count].nil?
    summary = +"\x04\x00".b
    summary << u16(table[:name].bytesize) << table[:name].b << u16(s[:column])
    terms = s[:terms]
    summary << [(s[:stale] ? 1 : 0) | (distribution ? 2 : 0) | (terms ? 4 : 0)].pack("C")
    summary << [s[:analyzed_rows], s[:null_count], s[:width_sum], s[:distinct_count] || 0].pack("q>q>q>q>")
    summary << u32(s[:sample_rows]) << u32(s[:sample_nonnull_rows])
    summary << u16(s[:mcv].size) << u16(s[:histogram].size)
    summary << [terms[:occurrences], terms[:untracked_rows]].pack("q>q>") << u16(terms[:entries].size) if terms
    out = [summary]
    s[:mcv].each_with_index do |(value, frequency), ordinal|
      encoded = encode_value(col[:type], value)
      item = +"\x04\x01".b
      item << u16(table[:name].bytesize) << table[:name].b << u16(s[:column])
      item << u16(ordinal) << u32(frequency) << u16(encoded.bytesize) << encoded
      out << item
    end
    s[:histogram].each_with_index do |value, ordinal|
      encoded = encode_value(col[:type], value)
      item = +"\x04\x02".b
      item << u16(table[:name].bytesize) << table[:name].b << u16(s[:column])
      item << u16(ordinal) << u16(encoded.bytesize) << encoded
      out << item
    end
    (terms ? terms[:entries] : []).each_with_index do |(term, rows), ordinal|
      item = +"\x04\x03".b
      item << u16(table[:name].bytesize) << table[:name].b << u16(s[:column])
      item << u16(ordinal) << [rows].pack("q>") << u16(term.bytesize) << term.b
      out << item
    end
    out
  end
end

# A table's statistics facts: written out, or (term_statistics_columns) derived from its rows.
def table_statistics(table)
  table[:statistics] || (table[:term_statistics_columns] || []).map { |c| term_column_statistics(table, c) }
end

# The GIN-admitted array element types (gin.md §3); with jsonb, the term-eligible column types.
GIN_ELEMENT_TYPES = %w[i16 i32 i64 boolean uuid date timestamp timestamptz f32 f64 interval text bytea
                       decimal].freeze
def term_eligible?(type) = type == "jsonb" || GIN_ELEMENT_TYPES.include?(array_elem(type))

# One value's GIN term set (gin.md §4): distinct terms in ascending byte order; NULL has none.
def gin_value_terms(type, value)
  return [] if value.nil?
  return gin_jsonb_terms(value).map(&:b).uniq.sort if type == "jsonb"

  elems = value.is_a?(Hash) ? value[:elements] : value
  elems.compact.map { |e| gin_element_term(array_elem(type), e).b }.uniq.sort
end

TERM_BUCKET_WIDTH = 10_000
TERM_ENTRIES = 100

# A column's GIN term facts (statistics.md §4.5), computed independently of the cores: lossy counting
# over every row's term set, rows in storage-key order, bucket width 10,000; the 100 most common
# terms by descending count then ascending bytes; the untracked-row bound.
def term_statistics(table, column)
  type = table[:columns][column][:type]
  occurrences = 0
  entries = {}
  pruned = false
  oversized = false
  table_entries(table).each do |_key, row|
    gin_value_terms(type, row[column]).each do |term|
      occurrences += 1
      bucket = (occurrences + TERM_BUCKET_WIDTH - 1) / TERM_BUCKET_WIDTH
      if term.bytesize > 128
        oversized = true
      elsif entries.key?(term)
        entries[term][0] += 1
      else
        entries[term] = [1, bucket - 1]
      end
      next unless (occurrences % TERM_BUCKET_WIDTH).zero?

      before = entries.size
      entries.reject! { |_, (count, delta)| count + delta <= bucket }
      pruned ||= entries.size != before
    end
  end
  ranked = entries.map { |term, (count, _)| [term, count] }.sort_by { |term, count| [-count, term] }
  untracked = 0
  untracked = [untracked, ranked[TERM_ENTRIES][1]].max if ranked.size > TERM_ENTRIES
  untracked = [untracked, (occurrences + TERM_BUCKET_WIDTH - 1) / TERM_BUCKET_WIDTH].max if pruned
  untracked = [untracked, 1].max if oversized
  { occurrences: occurrences, untracked_rows: untracked, entries: ranked.first(TERM_ENTRIES) }
end

# A term-eligible column's complete v42 statistics fact, derived from the rows (statistics.md §4):
# every row is sampled (the fixture is under 30,000 rows), the width is the value-body length, and
# array / jsonb columns are distribution-ineligible (no NDV, MCV, or histogram).
def term_column_statistics(table, column)
  type = table[:columns][column][:type]
  values = table_entries(table).map { |_key, row| row[column] }
  nonnull = values.compact
  { column: column, stale: false, analyzed_rows: values.size, null_count: values.size - nonnull.size,
    width_sum: nonnull.sum { |v| encode_value(type, v).bytesize - 1 }, distinct_count: nil,
    sample_rows: values.size, sample_nonnull_rows: nonnull.size, mcv: [], histogram: [],
    terms: term_statistics(table, column) }
end

# Serialize a composite-type catalog entry's BODY (after the entry_kind=1 byte), v9: name, field
# count, then per field — name, type code, [type name when code 14 (nested composite), the element
# descriptor when code 15 (array, v10) or 17 (range, v41)], flags
# (bit0 not_null), [decimal typmod when code 6] (format.md *Composite-type entry*).
def composite_type_entry_bytes(ct)
  out = +"".b
  out << u16(ct[:name].bytesize) << ct[:name].b
  out << u16(ct[:fields].size)
  ct[:fields].each do |f|
    out << u16(f[:name].bytesize) << f[:name].b
    if (elem = array_elem(f[:type]))
      # An array-typed field (spec/design/array.md §12): type_code 15 + the inline element-type
      # descriptor (§3), before the flags byte — mirroring where a nested-composite name sits.
      out << [15].pack("C")
      push_array_element_type(out, elem)
    elsif (relem = range_elem(f[:type]))
      # A range-typed field (v41, composite.md §3): type_code 17 + the one-byte range element
      # descriptor (ranges.md §3), before the flags byte. A range takes no typmod.
      out << [17].pack("C") << [TYPECODE.fetch(relem)].pack("C")
    elsif TYPECODE.key?(f[:type])
      out << [TYPECODE.fetch(f[:type])].pack("C")
    else
      out << [14].pack("C") << u16(f[:type].bytesize) << f[:type].b # nested composite, by name
    end
    out << [f[:not_null] ? 1 : 0].pack("C")
    out << u16(f[:precision] || 0) << u16(f[:scale] || 0) if f[:type] == "decimal"
    # A text field appends its varchar(n) max length (u32, v22); 0 = unbounded (types.md §15).
    out << u32(f[:varchar_len] || 0) if f[:type] == "text"
    # A timestamp/timestamptz/interval field appends its typmod (v43).
    out << temporal_typmod_bytes(f[:type], f[:temporal])
  end
  out
end

# Serialize a sequence catalog entry's BODY (after the entry_kind=2 byte): name, then six fixed i64
# fields (big-endian two's-complement, no sign-flip — `q>`) and a flags byte (bit0 cycle, bit1
# is_called, bit2 has_owner — v13). When has_owner, the flags byte is followed by the owner reference
# — owner table name (u16 len + bytes) then owner column ordinal (u16); a non-owned sequence writes
# nothing after the flags byte (spec/design/sequences.md §3/§12, format.md *Sequence entry*).
def sequence_entry_bytes(s)
  out = +"".b
  out << u16(s[:name].bytesize) << s[:name].b
  out << [s[:increment]].pack("q>")
  out << [s[:min_value]].pack("q>")
  out << [s[:max_value]].pack("q>")
  out << [s[:start]].pack("q>")
  out << [s[:cache]].pack("q>")
  out << [s[:last_value]].pack("q>")
  flags = 0
  flags |= 0b1 if s[:cycle]
  flags |= 0b10 if s[:is_called]
  flags |= 0b100 if s[:owned_by]
  out << [flags].pack("C")
  if (o = s[:owned_by])
    out << u16(o[0].bytesize) << o[0].b
    out << u16(o[1])
  end
  out
end

# Serialize a view catalog entry's BODY (after the entry_kind=5 byte, v38 — format.md *View entry*):
# the name; the output columns, each its name then (v48) a flags byte (bit0 has_default) and, when
# set, the default's text (u32 length + canonical text); the definition (u32 length + canonical token
# text); the dependency list in (name, kind) byte order — per dependency a kind byte (0 table, 1 view,
# v48: 2 sequence, 3 composite type), the name, and a table's referenced column names in byte order
# (none for any other kind); (v39) the check_option byte; (v48) the view flags byte (bit0
# security_barrier, bit1 security_invoker).
def view_entry_bytes(v)
  str = ->(x) { u16(x.bytesize) + x.b }
  out = str.(v[:name])
  out << u16(v[:columns].size)
  defaults = v[:defaults] || {}
  v[:columns].each do |c|
    out << str.(c)
    d = defaults[c]
    if d
      out << [1].pack("C") << u32(d.bytesize) << d.b
    else
      out << [0].pack("C")
    end
  end
  out << u32(v[:definition].bytesize) << v[:definition].b
  deps = v[:deps].sort_by { |d| [d[:name].b, d[:kind]] }
  out << u16(deps.size)
  deps.each do |d|
    out << [d[:kind]].pack("C") << str.(d[:name])
    cols = d[:columns].sort
    out << u16(cols.size)
    cols.each { |c| out << str.(c) }
  end
  out << [v[:check_option] || 0].pack("C")
  out << [(v[:security_barrier] ? 1 : 0) | (v[:security_invoker] ? 2 : 0)].pack("C")
  out.b
end

# Decode a view catalog entry's body (inverse of view_entry_bytes); the caller consumed the
# entry_kind byte.
def decode_view_entry(buf, pos)
  name, pos = take_str(buf, pos)
  ncols, pos = take(buf, pos, 2)
  columns = []
  defaults = {}
  ncols.unpack1("n").times do
    c, pos = take_str(buf, pos)
    columns << c
    fb, pos = take(buf, pos, 1)
    case fb.getbyte(0)
    when 0 then nil
    when 1
      dl, pos = take(buf, pos, 4)
      d, pos = take(buf, pos, dl.unpack1("N"))
      defaults[c] = d.force_encoding("UTF-8")
    else raise "unknown view column flags"
    end
  end
  dl, pos = take(buf, pos, 4)
  definition, pos = take(buf, pos, dl.unpack1("N"))
  nd, pos = take(buf, pos, 2)
  deps = []
  nd.unpack1("n").times do
    kb, pos = take(buf, pos, 1)
    kind = kb.getbyte(0)
    raise "unknown view dependency kind" unless [0, 1, 2, 3].include?(kind)

    dname, pos = take_str(buf, pos)
    nc, pos = take(buf, pos, 2)
    cols = []
    nc.unpack1("n").times do
      c, pos = take_str(buf, pos)
      cols << c
    end
    raise "a non-table view dependency lists columns" if kind != 0 && !cols.empty?

    deps << { kind: kind, name: dname, columns: cols }
  end
  cb, pos = take(buf, pos, 1)
  check_option = cb.getbyte(0)
  raise "unknown view check_option" unless [0, 1, 2].include?(check_option)

  fl, pos = take(buf, pos, 1)
  flags = fl.getbyte(0)
  raise "unknown view flags" unless flags & ~3 == 0

  [{ name: name, columns: columns, defaults: defaults, definition: definition.force_encoding("UTF-8"),
     deps: deps, check_option: check_option, security_barrier: flags & 1 != 0,
     security_invoker: flags & 2 != 0 }, pos]
end

# The view content a fixture should decode to (name-sorted, dependencies and their columns sorted).
def expected_views(fx)
  (fx[:views] || []).sort_by { |v| v[:name] }.map do |v|
    { name: v[:name], columns: v[:columns], defaults: v[:defaults] || {}, definition: v[:definition],
      deps: v[:deps].sort_by { |d| [d[:name].b, d[:kind]] }
                    .map { |d| { kind: d[:kind], name: d[:name], columns: d[:columns].sort } },
      check_option: v[:check_option] || 0, security_barrier: v[:security_barrier] || false,
      security_invoker: v[:security_invoker] || false }
  end
end

# Serialize a collation reference entry's BODY (after its entry_kind = 3 byte, v18): a flags byte
# (bit0 is_default), then the metadata — name + unicode_version + cldr_version + description, each
# u16-len + UTF-8. NO table: it is vendored into the binary and resolved by name on open
# (spec/design/collation.md §2/§5/§9, format.md *Collation entry*).
def collation_entry_bytes(c)
  out = +"".b
  out << [c[:default] ? 0b1 : 0].pack("C")
  [c[:name], c[:unicode], c[:cldr], c[:desc]].each { |s| out << u16(s.bytesize) << s.b }
  out
end

# Decode a collation reference entry's body (inverse of collation_entry_bytes); the caller has
# consumed the entry_kind byte. Reads the metadata only — the table is not in the file.
def decode_collation_entry(buf, pos)
  fb, pos = take(buf, pos, 1)
  f = fb.getbyte(0)
  raise "reserved collation flag set" if (f & ~0b1) != 0

  name, pos = take_str(buf, pos)
  unicode, pos = take_str(buf, pos)
  cldr, pos = take_str(buf, pos)
  desc, pos = take_str(buf, pos)
  [{ name: name, default: (f & 0b1) != 0, unicode: unicode, cldr: cldr, desc: desc }, pos]
end

# (key, row) pairs in stored (encoded-key) order. PK tables key on the PK member columns —
# the key is the CONCATENATION of the members' encodings in KEY order (a composite
# PRIMARY KEY, ../design/encoding.md §2.3; a single-column key is the one-member case).
# A no-PK table keys on a synthetic i64 rowid = insertion index (executor.rs).
def table_entries(table)
  pk_idxs = pk_order(table)
  pairs = table[:rows].each_with_index.map do |row, i|
    key = if pk_idxs.empty?
            encode_int(8, i)
          else
            pk_idxs.map do |pi|
              key_body(table[:columns][pi][:type], row[pi], table[:columns][pi][:collation])
            end.join.b
          end
    [key, row]
  end
  pairs.sort_by { |key, _| key } # String#<=> is bytewise == memcmp order
end

# A secondary-index entry key (indexes.md §3): each indexed column as the encoding.md §2.2
# NULLABLE SLOT (0x00 + bare order-preserving bytes when present, the lone 0x01 for NULL —
# always tagged, even for a NOT NULL column), then the row's storage key as the suffix.
# An index's per-element [desc, nulls_first] orders (indexes.md §11): a fixture's `orders:` list of
# :asc / :desc / :asc_nulls_first / :desc_nulls_last symbols, default ASC NULLS LAST everywhere.
ORDER_SYMBOLS = { asc: [false, false], asc_nulls_first: [false, true],
                  desc: [true, true], desc_nulls_last: [true, false] }.freeze

def index_orders(ix)
  ix[:cols].each_index.map { |i| ORDER_SYMBOLS.fetch((ix[:orders] || [])[i] || :asc) }
end

# The encoding.md §2.16 ordered slot tags: [present, null] per [desc, nulls_first].
def ordered_slot_tags(desc, nulls_first)
  { [false, false] => [0x00, 0x01], [false, true] => [0x01, 0x00],
    [true, true] => [0xFF, 0xFE], [true, false] => [0xFE, 0xFF] }.fetch([desc, nulls_first])
end

def index_entry_key(table, ix, storage_key, row)
  out = +"".b
  orders = index_orders(ix)
  ix[:cols].each_with_index do |ci, i|
    desc, nulls_first = orders[i]
    present, null = ordered_slot_tags(desc, nulls_first)
    v = row[ci]
    if v.nil?
      out << [null].pack("C")
    else
      out << [present].pack("C")
      # A fixture may force a non-collation ("C" / byte-order) key encoding for an index over a
      # COLLATE-annotated column via `key_collation: "C"`, to author a version-SKEWED index whose
      # stored order is deliberately WRONG for the loaded collation (the skew read-safety regression
      # fixture, spec/design/collation.md §12/§14). Absent the override, the index keys use the
      # column's frozen collation, as every other fixture does.
      coll = ix.key?(:key_collation) ? ix[:key_collation] : table[:columns][ci][:collation]
      coll = nil if coll == "C"
      body = key_body(table[:columns][ci][:type], v, coll)
      # A DESC element's body is bitwise inverted (§2.16).
      body = body.bytes.map { |b| b ^ 0xFF }.pack("C*") if desc
      out << body
    end
  end
  out << storage_key
  out
end

# Whether a (partial) index holds `row`: a fixture's `where:` lambda mirrors the index's SQL predicate
# (indexes.md §9, gist.md §7.1); an index without one holds every row.
def index_admits?(ix, row)
  ix[:where].nil? || ix[:where].call(row)
end

# An index's (entry_key, record-plan) pairs in entry-key order. An index record has an EMPTY
# payload (key only — format.md "Index trees"), so its plan is trivially inline. `forged_empty: true`
# authors an index whose tree is empty although the table has rows: the stand-in for an index a newer
# jed maintained under text this reference cannot evaluate (unparsable_definitions.jed). A reader that
# wrongly trusts it loses rows.
def index_entries(table, ix)
  return [] if ix[:forged_empty]
  table_entries(table).select { |_k, row| index_admits?(ix, row) }.map do |storage_key, row|
    key = index_entry_key(table, ix, storage_key, row)
    [key, { key: key, row: [], table: { columns: [] }, forms: [], comps: [], size: key.bytesize }]
  end.sort_by { |key, _| key }
end

# A GIN index's (entry_key, record-plan) pairs in entry-key order (spec/design/gin.md §4): one
# entry per DISTINCT non-NULL array element — encode(element) ‖ storage_key, with NO presence tag
# (a term is never NULL) and an EMPTY payload. A NULL whole-value array and an empty array yield no
# entries (they appear in no posting list). Terms dedup by their ENCODED bytes, not Ruby value
# equality: a float term's key canonicalizes -0 → +0 and every NaN (encoding.md §2.8), so {-0,0} and
# {NaN,NaN} are one term each (Ruby's uniq would keep two distinct NaN objects).
#
# A MULTI-column GIN index (v47, gin.md §4.4) prefixes each column's terms with its 0-based key position
# as one byte: column_tag ‖ term ‖ storage_key. A single-column index has no tag.
#
# Since v47 the tree's records are POSTING RUNS (gin.md §4.5): each term's storage keys, ascending,
# packed greedily into run keys of at most GIN_RUN_MAX_BYTES (a run of one key may be longer). A run
# key is term ‖ last ‖ body ‖ u16 BE len(body), the body front-coding every key but the last, in
# ascending order, as varint(shared) ‖ varint(suffix length) ‖ suffix against the previous key (the
# first against the last). A from-scratch image is this greedy packing.
GIN_RUN_MAX_BYTES = 64

def gin_varint(v)
  out = "".b
  loop do
    byte = v & 0x7F
    v >>= 7
    return out << (v.zero? ? byte : byte | 0x80).chr if v.zero?

    out << (byte | 0x80).chr
  end
end

def gin_shared_prefix(a, b)
  n = 0
  n += 1 while n < a.bytesize && n < b.bytesize && a.getbyte(n) == b.getbyte(n)
  n
end

def gin_encode_run(term, keys)
  last = keys.last
  body = "".b
  prev = last
  keys[0...-1].each do |k|
    shared = gin_shared_prefix(prev, k)
    body << gin_varint(shared) << gin_varint(k.bytesize - shared) << k.byteslice(shared..)
    prev = k
  end
  (term + last + body + [body.bytesize].pack("n")).b
end

def gin_pack_runs(term, keys)
  runs = []
  start = 0
  while start < keys.size
    stop = start + 1
    stop += 1 while stop < keys.size && gin_encode_run(term, keys[start..stop]).bytesize <= GIN_RUN_MAX_BYTES
    runs << gin_encode_run(term, keys[start...stop])
    start = stop
  end
  runs
end

def gin_index_entries(table, ix)
  postings = Hash.new { |h, k| h[k] = [] }
  ix[:cols].each_with_index do |ci, pos|
    col_type = table[:columns][ci][:type]
    elem_type = array_elem(col_type)
    raise "a GIN index column must be an array or jsonb" unless elem_type || col_type == "jsonb"
    tag = ix[:cols].size > 1 ? [pos].pack("C") : "".b
    table_entries(table).each do |storage_key, row|
      av = row[ci]
      next if av.nil? # a NULL array / document yields no terms
      terms = if col_type == "jsonb"
                gin_jsonb_terms(av)
              else
                elems = av.is_a?(Hash) ? av[:elements] : av
                elems.compact.map { |e| gin_element_term(elem_type, e) }
              end
      terms.uniq.each { |term| postings[(tag + term).b] << storage_key.b }
    end
  end
  postings.keys.sort.flat_map do |term|
    gin_pack_runs(term, postings[term].sort).map do |key|
      [key, { key: key, row: [], table: { columns: [] }, forms: [], comps: [], size: key.bytesize }]
    end
  end.sort_by { |key, _| key }
end

# An array_ops term (spec/design/gin.md §4/§4.2/§4.3): the element's key body, a text or bytea
# element's over its first 128 bytes (the C key — never a collation sort key; the cut is at a byte),
# and a decimal element's value-based key cut to its first 128 bytes.
def gin_element_term(elem_type, e)
  case elem_type
  when "text", "bytea" then enc_terminated(e.b.byteslice(0, GIN_JSONB_MAX_TERM_BYTES))
  when "decimal" then key_body(elem_type, e).byteslice(0, GIN_JSONB_MAX_TERM_BYTES)
  else key_body(elem_type, e)
  end
end

# GIN `jsonb_ops` index terms of a tagged jsonb node (spec/design/gin.md §2.1/§4.1): a KEY term
# (flag 0x01) per object key and per string array element (a top-level string included), a value
# term per other scalar — 0x02 null, 0x03 boolean (+ 0x00/0x01), 0x04 number, 0x05 string member
# value. A key/string body is text-terminated-escape over the UTF-8, a number body the same over its
# decimal-order-preserving key; a source over 128 bytes is cut to 128 and sets flag bit 0x10.
GIN_JSONB_MAX_TERM_BYTES = 128

def gin_jsonb_framed(flag, content)
  content = content.b
  if content.bytesize > GIN_JSONB_MAX_TERM_BYTES
    flag |= 0x10
    content = content.byteslice(0, GIN_JSONB_MAX_TERM_BYTES)
  end
  [flag].pack("C") + enc_terminated(content)
end

def gin_jsonb_scalar_term(node, as_key)
  case node[0]
  when :null then "\x02".b
  when :bool then node[1] ? "\x03\x01".b : "\x03\x00".b
  when :num then gin_jsonb_framed(0x04, encode_decimal_key(node[1]))
  when :str then gin_jsonb_framed(as_key ? 0x01 : 0x05, node[1])
  else raise "not a jsonb scalar #{node.inspect}"
  end
end

def gin_jsonb_container?(node) = %i[obj arr].include?(node[0])

def gin_jsonb_walk(node, out)
  case node[0]
  when :obj
    node[1].each do |k, v|
      out << gin_jsonb_framed(0x01, k)
      gin_jsonb_container?(v) ? gin_jsonb_walk(v, out) : out << gin_jsonb_scalar_term(v, false)
    end
  when :arr
    node[1].each do |e|
      gin_jsonb_container?(e) ? gin_jsonb_walk(e, out) : out << gin_jsonb_scalar_term(e, e[0] == :str)
    end
  end
end

# The document's deduplicated term set, in ascending byte order.
def gin_jsonb_terms(node)
  out = []
  if gin_jsonb_container?(node)
    gin_jsonb_walk(node, out)
  else
    out << gin_jsonb_scalar_term(node, node[0] == :str) # a raw scalar is a one-element array
  end
  out.uniq.sort
end

# --- GiST R-tree (spec/design/gist.md §3/§4.1) -------------------------------
#
# The on-disk form of a GiST index is a persisted R-tree (page types 5/6), NOT the flat leaf-key
# B-tree. The reference builds it exactly as the cores do: from the leaf SET in CANONICAL order —
# range_total_cmp, here the order-preserving range-bounds KEY bytes (encode_range_key, which is
# order-preserving for that total order by construction, encoding.md §2.11), ties by storage key —
# inserting each via least-enlargement choose-subtree + a deterministic median split at GIST_FANOUT.
# So the tree (and its bytes) are a pure function of the leaf set (content-deterministic), identical
# across every core. A node is { leaf: true, entries: [[bound, skey], ...] } or
# { leaf: false, children: [[union_bound, child_node], ...] }; `bound`/`union_bound` are range values
# (:empty or a {lower:,upper:,lower_inc:,upper_inc:} hash) and `skey` is a binary storage key.

# A GiST opclass (gist.md §2/§5/§6): { kind: :range, elem: <subtype> } for `range_ops`, or
# { kind: :scalar, type: <scalar> } for the scalar `=` opclass (GX2). The opclass is the ONLY
# type-specific part; the tree machinery below is opclass-agnostic, dispatching on `op[:kind]`.
# The per-column opclasses of a GiST index (gist.md §7): one per indexed column — `range_ops` for a
# range column, the scalar `=` opclass otherwise. A single-column GX1/GX2 index has one; an EXCLUDE
# backing index one per WITH column.
def gist_opclasses_for(table, ix)
  ix[:cols].map do |ci|
    ct = table[:columns][ci][:type]
    if (elem = range_elem(ct))
      { kind: :range, elem: elem }
    else
      { kind: :scalar, type: ct }
    end
  end
end

# One column's leaf component bound for value `v`: the range value itself (range opclass), or `[v, v]`
# over the order-preserving KEY encoding (scalar opclass — a leaf is the degenerate `[v, v]`, the
# bytes `key_body` produces, byte-comparable, gist.md §6).
def gist_comp_leaf(op, v)
  return v if op[:kind] == :range

  k = key_body(op[:type], v).b
  [k, k]
end

# A row's TUPLE leaf bound: one component per indexed column (callers skip a row with any NULL indexed
# column — gist.md §4.1/§7). For a single-column index this is a one-element tuple.
def gist_leaf_bound(ops, row, cols)
  ops.each_with_index.map { |op, k| gist_comp_leaf(op, row[cols[k]]) }
end

# range_union (range.rs, strict = false) — the convex hull. `empty` contributes nothing; otherwise
# lower = the lesser lower bound, upper = the greater upper bound. GX1's range elements are the six
# discrete/continuous subtypes; this reference's GiST golden uses only FINITE, canonical `[)` i32
# ranges (so the bound comparison reduces to value min/max with the canonical inclusivity), matching
# the cores for that set. An infinite bound is not exercised by the golden (a defensive raise).
def gist_range_union(a, b)
  return b if a == :empty
  return a if b == :empty
  if [a[:lower], a[:upper], b[:lower], b[:upper]].any?(&:nil?) ||
     !(a[:lower_inc] && b[:lower_inc]) || a[:upper_inc] || b[:upper_inc]
    raise "GiST golden expects finite canonical [) ranges"
  end
  { lower: [a[:lower], b[:lower]].min, upper: [a[:upper], b[:upper]].max,
    lower_inc: true, upper_inc: false }
end

# union of two COMPONENT bounds: the convex hull for a range; componentwise [min(min), max(max)]
# (byte-wise, the order-preserving key order) for a scalar.
def gist_comp_union(op, a, b)
  return gist_range_union(a, b) if op[:kind] == :range

  amin, amax = a
  bmin, bmax = b
  [[amin, bmin].min, [amax, bmax].max]
end

# union of two TUPLE bounds — componentwise (one per indexed column).
def gist_union(ops, a, b)
  ops.each_index.map { |k| gist_comp_union(ops[k], a[k], b[k]) }
end

# Serialize a TUPLE bound to its self-delimiting bytes — the per-column components concatenated, each
# the range body (ranges) or `[min, max]` length-prefixed (scalars). For a single-column index this is
# exactly the one component's bytes (gist.md §4.1/§6).
def gist_encode_bound(ops, bound)
  out = +"".b
  ops.each_index do |k|
    op = ops[k]
    if op[:kind] == :range
      out << encode_range_body(op[:elem], bound[k])
    else
      min, max = bound[k]
      out << u16(min.bytesize) << min << u16(max.bytesize) << max
    end
  end
  out
end

# The canonical total-order key for a TUPLE bound — the per-column sort keys concatenated, so the
# Array compares lexicographically over components (range_total_cmp = encode_range_key bytes for a
# range; the `[min, max]` key bytes for a scalar). `sort_by` appends the storage-key / subtree-min
# tiebreak uniformly.
def gist_sortkey(ops, bound)
  ops.each_index.flat_map do |k|
    ops[k][:kind] == :range ? [encode_range_key(ops[k][:elem], bound[k])] : bound[k]
  end
end

def gist_node_union(ops, node)
  bounds = node[:leaf] ? node[:entries].map { |b, _| b } : node[:children].map { |u, _| u }
  bounds.reduce { |acc, b| gist_union(ops, acc, b) }
end

def gist_subtree_min_skey(node)
  if node[:leaf]
    node[:entries].map { |_, s| s }.min
  else
    node[:children].map { |_, c| gist_subtree_min_skey(c) }.min
  end
end

# Canonical leaf order: (bound_total_cmp(bound), skey).
def gist_sort_leaf!(ops, entries)
  entries.sort_by! { |b, s| gist_sortkey(ops, b) + [s] }
end

# Canonical child order: (bound_total_cmp(union), subtree_min_skey).
def gist_sort_children!(ops, children)
  children.sort_by! { |u, c| gist_sortkey(ops, u) + [gist_subtree_min_skey(c)] }
end

# choose-subtree (penalty): the child whose union, MERGED with the new entry, has the
# lexicographically-smallest serialized bound bytes; ties keep the lower slot (gist.md §3).
def gist_choose_child(ops, children, bound)
  best = 0
  best_key = nil
  children.each_with_index do |(u, _), i|
    key = gist_encode_bound(ops, gist_union(ops, u, bound))
    if best_key.nil? || key < best_key
      best = i
      best_key = key
    end
  end
  best
end

# Insert (bound, skey) into `node` (mutated in place); returns nil or a new right-sibling child
# [union_bound, node] when the node split.
def gist_insert_node(ops, node, bound, skey)
  if node[:leaf]
    node[:entries] << [bound, skey]
    gist_sort_leaf!(ops, node[:entries])
  else
    i = gist_choose_child(ops, node[:children], bound)
    sib = gist_insert_node(ops, node[:children][i][1], bound, skey)
    node[:children][i][0] = gist_node_union(ops, node[:children][i][1])
    node[:children] << sib if sib
    gist_sort_children!(ops, node[:children])
  end
  gist_split_if_overflow(ops, node)
end

# Split an over-FANOUT node at the median (ceil(n/2)); return the new right sibling [union, node].
def gist_split_if_overflow(ops, node)
  key = node[:leaf] ? :entries : :children
  return nil if node[key].size <= GIST_FANOUT

  mid = (node[key].size + 1) / 2 # ceil(n/2) = div_ceil
  right_items = node[key].slice!(mid..)
  right = node[:leaf] ? { leaf: true, entries: right_items } : { leaf: false, children: right_items }
  [gist_node_union(ops, right), right]
end

# Build the canonical R-tree from leaf entries [[bound, skey], ...].
def gist_build(ops, entries)
  root = { leaf: true, entries: [] }
  entries.sort_by { |b, s| gist_sortkey(ops, b) + [s] }.each do |bound, skey|
    sib = gist_insert_node(ops, root, bound, skey)
    next unless sib

    children = [[gist_node_union(ops, root), root], sib]
    gist_sort_children!(ops, children)
    root = { leaf: false, children: children }
  end
  root
end

# Serialize the R-tree post-order into `pages` (children before parent, root last); returns
# [root_index, next_index]. Leaf entry: bound_len(u16) ‖ bound ‖ skey_len(u16) ‖ skey.
# Interior entry: bound_len(u16) ‖ union_bound ‖ child_page(u32). (gist.md §4.1)
def gist_serialize(ops, node, next_index, pages)
  if node[:leaf]
    payload = +"".b
    node[:entries].each do |bound, skey|
      b = gist_encode_bound(ops, bound)
      payload << u16(b.bytesize) << b << u16(skey.bytesize) << skey
    end
    index = next_index
    next_index += 1
    put_page(pages, index, PAGE_GIST_LEAF, node[:entries].size, payload, 0)
    [index, next_index]
  else
    child_indices = node[:children].map do |_, c|
      ci, next_index = gist_serialize(ops, c, next_index, pages)
      ci
    end
    payload = +"".b
    node[:children].each_with_index do |(u, _), i|
      b = gist_encode_bound(ops, u)
      payload << u16(b.bytesize) << b << ptr(child_indices[i])
    end
    index = next_index
    next_index += 1
    put_page(pages, index, PAGE_GIST_INTERIOR, node[:children].size, payload, 0)
    [index, next_index]
  end
end

# Build + serialize a GiST index's R-tree from a table's rows; returns [root_index, next_index].
# One leaf entry per row with NO NULL indexed column (any NULL → the row is not indexed, gist.md
# §4.1/§7). An empty index serializes nothing and returns root 0 (the empty-index convention).
def serialize_gist_index(table, ix, next_index, pages)
  return [0, next_index] if ix[:forged_empty]

  ops = gist_opclasses_for(table, ix)
  entries = []
  table_entries(table).each do |storage_key, row|
    next if ix[:cols].any? { |ci| row[ci].nil? } || !index_admits?(ix, row)

    entries << [gist_leaf_bound(ops, row, ix[:cols]), storage_key]
  end
  return [0, next_index] if entries.empty?

  gist_serialize(ops, gist_build(ops, entries), next_index, pages)
end

# A GiST index's leaf keys (bound ‖ skey), one per fully-non-NULL indexed row, sorted bytewise — the
# SET a round-trip read must reproduce (order-independent: the comparison sorts).
def gist_leaf_keys_sorted(table, ix)
  return [] if ix[:forged_empty]

  ops = gist_opclasses_for(table, ix)
  keys = []
  table_entries(table).each do |storage_key, row|
    next if ix[:cols].any? { |ci| row[ci].nil? } || !index_admits?(ix, row)

    keys << (gist_encode_bound(ops, gist_leaf_bound(ops, row, ix[:cols])) + storage_key).b
  end
  keys.sort
end

# Walk a persisted GiST R-tree (page types 5/6), returning its leaf keys (bound ‖ skey) — the
# inverse of serialize_gist_index for the round-trip read.
def read_gist_keys(image, ps, root_page)
  keys = []
  walk = lambda do |idx|
    return if idx.zero?

    pg = read_page(image, ps, idx)
    case pg[:type]
    when PAGE_GIST_LEAF
      pos = 0
      pg[:item_count].times do
        bl, pos = take(pg[:payload], pos, 2)
        bound, pos = take(pg[:payload], pos, bl.unpack1("n"))
        sl, pos = take(pg[:payload], pos, 2)
        skey, pos = take(pg[:payload], pos, sl.unpack1("n"))
        keys << (bound + skey).b
      end
    when PAGE_GIST_INTERIOR
      children = []
      pos = 0
      pg[:item_count].times do
        bl, pos = take(pg[:payload], pos, 2)
        _bound, pos = take(pg[:payload], pos, bl.unpack1("n"))
        cp, pos = read_ptr(pg[:payload], pos)
        children << cp
      end
      children.each { |cp| walk.call(cp) }
    else
      raise "expected a GiST node page, got type #{pg[:type]}"
    end
  end
  walk.call(root_page)
  keys
end

# --- out-of-line large values (large-values.md §12) -------------------------

def spillable?(type) = %w[text bytea decimal].include?(type)
# RECORD_MAX(K) = (C − max(12, 12+16K))/2 (format.md "Why the record cap"): the value is KEPT from
# v23 (bplus-reshape.md §4.2), re-derived leaf-only — the v24 worst-case two-record leaf overhead
# (all-variable, 12+13K) stays under the reserve. K=0 (index trees) is exact: 2·(C−12)/2 + 12 = C.
def record_max(cap, k) = [(cap - [12, 12 + 16 * k].max) / 2, 0].max

# The dense-slot width of a FIXED-WIDTH column's value body (format.md v24 "Leaf node"), or nil for
# a VARIABLE-WIDTH column (text/bytea/decimal/json/jsonb/composite/array/range — the spillable set).
# The class decides the column's leaf region shape: fixed regions are flags + null bitmap + dense
# untagged slots; variable regions are flags + an end-offset value directory + tagged codec bytes
# (NULL = a zero-length span).
STORAGE_WIDTH = { "i16" => 2, "i32" => 4, "i64" => 8, "boolean" => 1, "uuid" => 16,
                  "timestamp" => 8, "timestamptz" => 8, "date" => 4, "interval" => 16,
                  "f64" => 8, "f32" => 4 }.freeze
def fixed_width(type) = type.is_a?(String) ? STORAGE_WIDTH[type] : nil

# leaf_overhead(N, cols): a v24 leaf's payload beyond Σ record_size (format.md "Leaf node") — the
# key directory (4N), the column directory (4(K+1)), and per region a flags byte plus the null
# bitmap (fixed-width, ceil(N/8)) or the value directory (variable, 4N).
def leaf_overhead(n, cols)
  cols.sum(4 * n + 4 * (cols.size + 1)) do |c|
    1 + (fixed_width(c[:type]) ? (n + 7) / 8 : 4 * n)
  end
end

# The value columns of a leaf's records ([] for an index tree) — the leaf overhead needs their
# classes. Derived from the record plan's table, so no extra threading.
def leaf_cols(node) = node[:recs].empty? ? [] : node[:recs][0][:table][:columns]

# A value's content payload P(v) — the bytes stored in the overflow chain when externalized: raw
# UTF-8 / raw bytes for text/bytea, the decimal body for decimal (large-values.md §12).
def value_payload(type, v)
  case type
  when "text", "bytea" then v.b
  when "decimal" then encode_decimal(v) # the body (no presence tag)
  else raise "only spillable values are externalized"
  end
end

# Decide each column's disposition for a record: [forms, comps, on-disk record size], where
# forms[i] is :inline / :inline_comp / :external / :external_comp and comps[i] holds the LZ4
# block for a compressed form. Spill only when forced (record > RECORD_MAX); then COMPRESS the
# largest eligible values first (store-smaller rule), then EXTERNALIZE the largest remaining —
# both passes ties-by-column-index. Deterministic, mirrors the cores' plan_dispositions
# (large-values.md §12/§13; format.md "Large values").
def plan_record(table, key, row, cap)
  cols = table[:columns]
  # Each column's inline-plain contribution to record_size (the v24 basis — format.md "Record"):
  # a fixed-width column always its width (NULL occupies a zero-filled slot); a variable column 0
  # when NULL (a zero-length span) else its tagged inline encoding.
  inline = cols.each_with_index.map do |c, i|
    if (w = fixed_width(c[:type]))
      w
    else
      row[i].nil? ? 0 : encode_value(c[:type], row[i]).bytesize
    end
  end
  forms = Array.new(cols.size, :inline)
  comps = Array.new(cols.size)
  cur = inline.dup
  size = key.bytesize + inline.sum
  max = record_max(cap, cols.size)
  return [forms, comps, size] if size <= max

  # Pass 1 — compress (lz4.md): payload >= S_COMPRESS, largest inline-plain encoded size first,
  # ties by ascending index; adopt iff the encoded compressed form is strictly smaller.
  cand = (0...cols.size).select do |i|
    spillable?(cols[i][:type]) && !row[i].nil? && value_payload(cols[i][:type], row[i]).bytesize >= S_COMPRESS
  end
  cand.sort_by! { |i| [-inline[i], i] }
  cand.each do |i|
    break if size <= max

    comp = LZ4.compress(value_payload(cols[i][:type], row[i]))
    next unless INLINE_COMP_OVERHEAD + comp.bytesize < inline[i]

    forms[i] = :inline_comp
    comps[i] = comp
    size += INLINE_COMP_OVERHEAD + comp.bytesize - cur[i]
    cur[i] = INLINE_COMP_OVERHEAD + comp.bytesize
  end
  return [forms, comps, size] if size <= max

  # Pass 2 — externalize: anything whose current encoded size beats its pointer, largest first.
  cand = (0...cols.size).select do |i|
    spillable?(cols[i][:type]) &&
      cur[i] > (forms[i] == :inline_comp ? EXTERNAL_COMP_PTR_LEN : EXTERNAL_PTR_LEN) + tag_len
  end
  cand.sort_by! { |i| [-cur[i], i] }
  cand.each do |i|
    break if size <= max

    ptr = (forms[i] == :inline_comp ? EXTERNAL_COMP_PTR_LEN : EXTERNAL_PTR_LEN) + tag_len
    forms[i] = forms[i] == :inline_comp ? :external_comp : :external
    size += ptr - cur[i]
    cur[i] = ptr
  end
  [forms, comps, size]
end

# Write a payload across a chain of overflow pages (cap-byte slabs, in order), allocating each via
# `alloc` and linking with next_page (0 terminates). Returns the first page index for the pointer.
def write_overflow_chain(payload, cap, alloc, pages)
  slab_cap = cap - tag_len # an encrypted chain page leads with its successor's tag (encrypted.md §2)
  n = (payload.bytesize + slab_cap - 1) / slab_cap
  indices = Array.new(n) { alloc.call }
  # Sealed last page first, so each page binds its successor (encrypted.md §7).
  (n - 1).downto(0) do |j|
    slab = payload.byteslice(j * slab_cap, slab_cap)
    nxt = j + 1 < n ? indices[j + 1] : 0
    put_page(pages, indices[j], PAGE_OVERFLOW, slab.bytesize, slab, nxt)
  end
  indices[0]
end

# Emit one record's on-disk bytes (key_len | key | values), spilling external columns to overflow
# pages drawn via `alloc` (large-values.md §12/§13). `rec` is a plan hash
# { key:, row:, table:, forms:, comps: } from plan_record.
# One value's on-disk body given its disposition (the value codec, unchanged across row-major and PAX):
# inline-plain is encode_value; the large-value forms carry a pointer / inline block and allocate
# overflow chains via `alloc` (large-values.md §12/§13). `comp` is the LZ4 block for a compressed form.
def emit_value(c, v, form, comp, cap, alloc, pages)
  case form
  when :external
    payload = value_payload(c[:type], v)
    first = write_overflow_chain(payload, cap, alloc, pages)
    [TAG_EXTERNAL].pack("C") + u32(first) + u32(payload.bytesize) + tag_of(first)
  when :inline_comp
    payload = value_payload(c[:type], v)
    [TAG_INLINE_COMP].pack("C") + u32(payload.bytesize) + u16(comp.bytesize) + comp
  when :external_comp
    payload = value_payload(c[:type], v)
    first = write_overflow_chain(comp, cap, alloc, pages) # the chain carries the COMPRESSED block
    [TAG_EXTERNAL_COMP].pack("C") + u32(first) + u32(comp.bytesize) + u32(payload.bytesize) + tag_of(first)
  else
    encode_value(c[:type], v)
  end
end

# Emit a v24 INTERIOR node payload (format.md "Interior node"): N+1 child pointers, an N-entry
# end-offset separator directory, then the separator key blob. Record-free.
def emit_interior(sep_keys, child_pages)
  out = +"".b
  child_pages.each { |cp| out << ptr(cp) }
  off = 0
  sep_keys.each { |k| off += k.bytesize; out << u32(off) }
  sep_keys.each { |k| out << k }
  out
end

# Emit a v24 leaf's payload COLUMN-MAJOR (format.md "Leaf node"): values are encoded in
# (record, column) order (so overflow chains allocate deterministically), then assembled as
# key dir (N end offsets) | key blob | col dir (K+1) | per region: flags byte, then null bitmap +
# dense untagged slots (fixed-width) or an N-entry end-offset value directory + tagged bodies
# (variable; NULL = a zero-length span). `recs` are record plans.
def emit_leaf_pax(recs, cap, alloc, pages)
  n = recs.size
  cols = recs.empty? ? [] : recs[0][:table][:columns]
  k = cols.size
  keys = recs.map { |r| r[:key] }
  val_bytes = Array.new(k) { Array.new(n) }
  recs.each_with_index do |rec, i|
    cols.each_with_index do |c, ci|
      v = rec[:row][ci]
      val_bytes[ci][i] =
        if (w = fixed_width(c[:type]))
          v.nil? ? ("\x00".b * w) : encode_value(c[:type], v).byteslice(1..) # untagged body; NULL slot zero-filled
        elsif v.nil?
          "".b # a zero-length span
        else
          emit_value(c, v, rec[:forms][ci], rec[:comps][ci], cap, alloc, pages)
        end
    end
  end
  out = +"".b
  off = 0
  keys.each { |kb| off += kb.bytesize; out << u32(off) }               # key directory (end offsets)
  keys.each { |kb| out << kb }                                          # key blob
  base_after_col_dir = out.bytesize + 4 * (k + 1)
  col_start = []
  cur = base_after_col_dir
  (0...k).each do |c|
    col_start[c] = cur
    cur += 1 + (fixed_width(cols[c][:type]) ? (n + 7) / 8 : 4 * n) + val_bytes[c].sum(&:bytesize)
  end
  col_start[k] = cur
  (0..k).each { |c| out << u32(col_start[c]) }                          # column directory
  (0...k).each do |c|
    out << "\x00".b                                                    # region flags (reserved)
    if fixed_width(cols[c][:type])
      bitmap = Array.new((n + 7) / 8, 0)
      recs.each_with_index { |rec, i| bitmap[i / 8] |= (0x80 >> (i % 8)) if rec[:row][c].nil? }
      out << bitmap.pack("C*")
    else
      voff = 0
      val_bytes[c].each { |vb| voff += vb.bytesize; out << u32(voff) } # value directory (end offsets)
    end
    val_bytes[c].each { |vb| out << vb }                               # value bodies / slots
  end
  out
end

# Lay catalog entries out on pages of capacity `cap` (format.md *Catalog*, v48): packed greedily,
# each wholly on one page, except that an entry longer than `cap` starts a fresh page and spans as
# many pages as it needs — the first with item_count 1, the rest 0, every page of the run but the
# last flagged `continued` (header byte 1 bit0); the entry after it starts a fresh page.
def catalog_layout(entries, cap)
  out = []
  cur = { count: 0, continued: false, payload: +"".b }
  entries.each do |e|
    if e.bytesize > cap
      if cur[:count].positive?
        out << cur
        cur = { count: 0, continued: false, payload: +"".b }
      end
      chunks = (0...e.bytesize).step(cap).map { |o| e.byteslice(o, cap) }
      chunks.each_with_index do |c, i|
        out << { count: i.zero? ? 1 : 0, continued: i + 1 < chunks.size, payload: c.b }
      end
      next
    end
    if cur[:count].positive? && cur[:payload].bytesize + e.bytesize > cap
      out << cur
      cur = { count: 0, continued: false, payload: +"".b }
    end
    cur[:count] += 1
    cur[:payload] << e
  end
  out << cur if cur[:count].positive? || out.empty?
  out
end

def pack(sizes, cap)
  groups = []
  cur = []
  used = 0
  sizes.each_with_index do |sz, i|
    raise "item of size #{sz} exceeds page capacity #{cap}" if sz > cap

    if !cur.empty? && used + sz > cap
      groups << cur
      cur = []
      used = 0
    end
    cur << i
    used += sz
  end
  groups << cur
  groups
end

# `live` defaults to every body page: a from-scratch image has no free list and no manifest.
def write_meta(image, ps, slot, page_size, txid, root, page_count, live = page_count - first_body)
  off = slot * ps
  image[off, ps] = "\x00".b * ps
  image[off, 4] = MAGIC
  image[off + 4, 2] = u16(VERSION)
  image[off + 8, 4] = u32(page_size)
  image[off + 12, 8] = u64(txid)
  image[off + 20, 4] = u32(root)
  image[off + 24, 4] = u32(page_count)
  image[off + 56, 4] = u32(live) # v34: the exact live page count
  if $enc
    # The encrypted meta (encrypted.md §6): suite and reserve, the catalog root's tag, then sealed.
    image.setbyte(off + 6, 1)
    image.setbyte(off + 7, RESERVE)
    image[off + 64, 16] = tag_of(root)
    image[off, ps] = seal_meta(image.byteslice(off, ps), slot)
    return
  end
  image[off + 32, 4] = u32(meta_crc(image.byteslice(off, ps)))
end

def write_page(image, ps, index, type, item_count, next_page, payload, flags: 0)
  off = index * ps
  image[off, 1] = [type].pack("C")
  image[off + 1, 1] = [flags].pack("C")
  image[off + 4, 4] = u32(item_count)
  image[off + 8, 4] = u32(next_page)
  image[off + PAGE_HEADER, payload.bytesize] = payload unless payload.empty?
  # The per-page checksum (v7) is computed last, over every byte but its own field at [12,16).
  image[off + 12, 4] = u32(page_crc(image.byteslice(off, ps)))
end

# --- size-driven B-tree (format.md "The per-table data B-tree") -------------
#
# Built the way the cores do: insert each (key, record) in ascending key order, splitting any
# node whose serialized payload exceeds the page capacity C. A node is a Hash
#   { keys: [binary-key, ...], recs: [record-bytes, ...], children: [node, ...] }
# A leaf has children == []; an interior has children.size == keys.size + 1. recs[i] is the full
# record bytes (key_len + key + values) for keys[i] — this B-tree stores a value with every key,
# separators included. Split point and the RECORD_MAX = C/2 cap mirror format.md exactly.

def node_leaf?(node) = node[:children].empty?

# A LEAF's `recs[i]` is the record PLAN for keys[i] — a hash { key:, row:, table:, forms:, comps:,
# size: } whose `size` is the on-disk (post-spill) record size (large-values.md §12). The tree
# splits on `size`; the actual pointer bytes (with allocated overflow pages) are emitted later by
# serialize_tree. An INTERIOR node has no recs (v24) — its payload is the separators themselves.
def node_payload(node)
  if node_leaf?(node)
    node[:recs].sum { |r| r[:size] } + leaf_overhead(node[:keys].size, leaf_cols(node))
  else
    (4 + tag_len) * (node[:keys].size + 1) + 4 * node[:keys].size + node[:keys].sum(&:bytesize)
  end
end

# The kind-shared split-point rule (format.md "Split point"): over the kind's range, m_max = the
# largest m whose left side fits, m_min = the smallest whose right side fits, m_balanced = the
# smallest m with 2·leftpayload(m) >= payload; a right-edge edit takes m_max, anything else
# clamp(min(m_balanced, m_max), m_min, m_max). nil when no m keeps both sides fitting.
def pick_split_m(range, payload, cap, right_edge, leftp, rightp)
  m_max = nil
  range.each { |m| leftp.call(m) <= cap ? m_max = m : break }
  m_min = nil
  range.reverse_each { |m| rightp.call(m) <= cap ? m_min = m : break }
  return nil if m_max.nil? || m_min.nil? || m_min > m_max
  return m_max if right_edge

  m_bal = range.find { |m| 2 * leftp.call(m) >= payload } || m_max
  m_bal.clamp(m_min, m_max)
end

# Split an overflowing node 2-way (format.md "Fan-out", v24): a LEAF splits COPY-UP — the left
# leaf keeps records [0, m), the right leaf [m, N), and the separator handed up is a COPY of
# keys[m] (no record leaves the leaf level) — [:split, left, sep_key, right]. An INTERIOR node
# splits PUSH-UP — the median separator moves up, left keeps [0, m) + children [0, m], right
# keeps [m+1, N) + children [m+1, N]; with N == 2 the split is pinned to m = 1, producing the
# legal N = 0 right interior (the degenerate max-separator contract). This builder only inserts
# in ascending key order (build_tree), so right_edge is always true here — the balanced arm is
# implemented so the reference states the whole contract.
def split_node(node, cap, right_edge)
  payload = node_payload(node)
  return [:whole, node] if payload <= cap

  n = node[:keys].size
  if node_leaf?(node)
    return [:whole, node] if n < 2 # a single over-cap record: unsupported, surfaces upstream

    cols = leaf_cols(node)
    prefix = [0]
    node[:recs].each { |r| prefix << prefix.last + r[:size] }
    leftp = ->(m) { prefix[m] + leaf_overhead(m, cols) }
    rightp = ->(m) { (prefix[n] - prefix[m]) + leaf_overhead(n - m, cols) }
    m = pick_split_m(1..(n - 1), payload, cap, right_edge, leftp, rightp)
    raise "unsplittable leaf (record over RECORD_MAX?)" if m.nil?

    left = { keys: node[:keys][0, m], recs: node[:recs][0, m], children: [] }
    right = { keys: node[:keys][m..], recs: node[:recs][m..], children: [] }
    [:split, left, right[:keys][0].dup, right]
  else
    m = if n == 2
          1 # the degenerate pin: sep[1] moves up, the right side is a legal N = 0 interior
        else
          prefix = [0]
          node[:keys].each { |k| prefix << prefix.last + k.bytesize }
          pw = 4 + tag_len # a child pointer's width (encrypted.md §2)
          leftp = ->(mm) { (pw + 4) * mm + pw + prefix[mm] }
          rightp = ->(mm) { (pw + 4) * (n - 1 - mm) + pw + (prefix[n] - prefix[mm + 1]) }
          mm = pick_split_m(1..(n - 2), payload, cap, right_edge, leftp, rightp)
          raise "unsplittable interior on the insert path" if mm.nil?

          mm
        end
    left = { keys: node[:keys][0, m], recs: [], children: node[:children][0, m + 1] }
    right = { keys: node[:keys][(m + 1)..], recs: [], children: node[:children][(m + 1)..] }
    [:split, left, node[:keys][m], right]
  end
end

# Insert (key, rec) into the subtree, rebuilding (copy-on-write) and splitting up the path. An
# interior descent routes by partition_point(sep <= key) — a key equal to a separator lies RIGHT
# (format.md "Interior node"); leaves hold the records.
def tree_insert(node, key, rec, cap)
  if node_leaf?(node)
    i = node[:keys].bsearch_index { |k| k >= key } || node[:keys].size
    raise "duplicate key in fixture" if i < node[:keys].size && node[:keys][i] == key

    return split_node({ keys: node[:keys].dup.insert(i, key),
                        recs: node[:recs].dup.insert(i, rec), children: [] }, cap,
                      i == node[:keys].size)
  end
  i = node[:keys].bsearch_index { |k| k > key } || node[:keys].size
  res = tree_insert(node[:children][i], key, rec, cap)
  if res[0] == :split
    _, left, sk, right = res
    children = node[:children].dup
    children[i] = left
    children.insert(i + 1, right)
    split_node({ keys: node[:keys].dup.insert(i, sk), recs: [], children: children }, cap,
               i == node[:keys].size)
  else
    children = node[:children].dup
    children[i] = res[1]
    [:whole, { keys: node[:keys], recs: [], children: children }]
  end
end

# Build a table's B-tree from its (key, record) pairs in key order. nil for an empty table.
def build_tree(pairs, cap)
  root = nil
  k = pairs.empty? ? 0 : pairs[0][1][:table][:columns].size
  max = record_max(cap, k)
  pairs.each do |key, rec|
    # rec is a record plan; rec[:size] is the post-spill on-disk size. A record still over
    # RECORD_MAX after externalizing every spillable value is genuinely unsupported (0A000).
    raise "record of #{rec[:size]}B exceeds RECORD_MAX #{max} after spilling" if rec[:size] > max

    if root.nil?
      root = { keys: [key], recs: [rec], children: [] }
      next
    end
    res = tree_insert(root, key, rec, cap)
    root = res[0] == :split ? { keys: [res[2]], recs: [], children: [res[1], res[3]] } : res[1]
  end
  root
end

# Post-order page allocation: children before their parent (so a parent's child pointers
# reference already-allocated pages). Fills `pages[index] = [page_type, item_count, payload]`.
# Returns [root_page (0 for an empty tree), next free index].
def serialize_tree(root, next_index, cap, pages)
  return [0, next_index] if root.nil?

  child_pages = root[:children].map do |c|
    cp, next_index = serialize_tree(c, next_index, cap, pages)
    cp
  end
  index = next_index
  next_index += 1
  # Emit records, spilling external values to overflow pages allocated AFTER this node's index
  # (post-order traversal + column order → deterministic, golden-pinnable layout — large-values.md §12).
  alloc = lambda do
    i = next_index
    next_index += 1
    i
  end
  n = root[:keys].size
  if node_leaf?(root)
    put_page(pages, index, PAGE_LEAF, n, emit_leaf_pax(root[:recs], cap, alloc, pages), 0) # column-major (v24)
  else
    put_page(pages, index, PAGE_INTERIOR, n, emit_interior(root[:keys], child_pages), 0) # record-free (v24)
  end
  [index, next_index]
end

# A from-scratch image (format.md "Allocation & incremental commit"): the special case where
# every node is dirty — data B-trees post-order (per table, name order) from page 2, then the
# catalog chain, then both meta slots at txid 1.
def build_image(types, sequences, tables, page_size, collations = [], start_page: ROOT_PAGE, views: [],
                encrypted: false, txid: TXID)
  return build_encrypted_image(types, sequences, tables, page_size, collations, views, txid) if encrypted

  ps = page_size
  cap = ps - PAGE_HEADER - ($enc ? RESERVE : 0)
  # Composite types in scope for the recursive value codec, keyed by lowercased name (§4).
  $ctypes = types.to_h { |t| [t[:name], t[:fields]] }
  sorted = tables.sort_by { |t| t[:name] }
  sorted_types = types.sort_by { |t| t[:name] }
  sorted_seqs = sequences.sort_by { |s| s[:name] }
  sorted_colls = collations.sort_by { |c| c[:name] }

  data_pages = {} # index => [page_type, item_count, payload]
  root_data = Array.new(sorted.size, 0)
  index_roots = Array.new(sorted.size) { [] }
  next_index = start_page
  sorted.each_with_index do |t, ti|
    pairs = table_entries(t).map do |key, row|
      forms, comps, size = plan_record(t, key, row, cap)
      [key, { key: key, row: row, table: t, forms: forms, comps: comps, size: size }]
    end
    root_data[ti], next_index = serialize_tree(build_tree(pairs, cap), next_index, cap, data_pages)
    # The table's index trees follow its data tree, in catalog (name) order (format.md
    # "Allocation & incremental commit" / "From-scratch image").
    (t[:indexes] || []).each do |ix|
      r, next_index =
        if ix[:kind] == "gist"
          # The on-disk form is the R-tree (pages 5/6), not the flat leaf B-tree (gist.md §4.1).
          serialize_gist_index(t, ix, next_index, data_pages)
        else
          entries = ix[:kind] == "gin" ? gin_index_entries(t, ix) : index_entries(t, ix)
          serialize_tree(build_tree(entries, cap), next_index, cap, data_pages)
        end
      index_roots[ti] << r
    end
  end

  # Catalog entries are kind-tagged: composite-type entries (kind 1, name order) first, then
  # sequence entries (kind 2, name order, v12), then collation snapshots (kind 3, name order, v17),
  # then table entries (kind 0) — format.md.
  cat_root = next_index
  cat_entries = []
  sorted_types.each { |ct| cat_entries << ("\x01".b + composite_type_entry_bytes(ct)) }
  sorted_seqs.each { |s| cat_entries << ("\x02".b + sequence_entry_bytes(s)) }
  sorted_colls.each { |c| cat_entries << ("\x03".b + collation_entry_bytes(c)) }
  sorted.each_with_index do |t, ti|
    cat_entries << ("\x00".b + table_entry_bytes(t, root_data[ti], index_roots[ti], t[:row_count] || t[:rows].length))
  end
  sorted.each { |t| cat_entries.concat(statistics_entries(t)) }
  # View entries (kind 5, v38) come last, in view-name byte order (format.md *View entry*).
  views.sort_by { |v| v[:name] }.each { |v| cat_entries << ("\x05".b + view_entry_bytes(v)) }
  cat_layout = catalog_layout(cat_entries, cap - tag_len)
  page_count = cat_root + cat_layout.size

  image = "\x00".b * (page_count * ps)
  unless $enc
    write_meta(image, ps, 0, page_size, TXID, cat_root, page_count)
    write_meta(image, ps, 1, page_size, TXID, cat_root, page_count)
  end

  data_pages.each do |index, page|
    if page[0] == :sealed
      image[index * ps, ps] = page[1]
    else
      type, count, payload, nxt = page
      write_page(image, ps, index, type, count, nxt || 0, payload)
    end
  end

  if $enc
    # Sealed last page first, so each binds its successor (encrypted.md §7); then the metas.
    (cat_layout.size - 1).downto(0) do |gi|
      cp = cat_layout[gi]
      index = cat_root + gi
      nxt = gi + 1 < cat_layout.size ? index + 1 : 0
      image[index * ps, ps] = seal_body(index, PAGE_CATALOG, cp[:count], nxt, cp[:payload],
                                        flags: cp[:continued] ? 1 : 0)
    end
    write_meta(image, ps, 0, page_size, $enc[:txid], cat_root, page_count)
    write_meta(image, ps, 1, page_size, $enc[:txid], cat_root, page_count)
  else
    cat_layout.each_with_index do |cp, gi|
      index = cat_root + gi
      nxt = gi + 1 < cat_layout.size ? index + 1 : 0
      write_page(image, ps, index, PAGE_CATALOG, cp[:count], nxt, cp[:payload], flags: cp[:continued] ? 1 : 0)
    end
  end

  image
end

# The encrypted variant's from-scratch image (encrypted.md): the key block (its DEK, KDF salt and
# wrap nonce the stream's first draws) at pages 2 and 3, then the image sealed from page 4 in seal
# order — the stream continuing — under the goldens' host key.
def build_encrypted_image(types, sequences, tables, page_size, collations, views, txid = TXID)
  rng = SeededStream.new(ENC_SEED)
  block, dek = new_key_block(rng)
  $enc = { ps: page_size, rng: rng, tags: {}, page_key: Xaes.page_key(dek), meta_key: Xaes.meta_key(dek),
           txid: txid }
  image = build_image(types, sequences, tables, page_size, collations, start_page: ENC_FIRST_BODY, views: views)
  key_page = block + "\x00".b * (page_size - block.bytesize)
  image[2 * page_size, page_size] = key_page
  image[3 * page_size, page_size] = key_page
  image
ensure
  $enc = nil
end

# The bytes a fixture should contain (applying any torn-slot corruption).
def fixture_image(fx)
  return cow_fixture_image(fx) if fx[:cow]
  return malformed_fixture_image(fx) if fx[:malformed]

  image = build_image(fx[:types] || [], fx[:sequences] || [], fx[:tables], fx[:page_size],
                      fx[:collations] || [], views: fx[:views] || [], encrypted: fx[:encrypted],
                      txid: fx[:txid] || TXID)
  if fx[:corrupt_slot]
    off = fx[:corrupt_slot] * fx[:page_size] + 35 # last CRC byte of that slot
    image.setbyte(off, image.getbyte(off) ^ 0xFF)
  end
  image
end

def refresh_meta_crc(image, ps, slot)
  off = slot * ps
  image[off + 32, 4] = u32(meta_crc(image.byteslice(off, ps)))
end

def refresh_body_crc(image, ps, index)
  off = index * ps
  image[off + 12, 4] = u32(page_crc(image.byteslice(off, ps)))
end

def refresh_manifest_chain_digest(image, ps, chain)
  digest = chain.reduce(0) { |crc, index| crc64(u32(index) + image.byteslice(index * ps, ps), crc) }
  image[48, 8] = u64(digest)
  refresh_meta_crc(image, ps, 0)
end

def cow_fixture_image(fx)
  ps = fx[:page_size]
  old = build_image([], [], [COW_OLD_TABLE], ps)
  old_count = old.bytesize / ps
  fresh = build_image([], [], [COW_NEW_TABLE], ps, [], start_page: old_count)
  root = fresh.byteslice(20, 4).unpack1("N")
  image = old + fresh.byteslice(old.bytesize, fresh.bytesize - old.bytesize)
  dirty = (old_count...(image.bytesize / ps)).to_a
  fx[:cow_orphans].times do |i|
    index = image.bytesize / ps
    image << "\x00".b * ps
    write_page(image, ps, index, PAGE_OVERFLOW, 1, 0, [i + 1].pack("C"))
    dirty << index
  end
  entries = dirty.map { |index| u32(index) + u64(crc64(image.byteslice(index * ps, ps))) }
  inline_count = [entries.length, (ps - 64) / 12].min
  overflow_count = (entries.length - inline_count + (ps - 32) / 12 - 1) / ((ps - 32) / 12)
  overflow_count += 1 if fx[:cow] == :empty_tail
  chain = Array.new(overflow_count) { |i| image.bytesize / ps + i }
  image << "\x00".b * (overflow_count * ps)
  remaining = entries.drop(inline_count)
  chain.each_with_index do |index, ordinal|
    chunk = remaining.shift((ps - 32) / 12)
    payload = u64(2) + u32(ordinal) + u32(0) + chunk.join.b
    write_page(image, ps, index, 8, chunk.length, chain[ordinal + 1] || 0, payload)
  end
  # The new state's live pages are exactly its freshly appended leaf + catalog pages; the orphan
  # writes and the manifest chain are not reachable.
  write_meta(image, ps, 0, ps, 2, root, image.bytesize / ps, fresh.bytesize / ps - old_count)
  image[36, 12] = u32(chain.first || 0) + u32(entries.length) + u32(chain.length)
  image[64, inline_count * 12] = entries.first(inline_count).join.b
  refresh_manifest_chain_digest(image, ps, chain)

  # Reseal outer checksums for malformed-but-checksummed structural cases. This
  # prevents a test from passing merely because it rejected an unrelated CRC.
  case fx[:cow]
  when :torn_body
    image.setbyte(old_count * ps + 31, image.getbyte(old_count * ps + 31) ^ 0x40)
  when :stale_body
    image[old_count * ps, ps] = old.byteslice(2 * ps, ps)
  when :stale_orphan
    index = dirty.last
    image.setbyte(index * ps + 16, 0xFE)
    refresh_body_crc(image, ps, index)
  when :torn_meta
    image.setbyte(63, image.getbyte(63) ^ 0x80)
  when :torn_manifest
    index = chain.last
    image.setbyte(index * ps + 32, image.getbyte(index * ps + 32) ^ 0x80)
  when :wrong_manifest_txid
    image[chain.first * ps + 16, 8] = u64(1)
    refresh_body_crc(image, ps, chain.first)
    refresh_manifest_chain_digest(image, ps, chain)
  when :manifest_cycle
    image[chain.last * ps + 8, 4] = u32(chain.first)
    refresh_body_crc(image, ps, chain.last)
    refresh_manifest_chain_digest(image, ps, chain)
  when :duplicate_entry
    image[76, 12] = image.byteslice(64, 12)
    refresh_meta_crc(image, ps, 0)
  when :out_of_bounds
    image[76, 4] = u32(image.bytesize / ps)
    refresh_meta_crc(image, ps, 0)
  when :missing_root
    image[40, 4] = u32(1)
    image[76, 12] = "\x00".b * 12
    refresh_meta_crc(image, ps, 0)
  when :root_manifest
    image[20, 4] = u32(chain.first)
    refresh_meta_crc(image, ps, 0)
  when :free_head_manifest
    image[28, 4] = u32(chain.first)
    refresh_meta_crc(image, ps, 0)
  when :invalid_free_list
    index = image.bytesize / ps
    image << "\x00".b * ps
    write_page(image, ps, index, 7, 1, 0, u32(old_count))
    image[24, 4] = u32(index + 1)
    image[28, 4] = u32(index)
    image[40, 4] = u32(entries.length + 1)
    image[64 + entries.length * 12, 12] = u32(index) + u64(crc64(image.byteslice(index * ps, ps)))
    refresh_meta_crc(image, ps, 0)
  when :bad_padding
    image.setbyte(ps - 1, 1)
    refresh_meta_crc(image, ps, 0)
  end
  image
end

# Each table's root data page, read back from a clean image's catalog.
def table_root_pages(image, ps)
  roots = {}
  cat = select_meta(image, ps)[:root_page]
  while cat != 0
    pg = read_page(image, ps, cat)
    pos = 0
    pg[:item_count].times do
      kind = pg[:payload].getbyte(pos)
      pos += 1
      if kind == 1
        _ct, pos = decode_composite_type_entry(pg[:payload], pos)
        next
      end
      raise "malformed fixtures carry only types and tables" unless kind.zero?

      entry, pos = decode_table_entry(pg[:payload], pos)
      roots[entry[:name]] = entry[:root_data_page]
    end
    cat = pg[:next_page]
  end
  roots
end

def page_u32(image, ps, index, offset) = image.byteslice(index * ps + offset, 4).unpack1("N")

def set_page_u32(image, ps, index, offset, value)
  image[index * ps + offset, 4] = u32(value)
  refresh_body_crc(image, ps, index)
end

def malformed_fixture_image(fx)
  ps = fx[:page_size]
  image = build_image(fx[:types] || [], [], fx[:tables], ps)
  roots = table_root_pages(image, ps)
  case fx[:malformed]
  when :leaf_directory
    # t's second leaf: its first key-directory end offset now runs past the page.
    leaf = page_u32(image, ps, roots["t"], PAGE_HEADER + 4)
    set_page_u32(image, ps, leaf, PAGE_HEADER, 0xFFFF_FFF0)
  when :wrong_tree_leaf
    # t's second child pointer now names u's root leaf (two text columns and a boolean, not i16).
    set_page_u32(image, ps, roots["t"], PAGE_HEADER + 4, roots["u"])
  when :interior_cycle
    # t's root interior names itself as its first child.
    set_page_u32(image, ps, roots["t"], PAGE_HEADER, roots["t"])
  when :decimal_group
    # The first base-10^4 group of row 2's d (-12345.6789) is now 65535.
    leaf = roots["t"]
    pattern = encode_value("decimal", "-12345.6789")
    payload = image.byteslice(leaf * ps, ps)
    at = payload.index(pattern)
    raise "decimal pattern not unique" unless at && payload.index(pattern, at + 1).nil?

    image[leaf * ps + at + 6, 2] = u16(0xFFFF)
    refresh_body_crc(image, ps, leaf)
  when :overflow_cycle
    # The first overflow page of a multi-page chain now links back to itself.
    first = (2...image.bytesize / ps).find do |i|
      image.getbyte(i * ps) == PAGE_OVERFLOW && page_u32(image, ps, i, 8) != 0
    end
    set_page_u32(image, ps, first, 8, first)
  when :drop_types
    # The catalog keeps the table but loses the composite-type entries ahead of it.
    cat = select_meta(image, ps)[:root_page]
    dropped = fx[:types].sum { |t| 1 + composite_type_entry_bytes(t).bytesize }
    payload = image.byteslice(cat * ps + PAGE_HEADER + dropped, ps - PAGE_HEADER - dropped)
    image[cat * ps + PAGE_HEADER, ps - PAGE_HEADER] = payload + "\x00".b * dropped
    image[cat * ps + 4, 4] = u32(page_u32(image, ps, cat, 4) - fx[:types].size)
    refresh_body_crc(image, ps, cat)
  when :none
    nil
  else
    raise "unknown malformed mutation #{fx[:malformed]}"
  end
  image
end

# --- decoding (independent reader) ------------------------------------------

# The v31 host-function dependency list (extensibility.md §8.1), shared by an index (v31) and a CHECK
# (v35, §7.1.3): u16 dep_count, then per dependency name ‖ u16 arg_count ‖ arg type codes ‖ result
# type code ‖ component_id ‖ u32 semantic_version, in ascending (name, arg-type codes) order.
def write_host_deps(out, deps)
  out << u16(deps.size)
  deps.each do |dep|
    out << u16(dep[:name].bytesize) << dep[:name].b
    out << u16(dep[:arg_types].size)
    dep[:arg_types].each { |tc| out << [tc].pack("C") }
    out << [dep[:result]].pack("C")
    out << u16(dep[:component_id].bytesize) << dep[:component_id].b
    out << u32(dep[:semantic_version])
  end
end

def read_host_deps(buf, pos)
  deps = []
  dc, pos = take(buf, pos, 2)
  raise "a host-function dependency flag with an empty dependency list" if dc.unpack1("n").zero?
  dc.unpack1("n").times do
    dnl, pos = take(buf, pos, 2)
    dname, pos = take(buf, pos, dnl.unpack1("n"))
    ac, pos = take(buf, pos, 2)
    arg_types = []
    ac.unpack1("n").times do
      tcb, pos = take(buf, pos, 1)
      arg_types << tcb.getbyte(0)
    end
    rtcb, pos = take(buf, pos, 1)
    cidl, pos = take(buf, pos, 2)
    cid, pos = take(buf, pos, cidl.unpack1("n"))
    svb, pos = take(buf, pos, 4)
    deps << { name: dname.force_encoding("UTF-8"), arg_types: arg_types,
              result: rtcb.getbyte(0), component_id: cid.force_encoding("UTF-8"),
              semantic_version: svb.unpack1("N") }
  end
  [deps, pos]
end

def take(buf, pos, n)
  raise "unexpected end of page data" if pos + n > buf.bytesize

  [buf.byteslice(pos, n), pos + n]
end

# Read a u16-length-prefixed UTF-8 string (the catalog's name/string encoding).
def take_str(buf, pos)
  nl, pos = take(buf, pos, 2)
  s, pos = take(buf, pos, nl.unpack1("n"))
  [s.force_encoding("UTF-8"), pos]
end

def read_meta(image, ps, slot)
  off = slot * ps
  return nil if off + ps > image.bytesize

  m = image.byteslice(off, ps)
  return nil unless m.byteslice(0, 4) == MAGIC
  return nil unless m.byteslice(4, 2).unpack1("n") == VERSION
  # Offsets 6/7: the encryption suite and its reserve (encrypted.md §6).
  suite = { [0, 0] => 0, [1, RESERVE] => 1 }[[m.getbyte(6), m.getbyte(7)]]
  return nil if suite.nil? || (suite == 1 && ps < 1024)
  return nil unless m.byteslice(8, 4).unpack1("N") == ps
  return nil unless meta_crc(m) == m.byteslice(32, 4).unpack1("N")

  first = suite.zero? ? ROOT_PAGE : ENC_FIRST_BODY
  meta = { txid: m.byteslice(12, 8).unpack1("Q>"), root_page: m.byteslice(20, 4).unpack1("N"),
           page_count: m.byteslice(24, 4).unpack1("N"), free_head: m.byteslice(28, 4).unpack1("N"),
           live: m.byteslice(56, 4).unpack1("N"), suite: suite, slot: slot }
  if suite == 1
    meta[:root_tag] = m.byteslice(64, 16)
    meta[:free_tag] = m.byteslice(80, 16)
    return nil if meta[:free_head].zero? && meta[:free_tag] != ZERO_TAG
  end
  return nil unless meta[:page_count] > first && meta[:page_count] <= image.bytesize / ps
  return nil unless (first...meta[:page_count]).cover?(meta[:root_page])
  return nil unless meta[:live] <= meta[:page_count] - first
  return nil unless meta[:free_head].zero? || (first...meta[:page_count]).cover?(meta[:free_head])
  return nil unless valid_manifest?(image, ps, m, meta)

  meta
end

def valid_manifest?(image, ps, m, meta)
  head, count, chain_count = m.byteslice(36, 12).unpack("N3")
  digest = m.byteslice(48, 8).unpack1("Q>")
  encrypted = meta[:suite] == 1
  first = encrypted ? ENC_FIRST_BODY : ROOT_PAGE
  # An encrypted meta's inline manifest follows its two tags and stops at its reserve (§6).
  lo, hi = encrypted ? [96, ps - RESERVE] : [64, ps]
  return false unless m.byteslice(60, 4) == "\x00".b * 4
  return false if count + chain_count > meta[:page_count] - first
  return false unless (chain_count.zero? && head.zero? && digest.zero?) ||
                      (chain_count.positive? && head >= first && head < meta[:page_count])
  return false if count.zero? && (!chain_count.zero? || !head.zero? || !digest.zero?)

  inline = [count, (hi - lo) / 12].min
  refs = m.byteslice(lo, inline * 12).unpack("NQ>" * inline).each_slice(2).to_a
  return false unless m.byteslice(lo + inline * 12, hi - lo - inline * 12).bytes.all?(&:zero?)
  chain = []
  chain_digest = 0
  remaining = count - inline
  page = head
  chain_count.times do |ordinal|
    return false unless page >= first && page < meta[:page_count] && !chain.include?(page)
    block = image.byteslice(page * ps, ps)
    return false unless block.getbyte(0) == 8 && block.byteslice(1, 3) == "\x00".b * 3
    return false unless page_crc(block) == block.byteslice(12, 4).unpack1("N")
    return false unless block.byteslice(16, 8).unpack1("Q>") == meta[:txid]
    return false unless block.byteslice(24, 4).unpack1("N") == ordinal
    return false unless block.byteslice(28, 4) == "\x00".b * 4
    entries = block.byteslice(4, 4).unpack1("N")
    return false unless entries == [remaining, (ps - 32) / 12].min
    refs.concat(block.byteslice(32, entries * 12).unpack("NQ>" * entries).each_slice(2).to_a)
    return false unless block.byteslice(32 + entries * 12, ps - 32 - entries * 12).bytes.all?(&:zero?)
    chain_digest = crc64(u32(page) + block, chain_digest)
    chain << page
    page = block.byteslice(8, 4).unpack1("N")
    remaining -= entries
  end
  return false unless page.zero? && remaining.zero? && chain_digest == digest
  ids = refs.map(&:first)
  return false unless ids == ids.sort.uniq && (ids & chain).empty?
  return false if count.positive? && !ids.include?(meta[:root_page])
  return false if count.positive? && meta[:free_head].positive? && !ids.include?(meta[:free_head])
  return false unless refs.all? do |index, expected|
    next false unless index >= first && index < meta[:page_count]
    block = image.byteslice(index * ps, ps)
    # A listed encrypted page is ciphertext: its digest is the whole check (§6).
    (encrypted || ((1..7).cover?(block.getbyte(0)) && page_crc(block) == block.byteslice(12, 4).unpack1("N"))) &&
      crc64(block) == expected
  end
  meta[:protected] = ids + chain
  true
end

# Logical free-list checks follow candidate selection, like catalog decoding. They
# reject the selected database rather than falling back to hide semantic corruption.
def validate_free_list!(image, ps, meta)
  seen = []
  free_pages = []
  page = meta[:free_head]
  until page.zero?
    raise "invalid free-list chain" unless page >= first_body && page < meta[:page_count] && !seen.include?(page)
    if $enc
      # An encrypted free-list page: tag-checked and decrypted; its successor's tag is recorded.
      pg = read_page(image, ps, page)
      raise "invalid free-list page" unless pg[:type] == 7
      raise "invalid free-list count" if pg[:item_count] > pg[:payload].bytesize / 4

      free_pages.concat(pg[:payload].byteslice(0, pg[:item_count] * 4).unpack("N*"))
      seen << page
      page = pg[:next_page]
      next
    end
    block = image.byteslice(page * ps, ps)
    raise "invalid free-list page" unless block.getbyte(0) == 7 && page_crc(block) == block.byteslice(12, 4).unpack1("N")
    entries = block.byteslice(4, 4).unpack1("N")
    raise "invalid free-list count" if entries > (ps - 16) / 4
    free_pages.concat(block.byteslice(16, entries * 4).unpack("N*"))
    seen << page
    page = block.byteslice(8, 4).unpack1("N")
  end
  unless free_pages == free_pages.sort.uniq && free_pages.all? { |id| id >= first_body && id < meta[:page_count] } &&
         (free_pages & (meta[:protected] + seen)).empty?
    raise "invalid free-list entry"
  end
  free_pages
end

def select_meta(image, ps)
  a = read_meta(image, ps, 0)
  b = read_meta(image, ps, 1)
  return (b && b[:txid] > a[:txid] ? b : a) if a && b
  return a if a
  return b if b

  raise "no valid meta page"
end

def read_page(image, ps, index)
  off = index * ps
  raise "page index out of range" if off + ps > image.bytesize

  return read_sealed_page(image, ps, index) if $enc

  p = image.byteslice(off, ps)
  # Verify the per-page checksum (v7) before trusting any header field (format.md *Page header*).
  raise "page checksum mismatch (corrupted page)" unless page_crc(p) == p.byteslice(12, 4).unpack1("N")
  $reached << index if $reached
  { type: p.getbyte(0), flags: p.getbyte(1), item_count: p.byteslice(4, 4).unpack1("N"),
    next_page: p.byteslice(8, 4).unpack1("N"), payload: p.byteslice(PAGE_HEADER, ps - PAGE_HEADER) }
end

# Read an encrypted body page (encrypted.md §4): the stored tag must equal the one the pointer that
# named the page bound, then it decrypts. A chain page's successor tag is recorded for the next read
# and stripped from the payload, so callers see the plaintext layout's fields.
def read_sealed_page(image, ps, index)
  stored = image.byteslice(index * ps, ps)
  expected = $enc[:tags][index] or raise "no pointer named page #{index}"
  nonce = stored.byteslice(ps - RESERVE, 24)
  tag = stored.byteslice(ps - TAG_LEN, TAG_LEN)
  raise "page does not match the pointer that names it" unless tag == expected

  body = Xaes.open($enc[:page_key], nonce, stored.byteslice(0, ps - RESERVE) + tag, ad_prefix(1, index))
  raise "page fails authentication" if body.nil?
  raise "encrypted page carries a CRC" unless body.byteslice(12, 4) == "\x00".b * 4

  $reached << index if $reached
  type = body.getbyte(0)
  nxt = body.byteslice(8, 4).unpack1("N")
  payload = body.byteslice(PAGE_HEADER, ps - RESERVE - PAGE_HEADER)
  if [PAGE_CATALOG, PAGE_OVERFLOW, 7].include?(type)
    succ = payload.byteslice(0, TAG_LEN)
    raise "the last chain page carries a successor tag" if nxt.zero? && succ != ZERO_TAG

    $enc[:tags][nxt] = succ unless nxt.zero?
    payload = payload.byteslice(TAG_LEN, payload.bytesize - TAG_LEN)
  end
  { type: type, flags: body.getbyte(1), item_count: body.byteslice(4, 4).unpack1("N"),
    next_page: nxt, payload: payload }
end

# Unlock an encrypted image for decoding (encrypted.md §5, §6): select the key-block copy, unwrap it
# with whichever known host key opens it, derive the subkeys, authenticate the selected meta over its
# manifest chain, and record the catalog root's tag.
def unlock_image(image, ps, meta)
  block = select_key_block(image.byteslice(2 * ps, ps), image.byteslice(3 * ps, ps))
  dek = DECODE_KEYS.lazy.map { |key| unwrap_key_block(block, key) }.find(&:itself) or raise "the key block does not unwrap"
  $enc = { ps: ps, tags: {}, page_key: Xaes.page_key(dek), meta_key: Xaes.meta_key(dek) }
  m = image.byteslice(meta[:slot] * ps, ps).dup
  nonce = m.byteslice(ps - RESERVE, 24)
  tag = m.byteslice(ps - TAG_LEN, TAG_LEN)
  m[32, 4] = u32(0)
  m[ps - RESERVE, RESERVE] = "\x00".b * RESERVE
  # The manifest-overflow chain the tag also covers: each page's index, then its bytes.
  chain = "".b
  id = m.byteslice(36, 4).unpack1("N")
  m.byteslice(44, 4).unpack1("N").times do
    raise "manifest chain leaves the image" if id.zero? || (id + 1) * ps > image.bytesize

    page = image.byteslice(id * ps, ps)
    chain << u32(id) << page
    id = page.byteslice(8, 4).unpack1("N")
  end
  raise "meta page fails authentication" if Xaes.open($enc[:meta_key], nonce, tag, ad_prefix(2, meta[:slot]) + m + chain).nil?

  $enc[:tags][meta[:root_page]] = meta[:root_tag]
  $enc[:tags][meta[:free_head]] = meta[:free_tag] unless meta[:free_head].zero?
end

# Decode a composite-type catalog entry's body (inverse of composite_type_entry_bytes); the caller
# has consumed the entry_kind byte. A nested-composite field stores the referenced type's NAME.
def decode_composite_type_entry(buf, pos)
  nl, pos = take(buf, pos, 2)
  name, pos = take(buf, pos, nl.unpack1("n"))
  fc, pos = take(buf, pos, 2)
  fields = []
  fc.unpack1("n").times do
    fnl, pos = take(buf, pos, 2)
    fname, pos = take(buf, pos, fnl.unpack1("n"))
    tcb, pos = take(buf, pos, 1)
    code = tcb.getbyte(0)
    precision = nil
    scale = nil
    if code == 14
      tnl, pos = take(buf, pos, 2)
      tname, pos = take(buf, pos, tnl.unpack1("n"))
      type = tname
    elsif code == 15
      # An array-typed field (spec/design/array.md §12): the element-type descriptor (inverse of
      # the code-15 branch above), then (below) the flags byte. The type string carries the "[]".
      elem, pos = read_array_element_type(buf, pos)
      type = "#{elem}[]"
    elsif code == 17
      # A range-typed field (v41): the one-byte range element descriptor (inverse of the code-17
      # branch above), then (below) the flags byte. The type string is the range's canonical name.
      ecb, pos = take(buf, pos, 1)
      type = RANGE_ELEM.key(CODETYPE.fetch(ecb.getbyte(0))) or raise "type code is not a valid range element subtype"
    else
      type = CODETYPE.fetch(code)
    end
    fl, pos = take(buf, pos, 1)
    raise "reserved composite field flag set" if (fl.getbyte(0) & ~0b1) != 0

    not_null = (fl.getbyte(0) & 1) != 0
    if type == "decimal"
      pb, pos = take(buf, pos, 2)
      sb, pos = take(buf, pos, 2)
      p = pb.unpack1("n")
      precision = p.zero? ? nil : p
      scale = p.zero? ? nil : sb.unpack1("n")
    end
    if type == "text"
      vb, pos = take(buf, pos, 4)
      n = vb.unpack1("N")
      varchar_len = n.zero? ? nil : n
    end
    tm, pos = decode_temporal_typmod(type, buf, pos)
    fields << { name: fname, type: type, not_null: not_null, precision: precision, scale: scale,
                varchar_len: varchar_len, temporal: tm }
  end
  [{ name: name, fields: fields }, pos]
end

def decode_table_entry(buf, pos)
  name_len, pos = take(buf, pos, 2)
  name, pos = take(buf, pos, name_len.unpack1("n"))
  col_count, pos = take(buf, pos, 2)
  columns = []
  col_count.unpack1("n").times do
    cnl, pos = take(buf, pos, 2)
    cname, pos = take(buf, pos, cnl.unpack1("n"))
    tc, pos = take(buf, pos, 1)
    # A composite column (v9, type_code 14): flags, then the type NAME in the typmod slot
    # (spec/design/composite.md §3), then (v44, flags bit3) the expression DEFAULT's text. Only
    # bit1 (not_null) and bit3 (default_is_expr) may be set.
    if tc.getbyte(0) == 14
      cflags, pos = take(buf, pos, 1)
      cf = cflags.getbyte(0)
      raise "reserved flag bit0 set (retired primary_key bit — v5)" if (cf & 0b01) != 0
      raise "invalid composite column flags (v44)" if (cf & ~0b1010 & 0xff) != 0

      tnl, pos = take(buf, pos, 2)
      tname, pos = take(buf, pos, tnl.unpack1("n"))
      cdefault_expr = nil
      if (cf & 0b1000) != 0
        del, pos = take(buf, pos, 2)
        de, pos = take(buf, pos, del.unpack1("n"))
        cdefault_expr = de.force_encoding("UTF-8")
      end
      columns << { name: cname, type: tname, pk: false, not_null: (cf & 0b10) != 0,
                   precision: nil, scale: nil, varchar_len: nil, default: :none,
                   default_expr: cdefault_expr, identity: nil, collation: nil, temporal: nil }
      next
    end
    # An array column (v10, type_code 15): flags, then the element-type descriptor
    # (spec/design/array.md §3). The column type is the element type's string + "[]". No default.
    if tc.getbyte(0) == 15
      aflags, pos = take(buf, pos, 1)
      af = aflags.getbyte(0)
      raise "reserved flag bit0 set (retired primary_key bit — v5)" if (af & 0b01) != 0

      elem_type, pos = read_array_element_type(buf, pos)
      # bit6 has_collation (v36): a text[] column's collation name follows the element descriptor.
      collation = nil
      if (af & 0b100_0000) != 0
        raise "has_collation set on a non-text array column (v36)" unless elem_type == "text"

        cl, pos = take(buf, pos, 2)
        cb, pos = take(buf, pos, cl.unpack1("n"))
        collation = cb.force_encoding("UTF-8")
      end
      columns << { name: cname, type: "#{elem_type}[]", pk: false, not_null: (af & 0b10) != 0,
                   precision: nil, scale: nil, varchar_len: nil, default: :none,
                   default_expr: nil, identity: nil, collation: collation, temporal: nil }
      next
    end
    # A range column (v15, type_code 17): flags, then the element-type descriptor — one scalar code
    # (spec/design/ranges.md §3). The column type is the element's range name. No default this slice.
    if tc.getbyte(0) == 17
      rflags, pos = take(buf, pos, 1)
      rf = rflags.getbyte(0)
      raise "reserved flag bit0 set (retired primary_key bit — v5)" if (rf & 0b01) != 0

      ecb, pos = take(buf, pos, 1)
      elem_type = CODETYPE.fetch(ecb.getbyte(0))
      rname = RANGE_ELEM.key(elem_type) or raise "type code is not a valid range element subtype"
      columns << { name: cname, type: rname, pk: false, not_null: (rf & 0b10) != 0,
                   precision: nil, scale: nil, varchar_len: nil, default: :none,
                   default_expr: nil, identity: nil, collation: nil, temporal: nil }
      next
    end
    flags, pos = take(buf, pos, 1)
    f = flags.getbyte(0)
    raise "reserved flag bit0 set (retired primary_key bit — v5)" if (f & 0b01) != 0
    raise "reserved column flag bit7 set" if (f & 0b1000_0000) != 0 # bit6 = has_collation (v17)
    # bit4 is_identity + bit5 identity_always (v15) — identity_always meaningful only with bit4.
    raise "identity_always set without is_identity" if (f & 0b11_0000) == 0b10_0000
    identity = if (f & 0b1_0000) != 0
                 (f & 0b10_0000) != 0 ? :always : :by_default
               end

    type = CODETYPE.fetch(tc.getbyte(0))
    precision = nil
    scale = nil
    varchar_len = nil
    if type == "decimal"
      pb, pos = take(buf, pos, 2)
      sb, pos = take(buf, pos, 2)
      p = pb.unpack1("n")
      precision = p.zero? ? nil : p
      scale = p.zero? ? nil : sb.unpack1("n")
    end
    # A text column carries its varchar(n) max length (u32, v22); 0 = unbounded (types.md §15).
    if type == "text"
      vb, pos = take(buf, pos, 4)
      n = vb.unpack1("N")
      varchar_len = n.zero? ? nil : n
    end
    # A timestamp/timestamptz/interval column carries its precision / qualifier typmod (v43).
    tm, pos = decode_temporal_typmod(type, buf, pos)
    # The default follows the typmod: a CONSTANT default (bit2) is a value via the value codec; an
    # EXPRESSION default (bit3, v8) is the expr-text (u16 length + UTF-8). Mutually exclusive.
    raise "column has both a constant and an expression default" if (f & 0b1100) == 0b1100

    default = :none
    default, pos = decode_value(type, buf, pos) if (f & 0b100) != 0
    default_expr = nil
    if (f & 0b1000) != 0
      el, pos = take(buf, pos, 2)
      de, pos = take(buf, pos, el.unpack1("n"))
      default_expr = de.force_encoding("UTF-8")
    end
    # The effective collation name (v17, flags bit6) — last in the per-column entry (collation.md §5).
    collation = nil
    if (f & 0b100_0000) != 0
      cl, pos = take(buf, pos, 2)
      cb, pos = take(buf, pos, cl.unpack1("n"))
      collation = cb.force_encoding("UTF-8")
    end
    columns << { name: cname, type: type, pk: false, not_null: (f & 0b10) != 0,
                 precision: precision, scale: scale, varchar_len: varchar_len, default: default,
                 default_expr: default_expr, identity: identity, collation: collation,
                 temporal: tm }
  end
  # The primary key (v5): member ordinals in KEY order; pk membership marks the columns.
  pk = []
  pkc, pos = take(buf, pos, 2)
  pkc.unpack1("n").times do
    ob, pos = take(buf, pos, 2)
    pk << ob.unpack1("n")
  end
  pk.each { |i| columns[i][:pk] = true }
  # CHECK constraints (v4): (name, expression text) pairs in evaluation order.
  checks = []
  cc, pos = take(buf, pos, 2)
  cc.unpack1("n").times do
    nl, pos = take(buf, pos, 2)
    cname, pos = take(buf, pos, nl.unpack1("n"))
    el, pos = take(buf, pos, 2)
    expr, pos = take(buf, pos, el.unpack1("n"))
    # v35: check_flags (bit0 has_host_deps; reserved bits must be 0), then the optional dep list.
    cfb, pos = take(buf, pos, 1)
    raise "a check_flags reserved bit is set (v35)" if (cfb.getbyte(0) & 0xFE) != 0
    check_deps = []
    check_deps, pos = read_host_deps(buf, pos) if (cfb.getbyte(0) & 1) != 0
    checks << { name: cname, expr: expr, host_deps: check_deps }
  end
  # Secondary indexes (v5): name + key-column ordinals + the v6 flags byte (bit0 unique)
  # + root page, in name order.
  indexes = []
  ic, pos = take(buf, pos, 2)
  ic.unpack1("n").times do
    nl, pos = take(buf, pos, 2)
    iname, pos = take(buf, pos, nl.unpack1("n"))
    kc, pos = take(buf, pos, 2)
    cols = []
    kc.unpack1("n").times do
      ob, pos = take(buf, pos, 2)
      ord = ob.unpack1("n")
      if ord == 0xFFFF
        # An expression key element (v26): the sentinel + u16 length + canonical text.
        el, pos = take(buf, pos, 2)
        et, pos = take(buf, pos, el.unpack1("n"))
        cols << { expr: et }
      else
        cols << ord
      end
    end
    fb, pos = take(buf, pos, 1)
    # bit0 unique (v6), bit1 has_predicate (v27), bit2 has_host_deps (v31 — extensibility.md §8.1).
    raise "reserved index flag set" if (fb.getbyte(0) & ~0b11_1111) != 0
    # v50: bit5 is_constraint — only a unique index backs a UNIQUE constraint.
    raise "a constraint index must be unique (v50)" if (fb.getbyte(0) & 32) != 0 && (fb.getbyte(0) & 1).zero?
    kb, pos = take(buf, pos, 1) # v13: index_kind byte (0 = btree, 1 = GIN); v20: 2 = GiST
    raise "reserved index kind (only 0=btree, 1=gin, 2=gist defined — v20)" if kb.getbyte(0) > 2
    has_predicate = (fb.getbyte(0) & 0b10) != 0
    # v45: a GiST index may be partial (the backing index of EXCLUDE … WHERE, gist.md §7.1).
    raise "a gin index cannot be partial (v27)" if has_predicate && kb.getbyte(0) == 1
    has_host_deps = (fb.getbyte(0) & 0b100) != 0
    raise "a non-btree index cannot have host-function dependencies (v31)" if has_host_deps && kb.getbyte(0) != 0 && !(kb.getbyte(0) == 2 && has_predicate)
    index_root, pos = read_ptr(buf, pos)
    # v27: the partial-index predicate canonical text follows index_root_page when bit1 is set.
    predicate = nil
    if has_predicate
      pl, pos = take(buf, pos, 2)
      predicate, pos = take(buf, pos, pl.unpack1("n"))
    end
    # v31: the host-function dependency list follows the predicate when bit2 is set (extensibility.md §8.1).
    host_deps = []
    host_deps, pos = read_host_deps(buf, pos) if has_host_deps
    timezone_deps = nil
    if (fb.getbyte(0) & 8) != 0
      raise "a non-btree index cannot have timezone dependencies" if kb.getbyte(0) != 0 && !(kb.getbyte(0) == 2 && has_predicate)
      db, pos = take(buf, pos, 1)
      dc, pos = take(buf, pos, 2)
      dynamic, count = db.getbyte(0), dc.unpack1("n")
      raise "invalid timezone dependency header" if dynamic > 1 || (dynamic == 0 && count == 0)
      timezone_deps = { dynamic: dynamic == 1, zones: [] }
      count.times do
        nl, pos = take(buf, pos, 2)
        dep_name, pos = take(buf, pos, nl.unpack1("n"))
        vl, pos = take(buf, pos, 2)
        version, pos = take(buf, pos, vl.unpack1("n"))
        cb, pos = take(buf, pos, 4)
        prev = timezone_deps[:zones].last
        raise "invalid timezone dependency order or name/version" if dep_name.empty? || version.empty? || (prev && prev[:name].b >= dep_name.b)
        timezone_deps[:zones] << { name: dep_name.force_encoding("UTF-8"), version: version.force_encoding("UTF-8"), checksum: cb.unpack1("N") }
      end
    end
    # v46: bit4 has_key_options — one option byte per key element after the timezone dependencies.
    orders = nil
    if (fb.getbyte(0) & 16) != 0
      raise "a non-btree index cannot have key options" if kb.getbyte(0) != 0
      orders = cols.map do
        ob, pos = take(buf, pos, 1)
        o = ob.getbyte(0)
        raise "reserved index key option bit set" if (o & ~0b11) != 0
        ORDER_SYMBOLS.key([(o & 1) != 0, (o & 2) != 0])
      end
      raise "index key options are all default" if orders.all? { |o| o == :asc }
    end
    indexes << { name: iname, cols: cols, unique: (fb.getbyte(0) & 1) != 0, constraint: (fb.getbyte(0) & 32) != 0, orders: orders,
                 kind: { 1 => "gin", 2 => "gist" }.fetch(kb.getbyte(0), "btree"),
                 root_page: index_root, predicate: predicate, host_deps: host_deps, timezone_deps: timezone_deps }
  end
  # Foreign keys (v11): name + local ordinals + referenced table + referenced ordinals + the
  # actions byte, in name order. An FK owns no B-tree (no root page).
  fks = []
  fc, pos = take(buf, pos, 2)
  fc.unpack1("n").times do
    nl, pos = take(buf, pos, 2)
    fname, pos = take(buf, pos, nl.unpack1("n"))
    lc, pos = take(buf, pos, 2)
    local = []
    lc.unpack1("n").times do
      ob, pos = take(buf, pos, 2)
      local << ob.unpack1("n")
    end
    rtl, pos = take(buf, pos, 2)
    rtable, pos = take(buf, pos, rtl.unpack1("n"))
    rc, pos = take(buf, pos, 2)
    ref = []
    rc.unpack1("n").times do
      ob, pos = take(buf, pos, 2)
      ref << ob.unpack1("n")
    end
    raise "fk column count mismatch (local != ref)" if local.size != ref.size

    ab, pos = take(buf, pos, 1)
    raise "reserved fk action bits set / unsupported action code" \
      if (ab.getbyte(0) & 0b1000_0000) != 0 || (ab.getbyte(0) & 0b111) > 4 || ((ab.getbyte(0) >> 3) & 0b111) > 4
    fks << { name: fname, local: local, ref_table: rtable, ref: ref, actions: ab.getbyte(0) }
  end
  # EXCLUDE constraints (v21): name + backing GiST index name + the (column ordinal, operator) element
  # vector, in name order (spec/design/gist.md §7/§8).
  exclusions = []
  ec, pos = take(buf, pos, 2)
  ec.unpack1("n").times do
    nl, pos = take(buf, pos, 2)
    ename, pos = take(buf, pos, nl.unpack1("n"))
    il, pos = take(buf, pos, 2)
    iname, pos = take(buf, pos, il.unpack1("n"))
    elc, pos = take(buf, pos, 2)
    elements = []
    elc.unpack1("n").times do
      ob, pos = take(buf, pos, 2)
      opb, pos = take(buf, pos, 1)
      raise "unsupported exclusion operator code (only 0=&&, 1== — v21)" if opb.getbyte(0) > 1
      elements << [ob.unpack1("n"), { 0 => "&&", 1 => "=" }.fetch(opb.getbyte(0))]
    end
    exclusions << { name: ename, index: iname, elements: elements }
  end
  root, pos = read_ptr(buf, pos)
  count_raw, pos = take(buf, pos, 8)
  row_count = count_raw.unpack1("q>")
  raise "negative table row_count" if row_count.negative?
  raise "root_data_page/row_count invariant violated" unless root.zero? == row_count.zero?

  [{ name: name, columns: columns, pk: pk, checks: checks, indexes: indexes, fks: fks,
     exclusions: exclusions, root_data_page: root, row_count: row_count }, pos]
end

# Read one value via the value codec (inverse of encode_value): a presence tag, then — when
# present — the type's body. 0x01 = NULL. Shared by row records and the catalog default.
# Decode a decimal value's body (flags + u16 scale + u16 ndigits + base-10^4 groups). Shared by the
# inline decimal branch and by external reconstruction (a spilled decimal's payload is this body).
def decode_decimal_body(buf, pos)
  fb, pos = take(buf, pos, 1)
  scb, pos = take(buf, pos, 2)
  ndb, pos = take(buf, pos, 2)
  coeff = 0
  ndb.unpack1("n").times do
    gb, pos = take(buf, pos, 2)
    raise "decimal digit group out of range" if gb.unpack1("n") > 9999

    coeff = coeff * 10_000 + gb.unpack1("n")
  end
  [render_decimal((fb.getbyte(0) & 1) != 0, scb.unpack1("n"), coeff), pos]
end

# Reconstruct a value from the P(v) content gathered from its overflow chain (large-values.md §12).
def value_from_payload(type, payload)
  case type
  when "text" then payload.dup.force_encoding("UTF-8")
  when "bytea" then payload.dup.force_encoding("ASCII-8BIT")
  when "json" then payload.dup.force_encoding("UTF-8")
  when "jsonb" then decode_jsonb_body(payload, 0).first
  when "decimal" then decode_decimal_body(payload, 0).first
  else raise "a non-spillable type was stored external"
  end
end

# The tag ending an encrypted external value's pointer (encrypted.md §2), recorded for its chain.
def read_pointer_tag(first, buf, pos)
  return pos unless $enc

  tag, pos = take(buf, pos, TAG_LEN)
  $enc[:tags][first] = tag.b
  pos
end

# Gather `len` bytes of an external value's payload by following its overflow chain from `first`:
# each page is page_type 4, carries item_count payload bytes, chained via next_page (0 terminates).
def read_overflow_chain(first, len, fetch)
  out = +"".b
  seen = Set.new
  p = first
  while out.bytesize < len
    raise "overflow chain ended before the value length" if p.zero?
    raise "overflow chain revisits a page" unless seen.add?(p)
    # Chain exclusivity (large-values.md §15): a rewritten record may keep an unchanged value's chain,
    # but no two records of one generation share a chain page.
    raise "overflow page #{p} is referenced twice" if $claimed && !$claimed.add?(p)

    pg = fetch.call(p)
    raise "expected an overflow page" unless pg[:type] == PAGE_OVERFLOW

    n = pg[:item_count]
    raise "overflow page slab out of range" if n.zero? || n > pg[:payload].bytesize || out.bytesize + n > len

    out << pg[:payload].byteslice(0, n)
    p = pg[:next_page]
  end
  out
end

# Read one value via the value codec (inverse of encode_value/emit_record): a presence tag, then —
# when present-inline — the type's body, or — when external (0x02) — a pointer whose payload is
# gathered from the overflow chain via `fetch` and reconstructed by type (large-values.md §12).
# `fetch` is nil only where no value can be external (a catalog default). 0x01 = NULL.
def decode_value(type, buf, pos, fetch = nil)
  tag, pos = take(buf, pos, 1)
  t = tag.getbyte(0)
  return [nil, pos] if t == 0x01
  if t == TAG_EXTERNAL
    fpb, pos = take(buf, pos, 4)
    lnb, pos = take(buf, pos, 4)
    pos = read_pointer_tag(fpb.unpack1("N"), buf, pos)
    raise "external value with no overflow reader" if fetch.nil?

    return [value_from_payload(type, read_overflow_chain(fpb.unpack1("N"), lnb.unpack1("N"), fetch)), pos]
  end
  if t == TAG_INLINE_COMP
    rlb, pos = take(buf, pos, 4)
    clb, pos = take(buf, pos, 2)
    comp, pos = take(buf, pos, clb.unpack1("n"))
    return [value_from_payload(type, LZ4.decompress(comp, rlb.unpack1("N"))), pos]
  end
  if t == TAG_EXTERNAL_COMP
    fpb, pos = take(buf, pos, 4)
    slb, pos = take(buf, pos, 4)
    rlb, pos = take(buf, pos, 4)
    pos = read_pointer_tag(fpb.unpack1("N"), buf, pos)
    raise "external value with no overflow reader" if fetch.nil?

    comp = read_overflow_chain(fpb.unpack1("N"), slb.unpack1("N"), fetch)
    return [value_from_payload(type, LZ4.decompress(comp, rlb.unpack1("N"))), pos]
  end
  raise "invalid value presence tag" unless t.zero?

  if (elem = array_elem(type))
    # An array value body (inverse of encode_array_body): ndim/flags/dims, optional bitmap, elements.
    return decode_array_body(elem, buf, pos)
  end

  if (relem = range_elem(type))
    # A range value body (inverse of encode_range_body): flags byte + present bound bodies.
    return decode_range_body(relem, buf, pos)
  end

  if (fields = composite_fields(type))
    # A composite value body (inverse of encode_composite_body): bitmap, then each present field.
    return decode_composite_body(fields, buf, pos)
  end

  case type
  when "text"
    len, pos = take(buf, pos, 2)
    sb, pos = take(buf, pos, len.unpack1("n"))
    [sb.dup.force_encoding("UTF-8"), pos]
  when "boolean"
    bb, pos = take(buf, pos, 1)
    [bb.getbyte(0) == 1, pos]
  when "decimal"
    decode_decimal_body(buf, pos)
  when "bytea"
    len, pos = take(buf, pos, 2)
    bb, pos = take(buf, pos, len.unpack1("n"))
    [bb.dup.force_encoding("ASCII-8BIT"), pos] # raw bytes, no UTF-8 assertion
  when "uuid"
    ub, pos = take(buf, pos, 16) # fixed 16 bytes, no length prefix
    [uuid_from_bytes(ub), pos]
  when "json"
    len, pos = take(buf, pos, 2) # verbatim text, length-prefixed like text (spec/design/json.md §4)
    sb, pos = take(buf, pos, len.unpack1("n"))
    [sb.dup.force_encoding("UTF-8"), pos]
  when "jsonb"
    decode_jsonb_body(buf, pos) # the self-delimiting tagged-node tree (§2)
  when "interval"
    mb, pos = take(buf, pos, 4)
    db, pos = take(buf, pos, 4)
    ub, pos = take(buf, pos, 8)
    [[mb.unpack1("l>"), db.unpack1("l>"), ub.unpack1("q>")], pos]
  when "f64"
    vb, pos = take(buf, pos, 8)
    [vb.unpack1("G"), pos] # a canonical-NaN body unpacks to a Float NaN; content_equal? handles ==
  when "f32"
    vb, pos = take(buf, pos, 4)
    [vb.unpack1("g"), pos]
  else
    vb, pos = take(buf, pos, WIDTH.fetch(type))
    [decode_int(WIDTH.fetch(type), vb), pos]
  end
end

# Read an unsigned LEB128 varint (inverse of write_uvarint, spec/design/json.md §2.1) -> [value, pos].
def read_uvarint(buf, pos)
  result = 0
  shift = 0
  loop do
    b, pos = take(buf, pos, 1)
    byte = b.getbyte(0)
    result |= (byte & 0x7f) << shift
    return [result, pos] if (byte & 0x80).zero?

    shift += 7
    raise "jsonb varint overflows u64" if shift >= 64
  end
end

# Decode a jsonb node body (inverse of encode_jsonb_body, spec/design/json.md §2.1) into the tagged
# Ruby form -> [node, pos]. A nonzero flag nibble or the reserved NTAG_STRING_DICT (0x5) is data
# corruption (XX001 in the cores). Numbers decode to [:num, rendered-decimal-string].
def decode_jsonb_body(buf, pos)
  tb, pos = take(buf, pos, 1)
  tag = tb.getbyte(0)
  raise "jsonb node tag has a reserved flag bit set" if (tag & 0xf0) != 0

  case tag & 0x0f
  when 0x0 then [[:null], pos]
  when 0x1 then [[:bool, false], pos]
  when 0x2 then [[:bool, true], pos]
  when 0x3
    dec, pos = decode_decimal_body(buf, pos)
    [[:num, dec], pos]
  when 0x4
    s, pos = read_jsonb_string(buf, pos)
    [[:str, s], pos]
  when 0x5 then raise "jsonb string-dictionary reference before the dictionary slice"
  when 0x6
    count, pos = read_uvarint(buf, pos)
    elems = []
    count.times do
      node, pos = decode_jsonb_body(buf, pos)
      elems << node
    end
    [[:arr, elems], pos]
  when 0x7
    count, pos = read_uvarint(buf, pos)
    members = []
    count.times do
      ktb, pos = take(buf, pos, 1)
      ktag = ktb.getbyte(0)
      raise "jsonb object key is not a string node" unless (ktag & 0x0f) == 0x4 && (ktag & 0xf0).zero?

      key, pos = read_jsonb_string(buf, pos)
      val, pos = decode_jsonb_body(buf, pos)
      members << [key, val]
    end
    [[:obj, members], pos]
  else raise "unknown jsonb node tag"
  end
end

# Read a NTAG_STRING payload (varint len ‖ utf8) after its tag -> [string, pos].
def read_jsonb_string(buf, pos)
  len, pos = read_uvarint(buf, pos)
  sb, pos = take(buf, pos, len)
  [sb.dup.force_encoding("UTF-8"), pos]
end

# Decode an array value body (inverse of encode_array_body, spec/design/array.md §4): ndim u8 ‖
# flags u8 ‖ per-dim (len u32 ‖ lb i32) ‖ [bitmap if HAS_NULLS] ‖ element bodies. ndim 0 = the empty
# array; ndim through 6 is accepted (multidim + custom lower bounds, §12). A present element has no
# tag, so a 0x00 is synthesized onto the remaining buffer and the real cursor advanced by what
# decode_value consumed past it (npos - 1) — the same trick decode_composite_body uses. Returns a
# flat element Array for a default-lower-bound 1-D value (matching the simple fixture form), else a
# Hash {dims:, lbounds:, elements:} (the shaped form). Returns [value, pos].
def decode_array_body(elem_type, buf, pos)
  nb, pos = take(buf, pos, 1)
  fb, pos = take(buf, pos, 1)
  ndim = nb.getbyte(0)
  flags = fb.getbyte(0)
  raise "array flags has a reserved bit set" if (flags & ~0x01) != 0
  return [[], pos] if ndim.zero? # empty array
  raise "array ndim exceeds the maximum of 6" if ndim > 6

  dims = []
  lbounds = []
  n = 1
  ndim.times do
    lenb, pos = take(buf, pos, 4)
    lbb, pos = take(buf, pos, 4)
    dims << lenb.unpack1("N")
    lbounds << lbb.unpack1("l>") # lower bound (i32 BE)
    n *= dims.last
  end
  has_nulls = (flags & 0x01) != 0
  bitmap = nil
  bitmap, pos = take(buf, pos, (n + 7) / 8) if has_nulls
  elems = []
  n.times do |i|
    if has_nulls && (bitmap.getbyte(i / 8) & (0x80 >> (i % 8))) != 0
      elems << nil
    else
      v, npos = decode_value(elem_type, "\x00".b + buf.byteslice(pos..), 0)
      pos += (npos - 1)
      elems << v
    end
  end
  # A default-lower-bound 1-D value decodes to the simple flat Array; anything shaped to the Hash.
  value = if ndim == 1 && lbounds[0] == 1
            elems
          else
            { dims: dims, lbounds: lbounds, elements: elems }
          end
  [value, pos]
end

# Decode a range value body (inverse of encode_range_body, spec/design/ranges.md §4): the flags byte
# (EMPTY/LB_INF/UB_INF/LB_INC/UB_INC) then the present bound bodies (each the element's value-codec
# body, no tag — synthesized 0x00 prepended like decode_array_body). Returns [:empty, pos] for the
# empty range, else [{lower:, upper:, lower_inc:, upper_inc:}, pos] (a nil bound is infinite).
def decode_range_body(elem_type, buf, pos)
  fb, pos = take(buf, pos, 1)
  flags = fb.getbyte(0)
  raise "range flags has a reserved bit set" if (flags & ~0x1f) != 0
  return [:empty, pos] if (flags & 0x01) != 0

  lb_inf = (flags & 0x02) != 0
  ub_inf = (flags & 0x04) != 0
  lower = nil
  unless lb_inf
    lower, npos = decode_value(elem_type, "\x00".b + buf.byteslice(pos..), 0)
    pos += (npos - 1)
  end
  upper = nil
  unless ub_inf
    upper, npos = decode_value(elem_type, "\x00".b + buf.byteslice(pos..), 0)
    pos += (npos - 1)
  end
  [{ lower: lower, upper: upper, lower_inc: (flags & 0x08) != 0, upper_inc: (flags & 0x10) != 0 }, pos]
end

# Decode a composite value body (inverse of encode_composite_body): read the null bitmap
# (ceil(n/8) bytes), then for each field either NULL (bit set, no body) or its value body decoded
# recursively (no per-field presence tag). Returns [field-value array, pos]. Composite fields are
# never external (an over-cap record's WHOLE composite would spill — not handled here, no such
# fixture), so `fetch` is unneeded; the inline body is self-contained.
def decode_composite_body(fields, buf, pos)
  nbytes = (fields.size + 7) / 8
  bitmap, pos = take(buf, pos, nbytes)
  vals = []
  fields.each_with_index do |f, i|
    if (bitmap.getbyte(i / 8) & (0x80 >> (i % 8))) != 0
      vals << nil
    else
      # A present field has no tag; re-prepend a 0x00 present tag onto the remaining buffer so
      # `decode_value` reads the body, then advance the real cursor by the bytes it consumed past
      # that synthetic tag (npos - 1).
      v, npos = decode_value(f[:type], "\x00".b + buf.byteslice(pos..), 0)
      pos += (npos - 1)
      vals << v
    end
  end
  [vals, pos]
end

# Parse a v24 PAX leaf payload (format.md "Leaf node") into [keys, regions]: keys[i] the i-th key;
# regions[c] a class-shaped hash — { width:, bitmap:, body: } for a fixed-width column (bitmap the
# raw null-bitmap bytes, body the dense slot bytes) or { ends:, body: } for a variable column
# (ends the N end offsets, body the tagged value blob). `cols` are the table's column defs.
def parse_pax_leaf(payload, n, cols)
  pos = 0
  key_end = (0...n).map { |_| v = payload.byteslice(pos, 4).unpack1("N"); pos += 4; v }
  raise "PAX leaf key directory not ascending" unless key_end.each_cons(2).all? { |a, b| a <= b }

  key_blob = pos
  raise "PAX leaf key blob overruns page" if key_blob + (key_end.last || 0) > payload.bytesize

  keys = (0...n).map do |i|
    lo = i.zero? ? 0 : key_end[i - 1]
    payload.byteslice(key_blob + lo, key_end[i] - lo)
  end
  pos = key_blob + (n.zero? ? 0 : key_end[n - 1])
  k = cols.size
  col_start = (0..k).map { |_| v = payload.byteslice(pos, 4).unpack1("N"); pos += 4; v }
  raise "PAX leaf column directory start mismatch" unless col_start[0] == pos

  regions = cols.each_with_index.map do |c, ci|
    start = col_start[ci]
    flags = payload.getbyte(start)
    raise "PAX leaf region flags has a reserved bit set" unless flags.zero?

    if (w = fixed_width(c[:type]))
      bitmap = payload.byteslice(start + 1, (n + 7) / 8)
      body = start + 1 + (n + 7) / 8
      raise "fixed region extent mismatch" unless body + n * w == col_start[ci + 1]

      { width: w, bitmap: bitmap, body: payload.byteslice(body, n * w) }
    else
      ends = (0...n).map { |i| payload.byteslice(start + 1 + i * 4, 4).unpack1("N") }
      body = start + 1 + 4 * n
      raise "variable region extent mismatch" unless body + (ends.last || 0) == col_start[ci + 1]

      { ends: ends, body: payload.byteslice(body, (ends.last || 0)) }
    end
  end
  [keys, regions]
end

# Decode value (record i, column c) from a parsed v24 leaf region: nil from the bitmap (fixed) or
# the zero-length span (variable); a fixed slot is the untagged body (re-tagged for decode_value);
# a variable span is the tagged codec bytes as-is.
def leaf_value(region, type, i, fetch)
  if region[:width]
    w = region[:width]
    return nil if (region[:bitmap].getbyte(i / 8) & (0x80 >> (i % 8))) != 0

    v, = decode_value(type, "\x00".b + region[:body].byteslice(i * w, w), 0, fetch)
    v
  else
    lo = i.zero? ? 0 : region[:ends][i - 1]
    len = region[:ends][i] - lo
    return nil if len.zero?

    v, = decode_value(type, region[:body].byteslice(lo, len), 0, fetch)
    v
  end
end

# In-order walk of a table's B+tree -> rows in ascending key order (format.md v24: interior nodes
# are a record-free routing skeleton; all records live in leaves). Independent of how the tree was
# built. An external value's chain is followed through `fetch` (large-values.md §12).
def read_tree_rows(image, ps, root_page, columns)
  rows = []
  seen = Set.new
  fetch = ->(p) { read_page(image, ps, p) }
  walk = lambda do |idx|
    return if idx.zero?
    raise "B-tree page reached twice" unless seen.add?(idx)

    pg = read_page(image, ps, idx)
    case pg[:type]
    when PAGE_LEAF
      n = pg[:item_count]
      _keys, regions = parse_pax_leaf(pg[:payload], n, columns)
      (0...n).each do |i|
        rows << columns.each_with_index.map { |c, ci| leaf_value(regions[ci], c[:type], i, fetch) }
      end
    when PAGE_INTERIOR
      n = pg[:item_count]
      pos = 0
      children = (0..n).map do
        cp, pos = read_ptr(pg[:payload], pos)
        cp
      end
      children.each { |cp| walk.call(cp) }
    else
      raise "expected a B-tree node page, got type #{pg[:type]}"
    end
  end
  walk.call(root_page)
  rows
end

# In-order walk of an INDEX B+tree -> entry keys in ascending order. An index record is its key
# alone (format.md "Index trees"); interior separators are routing copies, not entries.
def read_tree_keys(image, ps, root_page)
  keys = []
  seen = Set.new
  walk = lambda do |idx|
    return if idx.zero?
    raise "B-tree page reached twice" unless seen.add?(idx)

    pg = read_page(image, ps, idx)
    case pg[:type]
    when PAGE_LEAF
      # An index leaf is a v24 leaf with K=0 value columns: key directory + key blob + a 1-entry
      # column directory. parse_pax_leaf returns the keys.
      leaf_keys, = parse_pax_leaf(pg[:payload], pg[:item_count], [])
      leaf_keys.each { |kk| keys << kk }
    when PAGE_INTERIOR
      n = pg[:item_count]
      pos = 0
      children = (0..n).map do
        cp, pos = read_ptr(pg[:payload], pos)
        cp
      end
      children.each { |cp| walk.call(cp) }
    else
      raise "expected a B-tree node page, got type #{pg[:type]}"
    end
  end
  walk.call(root_page)
  keys
end

def decode_statistics_entry(buf, pos, tables, statistics, expected)
  kb, pos = take(buf, pos, 1)
  kind = kb.getbyte(0)
  name, pos = take_str(buf, pos)
  cb, pos = take(buf, pos, 2)
  column = cb.unpack1("n")
  table = tables.find { |t| t[:name] == name } or raise "statistics reference unknown table"
  raise "statistics reference unknown column" if column >= table[:columns].size

  key = [name, column]
  if kind.zero?
    raise "duplicate statistics summary" if expected.key?(key)
    fb, pos = take(buf, pos, 1)
    flags = fb.getbyte(0)
    raise "reserved statistics flag" unless (flags & ~7).zero?
    raw, pos = take(buf, pos, 32)
    analyzed, null_count, width_sum, distinct = raw.unpack("q>q>q>q>")
    sb, pos = take(buf, pos, 8)
    sample_rows, sample_nonnull = sb.unpack("NN")
    counts, pos = take(buf, pos, 4)
    mcv_count, histogram_count = counts.unpack("nn")
    distribution = (flags & 2) != 0
    terms = nil
    term_count = 0
    if (flags & 4) != 0
      tb, pos = take(buf, pos, 18)
      occurrences, untracked = tb.unpack("q>q>")
      term_count = tb.byteslice(16, 2).unpack1("n")
      raise "invalid statistics term summary" if occurrences.negative? || untracked.negative? ||
                                                 untracked > analyzed - null_count || term_count > 100
      terms = { occurrences: occurrences, untracked_rows: untracked, entries: [] }
    end
    raise "statistics term eligibility mismatch" if terms.nil? == term_eligible?(table[:columns][column][:type])
    raise "invalid statistics summary" if analyzed.negative? || null_count.negative? || null_count > analyzed ||
                                          width_sum.negative? || sample_rows > [analyzed, 30_000].min ||
                                          sample_nonnull > sample_rows || mcv_count > 100 || histogram_count > 101 ||
                                          (!histogram_count.zero? && histogram_count < 2) ||
                                          (distribution ? !distinct.between?(0, analyzed - null_count) : !distinct.zero?)
    item = { table: name, column: column, stale: (flags & 1) != 0, analyzed_rows: analyzed,
             null_count: null_count, width_sum: width_sum,
             distinct_count: distribution ? distinct : nil, sample_rows: sample_rows,
             sample_nonnull_rows: sample_nonnull, mcv: [], histogram: [] }
    item[:terms] = terms if terms
    statistics << item
    expected[key] = [mcv_count, histogram_count, term_count]
    return pos
  end

  item = statistics.find { |s| s[:table] == name && s[:column] == column } or
    raise "statistics item precedes summary"
  counts = expected.fetch(key)
  ob, pos = take(buf, pos, 2)
  ordinal = ob.unpack1("n")
  if kind == 3
    raise "statistics term under a term-ineligible summary" unless item[:terms]

    rb, pos = take(buf, pos, 2 + 8)
    rows = rb.unpack1("q>")
    length = rb.byteslice(8, 2).unpack1("n")
    raise "invalid statistics term length" unless length.between?(1, 128)

    term, pos = take(buf, pos, length)
    entries = item[:terms][:entries]
    raise "invalid statistics term ordinal/rows" unless ordinal == entries.size && ordinal < counts[2] &&
                                                        rows.between?(1, item[:analyzed_rows] - item[:null_count])
    raise "duplicate statistics term" if entries.any? { |t, _| t == term }
    if (last = entries.last) && (rows > last[1] || (rows == last[1] && term < last[0]))
      raise "statistics terms are out of order"
    end

    entries << [term.b, rows]
    return pos
  end
  if kind == 1
    fb, pos = take(buf, pos, 4)
    frequency = fb.unpack1("N")
    raise "invalid statistics MCV ordinal/frequency" unless ordinal == item[:mcv].size &&
                                                              ordinal < counts[0] &&
                                                              frequency.between?(1, item[:sample_nonnull_rows])
  elsif kind != 2
    raise "unknown statistics entry kind"
  elsif ordinal != item[:histogram].size || ordinal >= counts[1]
    raise "invalid statistics histogram ordinal"
  end
  lb, pos = take(buf, pos, 2)
  length = lb.unpack1("n")
  raise "invalid statistics value length" unless length.between?(1, 128)
  encoded, pos = take(buf, pos, length)
  value, value_pos = decode_value(table[:columns][column][:type], encoded, 0)
  raise "noncanonical statistics value" unless value_pos == encoded.bytesize &&
                                               encode_value(table[:columns][column][:type], value) == encoded &&
                                               !value.nil?
  kind == 1 ? item[:mcv] << [value, frequency] : item[:histogram] << value
  pos
end

def decode_image(image)
  ps = image.byteslice(8, 4).unpack1("N")
  meta = select_meta(image, ps)
  unlock_image(image, ps, meta) if meta[:suite] == 1
  decode_unlocked(image, ps, meta)
ensure
  $enc = nil
end

def decode_unlocked(image, ps, meta)
  free_pages = validate_free_list!(image, ps, meta)
  # Every page the decode reads from the catalog root is live (v34): the catalog chain, each tree
  # and GiST node, and each overflow page. The meta's count must equal that reachable set.
  $reached = Set.new
  $claimed = Set.new
  types = []
  sequences = []
  collations = []
  tables = []
  views = []
  statistics = []
  statistics_expected = {}
  # Composite types in scope for the recursive value codec; populated as the (types-first) catalog
  # is read, so every composite a table row references is registered before its rows are decoded.
  $ctypes = {}
  cat = meta[:root_page]
  while cat != 0
    pg = read_page(image, ps, cat)
    raise "expected a catalog page" unless pg[:type] == PAGE_CATALOG && (pg[:flags] & ~1).zero?

    # A catalog entry longer than a page continues on the following chain pages (v48).
    nxt = pg[:next_page]
    payload = pg[:payload]
    if pg[:flags] & 1 == 1
      raise "a continued catalog page holds other entries" unless pg[:item_count] == 1

      payload = payload.dup
      loop do
        raise "a continued catalog entry ends the chain" if nxt.zero?

        cp = read_page(image, ps, nxt)
        raise "bad catalog continuation page" unless cp[:type] == PAGE_CATALOG && cp[:item_count].zero? &&
                                                        (cp[:flags] & ~1).zero?

        payload << cp[:payload]
        nxt = cp[:next_page]
        break if (cp[:flags] & 1).zero?
      end
    end

    pos = 0
    pg[:item_count].times do
      kb, pos = take(payload, pos, 1) # entry_kind: 0 table, 1 composite, 2 sequence, 3 collation
      kind = kb.getbyte(0)
      if kind == 1
        ct, pos = decode_composite_type_entry(payload, pos)
        types << ct
        $ctypes[ct[:name]] = ct[:fields]
        next
      end
      if kind == 2
        s, pos = decode_sequence_entry(payload, pos)
        sequences << s
        next
      end
      if kind == 3
        c, pos = decode_collation_entry(payload, pos)
        collations << c
        next
      end
      if kind == 4
        pos = decode_statistics_entry(payload, pos, tables, statistics, statistics_expected)
        next
      end
      if kind == 5
        v, pos = decode_view_entry(payload, pos)
        views << v
        next
      end
      raise "unknown catalog entry kind #{kind}" unless kind.zero?

      entry, pos = decode_table_entry(payload, pos)
      rows = read_tree_rows(image, ps, entry[:root_data_page], entry[:columns])
      raise "persisted row_count does not match decoded rows" unless entry[:row_count] == rows.length
      indexes = entry[:indexes].map do |ix|
        raw = if ix[:kind] == "gist"
                read_gist_keys(image, ps, ix[:root_page]).sort
              else
                read_tree_keys(image, ps, ix[:root_page])
              end
        { name: ix[:name], cols: ix[:cols], timezone_deps: ix[:timezone_deps], orders: ix[:orders],
          entries: raw.map { |k| k.unpack1("H*") } }
      end
      tables << { name: entry[:name], columns: entry[:columns], pk: entry[:pk],
                  checks: entry[:checks], indexes: indexes, rows: rows }
    end
    cat = nxt
  end
  statistics_expected.each do |(table, column), (mcv_count, histogram_count, term_count)|
    item = statistics.find { |s| s[:table] == table && s[:column] == column }
    raise "incomplete statistics group" unless item && item[:mcv].size == mcv_count &&
                                               item[:histogram].size == histogram_count &&
                                               (item[:terms] ? item[:terms][:entries].size : 0) == term_count
  end
  reached = $reached
  $reached = nil
  $claimed = nil
  raise "live page count #{meta[:live]} does not match #{reached.size} reachable pages" unless meta[:live] == reached.size
  raise "a free-list page is reachable" unless (free_pages & reached.to_a).empty?
  { types: types, sequences: sequences, collations: collations, tables: tables,
    statistics: statistics, views: views, meta: meta, free_pages: free_pages }
end

# --check-image FILE… (spec/design/simulation-testing.md §12, slice S6): decode arbitrary database
# images — the crash images the simulation runners dump — as an independent fourth voice. Selects
# the generation recovery must select (meta validation + the full commit manifest), validates the
# free list, decodes every table, index, and overflow chain (no overflow page named by two values —
# large-values.md §15), and checks the live page count against the reachable set and the free list
# against both. Prints one JSON line per image, the fields the runners' transcripts also report, or
# the error that made the image undecodable.
def check_images(paths)
  require "json"
  failed = false
  paths.each do |path|
    image = File.binread(path)
    decoded = decode_image(image)
    meta = decoded[:meta]
    puts JSON.generate({ file: path, version: meta[:txid], live: meta[:live], pages: meta[:page_count],
                         free: decoded[:free_pages].size })
  rescue StandardError => e
    failed = true
    puts JSON.generate({ file: path, error: e.message })
  end
  exit(failed ? 1 : 0)
end

# Decode a sequence catalog entry's body (inverse of sequence_entry_bytes); the caller has consumed
# the entry_kind byte. Six i64 fields (`q>`) + the flags byte (spec/design/sequences.md §3).
def decode_sequence_entry(buf, pos)
  nl, pos = take(buf, pos, 2)
  name, pos = take(buf, pos, nl.unpack1("n"))
  fields = {}
  %i[increment min_value max_value start cache last_value].each do |f|
    raw, pos = take(buf, pos, 8)
    fields[f] = raw.unpack1("q>")
  end
  fb, pos = take(buf, pos, 1)
  flags = fb.getbyte(0)
  raise "reserved sequence flag set" if (flags & ~0b111) != 0

  # The OWNED BY tail (v13): present iff bit2 (has_owner) is set.
  owned_by = nil
  if (flags & 0b100) != 0
    tl, pos = take(buf, pos, 2)
    table, pos = take(buf, pos, tl.unpack1("n"))
    col_raw, pos = take(buf, pos, 2)
    owned_by = [table, col_raw.unpack1("n")]
  end

  [{ name: name, increment: fields[:increment], min_value: fields[:min_value],
     max_value: fields[:max_value], start: fields[:start], cache: fields[:cache],
     cycle: (flags & 0b1) != 0, last_value: fields[:last_value],
     is_called: (flags & 0b10) != 0, owned_by: owned_by }, pos]
end

# The sequence content a fixture should decode to (name-sorted).
def expected_sequences(fx)
  (fx[:sequences] || []).sort_by { |s| s[:name] }
end

def expected_collations(fx)
  (fx[:collations] || []).sort_by { |c| c[:name] }
                         .map { |c| { name: c[:name], default: !!c[:default], unicode: c[:unicode], cldr: c[:cldr], desc: c[:desc] } }
end

def expected_statistics(fx)
  fx[:tables].sort_by { |t| t[:name] }.flat_map do |t|
    table_statistics(t).sort_by { |s| s[:column] }.map { |s| { table: t[:name], **s } }
  end
end

# The composite-type content a fixture should decode to (name-sorted, normalized fields).
def expected_types(fx)
  (fx[:types] || []).sort_by { |t| t[:name] }.map do |t|
    { name: t[:name],
      fields: t[:fields].map do |f|
        { name: f[:name], type: f[:type], not_null: f[:not_null] || false,
          precision: f[:precision], scale: f[:scale], varchar_len: f[:varchar_len],
          temporal: f[:temporal] }
      end }
  end
end

# The logical content a fixture should decode to (torn fixtures decode to the
# underlying pk_table content via the valid slot).
def expected_tables(fx)
  fx[:tables].sort_by { |t| t[:name] }.map do |t|
    { name: t[:name],
      columns: t[:columns].map do |c|
        { name: c[:name], type: c[:type], pk: c[:pk], not_null: c[:not_null],
          precision: c[:precision], scale: c[:scale], varchar_len: c[:varchar_len],
          default: c[:default], default_expr: c[:default_expr], identity: c[:identity],
          collation: c[:collation], temporal: c[:temporal] }
      end,
      pk: pk_order(t),
      checks: (t[:checks] || []).map { |ck| { name: ck[:name], expr: ck[:expr], host_deps: ck[:host_deps] || [] } },
      indexes: (t[:indexes] || []).map do |ix|
        if ix[:kind] == "gist"
          { name: ix[:name], cols: ix[:cols], timezone_deps: ix[:timezone_deps], orders: nil,
            entries: gist_leaf_keys_sorted(t, ix).map { |k| k.unpack1("H*") } }
        else
          ent = ix[:kind] == "gin" ? gin_index_entries(t, ix) : index_entries(t, ix)
          # The decoded key options (nil when the index writes none — every element ASC NULLS LAST).
          orders = index_orders(ix).any? { |o| o != [false, false] } ? index_orders(ix).map { |o| ORDER_SYMBOLS.key(o) } : nil
          { name: ix[:name], cols: ix[:cols], timezone_deps: ix[:timezone_deps], orders: orders,
            entries: ent.map { |key, _| key.unpack1("H*") } }
        end
      end,
      rows: table_entries(t).map { |_key, row| row } }
  end
end

# --- LZ4 byte vectors (lz4.md §4) --------------------------------------------
#
# Pins `input -> exact compressed bytes` for the lz4.md §2 encoder. Generated into
# lz4_vectors.toml; every core's codec test must reproduce each `compressed` byte-for-byte
# and decode it back to `input`. Inputs chosen to cover each encoder/format branch.
LZ4_VECTORS = [
  { name: "empty", input: "".b },                                  # -> the lone 0x00 token
  { name: "one_byte", input: "a".b },
  { name: "twelve_below_mflimit", input: "abcdefghijkl".b },       # 12 B: literals only
  { name: "thirteen_run", input: ("a" * 13).b },                   # smallest matchable input
  { name: "run_32", input: ("a" * 32).b },                         # overlapping match (offset 1)
  { name: "pattern_abc", input: ("abc" * 40).b },                  # multi-byte period
  { name: "long_literals_then_run", input: (filler_text(40) + ("z" * 60)).b }, # lit-extension + match
  { name: "long_run_extension", input: ("y" * 1000).b },           # match-length 255-extensions
  { name: "incompressible_64", input: filler_bytes(64) },          # high entropy: expands
  { name: "mixed_text", input: ("the quick brown fox jumps over the lazy dog " * 6).b },
  { name: "trailing_tail", input: (("b" * 20) + "QRSTUVWX").b }    # run ending inside the 12-byte tail
].freeze

def lz4_vectors_path = File.join(__dir__, "lz4_vectors.toml")

def lz4_vectors_toml
  out = +"# LZ4 block codec byte vectors — GENERATED by `ruby spec/fileformat/verify.rb --generate`\n"
  out << "# (the reference encoder in lz4.rb; the algorithm is pinned in lz4.md §2). Each core's\n"
  out << "# codec must reproduce `compressed_hex` from `input_hex` byte-for-byte, and decode it\n"
  out << "# back. Do not edit by hand.\n\nschema_version = 1\n"
  LZ4_VECTORS.each do |v|
    comp = LZ4.compress(v[:input])
    out << "\n[[vector]]\n"
    out << "name = \"#{v[:name]}\"\n"
    out << "input_hex = \"#{v[:input].unpack1('H*')}\"\n"
    out << "compressed_hex = \"#{comp.unpack1('H*')}\"\n"
  end
  out
end

# The encrypted variant's shared vectors (spec/fileformat/encrypted.md §3, §5, §8): XAES-256-GCM
# (the two C2SP vectors and one with associated data shaped like a page's), the HKDF subkeys, the
# host-key KEK, the goldens' key block, and the seeded key-entropy stream. The Argon2id vector is
# RFC 9106's reference-implementation output, checked by the cores (this reference has no Argon2id).
def encryption_vectors_path = File.join(__dir__, "encryption_vectors.toml")

def hex(bytes) = bytes.unpack1("H*")

def encryption_vectors_toml
  out = +"# Encrypted-variant vectors — GENERATED by `ruby spec/fileformat/verify.rb --generate` from\n"
  out << "# the reference cryptography in xaes.rb (spec/fileformat/encrypted.md). Every core reproduces\n"
  out << "# each one byte-for-byte. Do not edit by hand.\n\nschema_version = 1\n"
  nonce = "ABCDEFGHIJKLMNOPQRSTUVWX".b
  page_nonce = (0...24).map { |i| (0x60 + i).chr }.join.b
  [["c2sp_1", ("\x01" * 32).b, nonce, "XAES-256-GCM".b, "".b],
   ["c2sp_2", ("\x03" * 32).b, nonce, "XAES-256-GCM".b, "c2sp.org/XAES-256-GCM".b],
   ["page_ad", (0...32).map(&:chr).join.b, page_nonce, ("jed page " * 7).b, ad_prefix(1, 9)]].each do |name, key, n, pt, ad|
    out << "\n[[xaes]]\nname = \"#{name}\"\nkey_hex = \"#{hex(key)}\"\nnonce_hex = \"#{hex(n)}\"\n"
    out << "plaintext_hex = \"#{hex(pt)}\"\naad_hex = \"#{hex(ad)}\"\n"
    out << "sealed_hex = \"#{hex(Xaes.seal(key, n, pt, ad))}\"\n"
  end
  dek = (0...32).map(&:chr).join.b
  out << "\n[subkeys]\ndek_hex = \"#{hex(dek)}\"\npage_key_hex = \"#{hex(Xaes.page_key(dek))}\"\n"
  out << "meta_key_hex = \"#{hex(Xaes.meta_key(dek))}\"\n"
  salt = (0x40...0x60).map(&:chr).join.b
  out << "\n[host_kek]\nkey_hex = \"#{hex(ENC_KEY)}\"\nkdf_salt_hex = \"#{hex(salt)}\"\n"
  out << "kek_hex = \"#{hex(Xaes.host_kek(ENC_KEY, salt))}\"\n"
  block, golden_dek = new_key_block(SeededStream.new(ENC_SEED))
  out << "\n# The goldens' key block: the seeded stream's first draws under the host key above.\n"
  out << "[key_block]\nseed = #{ENC_SEED}\nblock_hex = \"#{hex(block)}\"\ndek_hex = \"#{hex(golden_dek)}\"\n"
  rewrap_key = ("\x5a" * 32).b
  rewrap = wrap_key_block(golden_dek, 2, rewrap_key, SeededStream.new(ENC_SEED + 1))
  out << "\n# A credential change of that block to a second host key: the same DEK at generation 2, the\n"
  out << "# salt and nonce drawn from a stream seeded with seed + 1 (encrypted.md §5).\n"
  out << "[rewrap]\nseed = #{ENC_SEED + 1}\nkey_hex = \"#{hex(rewrap_key)}\"\nblock_hex = \"#{hex(rewrap)}\"\n"
  out << "\n[argon2id]\npassword = \"password\"\nsalt = \"somesalt\"\nmemory_kib = 65536\niterations = 2\n"
  out << "parallelism = 1\nkey_hex = \"09316115d5cf24ed5a15a31a3ba326e5cf32edc24702987c02b6566f61913cf7\"\n"
  out
end

def verify_encryption_vectors
  fail!("encryption_vectors.toml: missing (run `ruby spec/fileformat/verify.rb --generate`)") unless File.exist?(encryption_vectors_path)
  fail!("XAES-256-GCM C2SP vector failed") unless Xaes.seal(("\x01" * 32).b, "ABCDEFGHIJKLMNOPQRSTUVWX".b, "XAES-256-GCM".b, "".b) ==
                                                ["ce546ef63c9cc60765923609b33a9a1974e96e52daf2fcf7075e2271"].pack("H*")
  unless File.read(encryption_vectors_path) == encryption_vectors_toml
    fail!("encryption_vectors.toml: differs from the reference cryptography (regenerate or fix it)")
  end
end

def verify_lz4_vectors
  fail!("lz4_vectors.toml: missing (run `ruby spec/fileformat/verify.rb --generate`)") unless File.exist?(lz4_vectors_path)
  unless File.read(lz4_vectors_path) == lz4_vectors_toml
    fail!("lz4_vectors.toml: differs from the reference encoder (regenerate or fix the codec)")
  end

  LZ4_VECTORS.each do |v|
    comp = LZ4.compress(v[:input])
    round = LZ4.decompress(comp, v[:input].bytesize)
    fail!("lz4 vector #{v[:name]}: decompress(compress(x)) != x") unless round == v[:input]
  end
end

# --- driver -----------------------------------------------------------------

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

# Deep equality for comparing a decoded fixture's logical content against the expected content.
# Identical to Ruby's `==` EXCEPT it treats two NaNs as equal: a stored NaN decodes to a Float NaN
# (Ruby's NaN == NaN is false), so the float fixtures would otherwise read as a false mismatch even
# though the BYTE check already pinned the on-disk bytes. -0.0 falls through to `==` (the byte check
# distinguishes -0 from +0). Floats are the only values Ruby's structural `==` gets wrong here.
def content_equal?(a, b)
  case a
  when Float
    return false unless b.is_a?(Float)
    return true if a.nan? && b.nan?
    return false if a.nan? || b.nan?

    a == b
  when Array
    b.is_a?(Array) && a.length == b.length && a.each_index.all? { |i| content_equal?(a[i], b[i]) }
  when Hash
    b.is_a?(Hash) && a.length == b.length && a.all? { |k, v| b.key?(k) && content_equal?(v, b[k]) }
  else
    a == b
  end
end

def fixtures_dir = File.join(__dir__, "fixtures")

def generate
  dir = fixtures_dir
  Dir.mkdir(dir) unless Dir.exist?(dir)
  FIXTURES.each do |fx|
    File.binwrite(File.join(dir, fx[:file]), fixture_image(fx))
    puts "wrote #{fx[:file]} (#{fixture_image(fx).bytesize} bytes)"
  end
  File.write(lz4_vectors_path, lz4_vectors_toml)
  File.write(encryption_vectors_path, encryption_vectors_toml)
  puts "wrote encryption_vectors.toml"
  puts "wrote lz4_vectors.toml (#{LZ4_VECTORS.size} vectors)"
  puts "Generated #{FIXTURES.size} fixtures in #{dir}"
end

def verify
  fail!("CRC32 self-test failed") unless crc32("123456789") == 0xCBF43926
  fail!("CRC64 self-test failed") unless crc64("123456789") == 0x6C40DF5F0B497347

  FIXTURES.each do |fx|
    path = File.join(fixtures_dir, fx[:file])
    fail!("#{fx[:file]}: missing (run `ruby spec/fileformat/verify.rb --generate`)") unless File.exist?(path)

    on_disk = File.binread(path)
    reference = fixture_image(fx)
    unless on_disk == reference
      fail!("#{fx[:file]}: bytes differ from the reference encoder " \
            "(disk #{on_disk.bytesize}B vs reference #{reference.bytesize}B)")
    end

    if fx[:reject]
      begin
        decode_image(on_disk)
      rescue RuntimeError
        next
      end
      fail!("#{fx[:file]}: accepted a malformed image")
    end
    next if fx[:malformed] # a semantic lie this decoder does not model: the bytes are the check
    if fx[:expected_open_error]
      # Completeness selects the newer root, then semantic free-list validation
      # rejects it; falling back here would conceal a different corruption class.
      fail!("#{fx[:file]}: manifest did not select new root") unless select_meta(on_disk, fx[:page_size])[:txid] == 2
      begin
        decode_image(on_disk)
      rescue RuntimeError => error
        fail!("#{fx[:file]}: wrong decode error: #{error.message}") unless error.message == fx[:expected_open_error]
      else
        fail!("#{fx[:file]}: accepted malformed logical contents")
      end
      next
    end
    decoded = decode_image(on_disk)
    if fx[:expected_txid] && select_meta(on_disk, fx[:page_size])[:txid] != fx[:expected_txid]
      fail!("#{fx[:file]}: recovery selected the wrong generation")
    end
    want = expected_tables(fx)
    fail!("#{fx[:file]}: decoded #{decoded[:tables].size} tables, expected #{want.size}") unless decoded[:tables].size == want.size
    decoded[:tables].each_with_index do |t, i|
      unless content_equal?(t, want[i])
        fail!("#{fx[:file]}: table #{i} mismatch\n  got:  #{t.inspect}\n  want: #{want[i].inspect}")
      end
    end
    want_types = expected_types(fx)
    fail!("#{fx[:file]}: decoded #{decoded[:types].size} types, expected #{want_types.size}") unless decoded[:types].size == want_types.size
    decoded[:types].each_with_index do |t, i|
      unless content_equal?(t, want_types[i])
        fail!("#{fx[:file]}: type #{i} mismatch\n  got:  #{t.inspect}\n  want: #{want_types[i].inspect}")
      end
    end
    want_seqs = expected_sequences(fx)
    fail!("#{fx[:file]}: decoded #{decoded[:sequences].size} sequences, expected #{want_seqs.size}") unless decoded[:sequences].size == want_seqs.size
    decoded[:sequences].each_with_index do |s, i|
      unless content_equal?(s, want_seqs[i])
        fail!("#{fx[:file]}: sequence #{i} mismatch\n  got:  #{s.inspect}\n  want: #{want_seqs[i].inspect}")
      end
    end
    want_colls = expected_collations(fx)
    fail!("#{fx[:file]}: decoded #{decoded[:collations].size} collations, expected #{want_colls.size}") unless decoded[:collations].size == want_colls.size
    decoded[:collations].each_with_index do |c, i|
      unless content_equal?(c, want_colls[i])
        fail!("#{fx[:file]}: collation #{i} mismatch\n  got:  #{c.inspect}\n  want: #{want_colls[i].inspect}")
      end
    end
    want_views = expected_views(fx)
    unless content_equal?(decoded[:views], want_views)
      fail!("#{fx[:file]}: views mismatch\n  got:  #{decoded[:views].inspect}\n  want: #{want_views.inspect}")
    end
    want_statistics = expected_statistics(fx)
    unless content_equal?(decoded[:statistics], want_statistics)
      fail!("#{fx[:file]}: statistics mismatch\n  got:  #{decoded[:statistics].inspect}\n  want: #{want_statistics.inspect}")
    end
  end

  verify_lz4_vectors
  verify_encryption_vectors
  puts "OK: #{FIXTURES.size} file-format fixtures verified (byte-exact + independent decode); " \
       "#{LZ4_VECTORS.size} LZ4 vectors and the encryption vectors verified"
end

if ARGV.include?("--generate")
  generate
elsif ARGV.first == "--check-image"
  check_images(ARGV.drop(1))
else
  verify
end
