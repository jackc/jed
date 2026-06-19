#!/usr/bin/env ruby
# frozen_string_literal: true

# Independent reference implementation of the on-disk file format (format.md).
# It encodes the golden fixtures from a declarative description and decodes them
# back, so the goldens in fixtures/ are pinned by a THIRD implementation, not just
# self-certified by the two cores that also read/write them (CLAUDE.md S8). Pure
# Ruby, ASCII-only, no gem dependency; test-time only (CLAUDE.md S5).
#
#   ruby spec/fileformat/verify.rb              # verify fixtures/ match the reference
#   ruby spec/fileformat/verify.rb --generate   # (re)write fixtures/ from the reference
#   (or: rake verify)
#
# Exit 0 = all fixtures conform; nonzero = mismatch (prints the offending case).

MAGIC = "JEDB".b
VERSION = 14 # format_version 14: the serial OWNED-sequence link — the sequence-entry flags byte
# gains bit2 has_owner; when set, the flags byte is followed by the owner reference (owner table name
# u16-len + bytes, then owner column ordinal u16). A non-owned sequence writes nothing after the flags
# byte (the v12 shape). The link records the OWNED BY relationship a serial column establishes, so a
# reopened database still auto-drops the owned sequence on DROP TABLE (spec/design/sequences.md §12).
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

WIDTH = { "i16" => 2, "i32" => 4, "i64" => 8, "timestamp" => 8, "timestamptz" => 8,
          "date" => 4 }.freeze
TYPECODE = { "i16" => 1, "i32" => 2, "i64" => 3, "text" => 4, "boolean" => 5, "decimal" => 6,
             "bytea" => 7, "uuid" => 8, "timestamp" => 9, "timestamptz" => 10, "interval" => 11,
             "f64" => 12, "f32" => 13, "date" => 16 }.freeze
CODETYPE = TYPECODE.invert.freeze

# An array (T[]) column type is the element type's string with a trailing "[]" (spec/design/array.md
# §2 — structural, no catalog object). `array_elem("i32[]")` => "i32"; nil for a non-array type.
def array_elem(type) = type.is_a?(String) && type.end_with?("[]") ? type[0...-2] : nil

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
def col(name, type, pk: false, not_null: nil, precision: nil, scale: nil, default: :none, default_expr: nil)
  { name: name, type: type, pk: pk, not_null: not_null.nil? ? pk : not_null,
    precision: precision, scale: scale, default: default, default_expr: default_expr }
end

# A composite-type field (format.md *Composite-type entry*, v9): a name + type (a scalar string,
# or a composite type NAME for a nested composite) + NOT NULL flag + decimal typmod.
def field(name, type, not_null: false, precision: nil, scale: nil)
  { name: name, type: type, not_null: not_null, precision: precision, scale: scale }
end

# A composite (row) type definition (`CREATE TYPE name AS (...)`): a name + ordered field list.
def ctype(name, fields) = { name: name, fields: fields }

# A sequence definition (`CREATE SEQUENCE name ...`, spec/design/sequences.md §3). The six i64
# fields + the cycle/is_called flags exactly as the cores persist them; `last_value` defaults to
# `start` (a fresh sequence). `owned_by` is `[table, column_ordinal]` for a `serial` column's OWNED
# sequence (§12, v13), else `nil`. The on-disk entry is fixed-width through the flags byte, with the
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
# `nil` is a NULL field. The cores build this via
#   CREATE TYPE addr AS (street text NOT NULL, zip i32)
#   CREATE TABLE t (id i32 PRIMARY KEY, home addr)
#   INSERT (1, ROW('Main', 90210)); INSERT (2, ROW('Oak', NULL))
COMPOSITE_TYPE_TABLE = {
  types: [ctype("addr", [field("street", "text", not_null: true), field("zip", "i32")])],
  tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("home", "addr")],
             rows: [[1, ["Main", 90210]], [2, ["Oak", nil]]] }]
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
    { name: "price_range", expr: "price >= 0.50 AND price <= 9999.99" },
    { name: "t_b_check", expr: "b > 0" },
    { name: "t_note_check", expr: "note = 'ok' OR note = 'a''b'" }
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
# mantissa, bits 0x7FEFFFFFFFFFFFFF). The PK is an i32 (float is not allowed in a key this slice
# — a float PRIMARY KEY traps 0A000). Values are the exact f64 the cores compute from the literals.
FLOAT64_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("d", "f64")],
  rows: [[1, 1.5], [2, -2.5], [3, 0.0], [4, -0.0], [5, Float::INFINITY],
         [6, -Float::INFINITY], [7, Float::NAN], [8, nil], [9, Float::MAX]]
}.freeze

# A table with a f32 column (type code 13): the 4-byte IEEE branch. Same special-value coverage
# as FLOAT64_TABLE (+0/-0 distinct on disk, ±Infinity, a canonicalized NaN → 0x7FC00000, NULL) plus
# 100.25 (exactly representable in binary32, bits 0x42C88000). Values are exactly representable in
# binary32 so the f64 fixture value equals the f32-widened decode. PK is i32 (no float key).
FLOAT32_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("r", "f32")],
  rows: [[1, 1.5], [2, -2.5], [3, 0.0], [4, -0.0], [5, Float::INFINITY],
         [6, -Float::INFINITY], [7, Float::NAN], [8, nil], [9, 100.25]]
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
    { name: "t_v_key", cols: [1], unique: true },
    { name: "uq", cols: [2], unique: true },
    { name: "wv", cols: [2, 1], unique: true }
  ],
  rows: [[1, 10, 100], [2, nil, 200], [3, nil, 300]]
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
ARRAY_TABLE = {
  name: "t",
  columns: [col("id", "i32", pk: true), col("xs", "i32[]"), col("tags", "text[]")],
  rows: [[1, [10, 20, 30], %w[a b]],
         [2, [40, 50], []],
         [3, [1, nil, 3], nil],
         [4, { dims: [2, 2], lbounds: [1, 1], elements: [10, 20, 30, 40] },
          { dims: [2], lbounds: [2], elements: %w[x y] }]]
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

# FOREIGN KEY constraints (v11 — spec/design/constraints.md §6): pins the catalog foreign-key list
# (fk_count + per FK: name, local ordinals, ref table, ref ordinals, actions byte). Two tables —
# parent `p` (a PK + two UNIQUE constraints, the FK targets) and child `c` carrying four FKs that
# cover every shape: `c_code_fk` (named, references the UNIQUE column code), `c_mgr_fkey` (a
# self-reference to c's own PK), `c_pid_fkey` (auto-named, references the PK), and `c_x_y_fkey`
# (auto-named, COMPOSITE — references the two-column UNIQUE (a,b) — with ON DELETE RESTRICT, the lone
# non-zero actions byte). FKs are emitted in ascending lowercased-name order; an FK owns no B-tree.
# The cores build this via
#   CREATE TABLE p (pid i32 PRIMARY KEY, code i32 UNIQUE, a i32, b i32, UNIQUE (a, b))
#   INSERT INTO p VALUES (1, 100, 10, 20), (2, 200, 30, 40)
#   CREATE TABLE c (id i32 PRIMARY KEY, pid i32, pcode i32, x i32, y i32, mgr i32,
#     FOREIGN KEY (pid) REFERENCES p (pid),
#     CONSTRAINT c_code_fk FOREIGN KEY (pcode) REFERENCES p (code),
#     FOREIGN KEY (x, y) REFERENCES p (a, b) ON DELETE RESTRICT,
#     FOREIGN KEY (mgr) REFERENCES c (id))
#   INSERT INTO c VALUES (10, 1, 100, 10, 20, NULL), (11, 2, 200, 30, 40, 10)
FK_TABLE = {
  tables: [
    { name: "p",
      columns: [col("pid", "i32", pk: true), col("code", "i32"),
                col("a", "i32"), col("b", "i32")],
      indexes: [
        { name: "p_a_b_key", cols: [2, 3], unique: true },
        { name: "p_code_key", cols: [1], unique: true }
      ],
      rows: [[1, 100, 10, 20], [2, 200, 30, 40]] },
    { name: "c",
      columns: [col("id", "i32", pk: true), col("pid", "i32"), col("pcode", "i32"),
                col("x", "i32"), col("y", "i32"), col("mgr", "i32")],
      fks: [
        { name: "c_code_fk", local: [2], ref_table: "p", ref: [1] },
        { name: "c_mgr_fkey", local: [5], ref_table: "c", ref: [0] },
        { name: "c_pid_fkey", local: [1], ref_table: "p", ref: [0] },
        { name: "c_x_y_fkey", local: [3, 4], ref_table: "p", ref: [2, 3], actions: 1 }
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

# SERIAL (v13 — spec/design/sequences.md §12): pins the OWNED-sequence link (the has_owner flag bit
# + the owner table-name/column-ordinal tail). The `serial` column `id` desugars to an i32 column
# that is NOT NULL (via the PK) with an EXPRESSION DEFAULT `nextval ( 't_id_seq' )` (flags bit3), and
# an OWNED sequence `t_id_seq` (owned_by ["t", 0]) created alongside. One INSERT advances the sequence
# once (is_called true, last_value 1). The cores build this via
#   CREATE TABLE t (id serial PRIMARY KEY, v text); INSERT INTO t (v) VALUES ('hello')
SERIAL_TABLE = {
  sequences: [
    seq("t_id_seq", increment: 1, min_value: 1, max_value: 9_223_372_036_854_775_807, start: 1,
        cache: 1, cycle: false, last_value: 1, is_called: true, owned_by: ["t", 0])
  ],
  tables: [{ name: "t",
             columns: [col("id", "i32", pk: true, default_expr: "nextval ( 't_id_seq' )"),
                       col("v", "text")],
             rows: [[1, "hello"]] }]
}.freeze

FIXTURES = [
  { file: "empty_db.jed",        page_size: 256, tables: [] },
  { file: "overflow_table.jed",  page_size: 256, tables: [OVERFLOW_TABLE] },
  { file: "compressed_table.jed", page_size: 256, tables: [COMPRESSED_TABLE] },
  { file: "one_table_empty.jed", page_size: 256,
    tables: [{ name: "t", columns: [col("id", "i32", pk: true), col("v", "i16")], rows: [] }] },
  { file: "pk_table.jed",        page_size: 256, tables: [PK_TABLE] },
  { file: "text_table.jed",      page_size: 256, tables: [TEXT_TABLE] },
  { file: "bool_table.jed",      page_size: 256, tables: [BOOL_TABLE] },
  { file: "bool_pk_table.jed",   page_size: 256, tables: [BOOL_PK_TABLE] },
  { file: "decimal_table.jed",   page_size: 256, tables: [DECIMAL_TABLE] },
  { file: "bytea_table.jed",     page_size: 256, tables: [BYTEA_TABLE] },
  { file: "uuid_table.jed",      page_size: 256, tables: [UUID_TABLE] },
  { file: "default_table.jed",   page_size: 256, tables: [DEFAULT_TABLE] },
  { file: "default_expr_table.jed", page_size: 256, tables: [DEFAULT_EXPR_TABLE] },
  { file: "timestamp_table.jed",   page_size: 256, tables: [TIMESTAMP_TABLE] },
  { file: "timestamptz_table.jed", page_size: 256, tables: [TIMESTAMPTZ_TABLE] },
  { file: "interval_table.jed",    page_size: 256, tables: [INTERVAL_TABLE] },
  { file: "float64_table.jed",     page_size: 256, tables: [FLOAT64_TABLE] },
  { file: "float32_table.jed",     page_size: 256, tables: [FLOAT32_TABLE] },
  { file: "date_table.jed",        page_size: 256, tables: [DATE_TABLE] },
  { file: "nopk_table.jed",      page_size: 256,
    tables: [{ name: "r", columns: [col("a", "i16"), col("b", "i64")],
               rows: [[7, 70], [8, 80], [9, 90]] }] },
  { file: "composite_pk_table.jed", page_size: 256, tables: [COMPOSITE_PK_TABLE] },
  { file: "check_table.jed", page_size: 256, tables: [CHECK_TABLE] },
  { file: "index_table.jed", page_size: 256, tables: [INDEX_TABLE] },
  { file: "unique_table.jed", page_size: 256, tables: [UNIQUE_TABLE] },
  { file: "gin_array_table.jed", page_size: 256, tables: [GIN_ARRAY_TABLE] },
  { file: "fk_table.jed", page_size: 256, tables: FK_TABLE[:tables] },
  { file: "array_table.jed", page_size: 256, tables: [ARRAY_TABLE] },
  { file: "composite_type_table.jed", page_size: 256,
    types: COMPOSITE_TYPE_TABLE[:types], tables: COMPOSITE_TYPE_TABLE[:tables] },
  { file: "array_composite_table.jed", page_size: 256,
    types: ARRAY_COMPOSITE_TABLE[:types], tables: ARRAY_COMPOSITE_TABLE[:tables] },
  { file: "composite_array_field_table.jed", page_size: 256,
    types: COMPOSITE_ARRAY_FIELD_TABLE[:types], tables: COMPOSITE_ARRAY_FIELD_TABLE[:tables] },
  { file: "nested_composite_table.jed", page_size: 256,
    types: NESTED_COMPOSITE_TABLE[:types], tables: NESTED_COMPOSITE_TABLE[:tables] },
  { file: "sequence_table.jed", page_size: 256,
    sequences: SEQUENCE_TABLE[:sequences], tables: SEQUENCE_TABLE[:tables] },
  { file: "serial_table.jed", page_size: 256,
    sequences: SERIAL_TABLE[:sequences], tables: SERIAL_TABLE[:tables] },
  { file: "tall_tree.jed",       page_size: 256, tables: [TALL_TREE] },
  # Torn-write fallback: same image as pk_table, with one meta slot's CRC smashed.
  { file: "torn_meta_slot0.jed", page_size: 256, tables: [PK_TABLE], corrupt_slot: 0 },
  { file: "torn_meta_slot1.jed", page_size: 256, tables: [PK_TABLE], corrupt_slot: 1 }
].freeze

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

# int-be-signflip: add bias 2^(bits-1), emit unsigned big-endian (encoding.md).
def encode_int(width, value)
  bias = 1 << (width * 8 - 1)
  u = value + bias
  Array.new(width) { |i| (u >> (8 * (width - 1 - i))) & 0xFF }.pack("C*")
end

def decode_int(width, bytes)
  bytes.bytes.reduce(0) { |acc, b| (acc << 8) | b } - (1 << (width * 8 - 1))
end

# The BARE order-preserving KEY body for one present (non-NULL) value of `type` — no presence
# tag (callers add it for nullable index slots; a PK member is NOT NULL). uuid is the 16 raw
# bytes (uuid-raw16, §2.7), boolean a single bool-byte (0x00 false / 0x01 true, §2.9), every
# other keyable type the sign-flipped fixed-width int encoding (timestamps reuse the i64 rule).
def key_body(type, v)
  case type
  when "uuid" then uuid_to_bytes(v)
  when "boolean" then (v ? "\x01".b : "\x00".b)
  else encode_int(WIDTH.fetch(type), v)
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
def composite_fields(type) = $ctypes[type.to_s.downcase]

def encode_value(type, v)
  return "\x01".b if v.nil?

  if (elem = array_elem(type))
    # An array value (spec/design/array.md §4): present tag, then the compact body.
    return "\x00".b + encode_array_body(elem, v)
  end

  if (fields = composite_fields(type))
    # A composite value (spec/design/composite.md §4): present tag, then the recursive body.
    return "\x00".b + encode_composite_body(fields, v)
  end

  case type
  when "text", "bytea"
    bytes = v.b
    "\x00".b + u16(bytes.bytesize) + bytes
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

def table_entry_bytes(table, root_data_page, index_roots)
  out = +"".b
  out << u16(table[:name].bytesize) << table[:name].b
  out << u16(table[:columns].size)
  table[:columns].each do |c|
    out << u16(c[:name].bytesize) << c[:name].b
    # A composite column (v9): type_code 14, flags, then the type NAME in the typmod slot
    # (spec/design/composite.md §3). Composite columns carry no default this slice (bits 2/3 = 0).
    if composite_fields(c[:type])
      out << [14].pack("C")
      out << [c[:not_null] ? 0b10 : 0].pack("C")
      out << u16(c[:type].bytesize) << c[:type].b
      next
    end
    # An array column (v10): type_code 15, flags, then the element-type descriptor
    # (spec/design/array.md §3). Array columns carry no default this slice (bits 2/3 = 0).
    if (elem = array_elem(c[:type]))
      out << [15].pack("C")
      out << [c[:not_null] ? 0b10 : 0].pack("C")
      push_array_element_type(out, elem)
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
    out << [flags].pack("C")
    # A decimal column appends its typmod (precision, scale) — only for type_code 6, so
    # non-decimal entries are byte-unchanged. precision 0 = unconstrained numeric.
    out << u16(c[:precision] || 0) << u16(c[:scale] || 0) if c[:type] == "decimal"
    # A column with a constant DEFAULT (flags bit2) appends its pre-evaluated default value via
    # the same value codec rows use — AFTER the typmod, presence-gated. A DEFAULT NULL is one
    # 0x01. An EXPRESSION default (flags bit3, v8) instead appends its expr-text (u16 length +
    # UTF-8) there, the same token rendering a CHECK uses — bit2/bit3 are exclusive.
    if has_default
      out << encode_value(c[:type], c[:default])
    elsif has_default_expr
      out << u16(c[:default_expr].bytesize) << c[:default_expr].b
    end
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
  end
  # Secondary indexes (v5): count, then per index the name, key-column ordinals (index-key
  # order), the v6 flags byte (bit0 unique — indexes.md §8), and the index tree's root
  # page — in ascending lowercased-name order (indexes.md).
  indexes = table[:indexes] || []
  out << u16(indexes.size)
  indexes.each_with_index do |ix, k|
    out << u16(ix[:name].bytesize) << ix[:name].b
    out << u16(ix[:cols].size)
    ix[:cols].each { |i| out << u16(i) }
    out << [ix[:unique] ? 1 : 0].pack("C")
    out << [ix[:kind] == "gin" ? 1 : 0].pack("C") # v13: index_kind byte (0 = btree, 1 = GIN)
    out << u32(index_roots[k])
  end
  # Foreign keys (v11): count, then per FK the name, the local-column ordinals (into THIS table,
  # list order), the referenced table name, the referenced-column ordinals (into the PARENT
  # table, list order), and the actions byte (bits 0-1 on_delete, bits 2-3 on_update; 0 = NO
  # ACTION, 1 = RESTRICT). In ascending lowercased-name order (constraints.md §6.9). An FK owns
  # no B-tree, so no root page.
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
  out << u32(root_data_page)
  out
end

# Serialize a composite-type catalog entry's BODY (after the entry_kind=1 byte), v9: name, field
# count, then per field — name, type code, [type name when code 14 (nested composite)], flags
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
    elsif TYPECODE.key?(f[:type])
      out << [TYPECODE.fetch(f[:type])].pack("C")
    else
      out << [14].pack("C") << u16(f[:type].bytesize) << f[:type].b # nested composite, by name
    end
    out << [f[:not_null] ? 1 : 0].pack("C")
    out << u16(f[:precision] || 0) << u16(f[:scale] || 0) if f[:type] == "decimal"
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
            pk_idxs.map { |pi| key_body(table[:columns][pi][:type], row[pi]) }.join.b
          end
    [key, row]
  end
  pairs.sort_by { |key, _| key } # String#<=> is bytewise == memcmp order
end

# A secondary-index entry key (indexes.md §3): each indexed column as the encoding.md §2.2
# NULLABLE SLOT (0x00 + bare order-preserving bytes when present, the lone 0x01 for NULL —
# always tagged, even for a NOT NULL column), then the row's storage key as the suffix.
def index_entry_key(table, ix, storage_key, row)
  out = +"".b
  ix[:cols].each do |ci|
    v = row[ci]
    if v.nil?
      out << "\x01".b
    else
      out << "\x00".b
      out << key_body(table[:columns][ci][:type], v)
    end
  end
  out << storage_key
  out
end

# An index's (entry_key, record-plan) pairs in entry-key order. An index record has an EMPTY
# payload (key only — format.md "Index trees"), so its plan is trivially inline.
def index_entries(table, ix)
  table_entries(table).map do |storage_key, row|
    key = index_entry_key(table, ix, storage_key, row)
    [key, { key: key, row: [], table: { columns: [] }, forms: [], comps: [], size: 2 + key.bytesize }]
  end.sort_by { |key, _| key }
end

# A GIN index's (entry_key, record-plan) pairs in entry-key order (spec/design/gin.md §4): one
# entry per DISTINCT non-NULL array element — encode(element) ‖ storage_key, with NO presence tag
# (a term is never NULL) and an EMPTY payload. A NULL whole-value array and an empty array yield no
# entries (they appear in no posting list). This slice: a single integer-element array column.
def gin_index_entries(table, ix)
  elem_type = array_elem(table[:columns][ix[:cols][0]][:type])
  raise "a GIN index column must be an array" unless elem_type
  pairs = []
  table_entries(table).each do |storage_key, row|
    av = row[ix[:cols][0]]
    next if av.nil? # a NULL array yields no terms
    elems = av.is_a?(Hash) ? av[:elements] : av
    elems.compact.uniq.each do |e|
      key = (key_body(elem_type, e) + storage_key).b
      pairs << [key, { key: key, row: [], table: { columns: [] }, forms: [], comps: [], size: 2 + key.bytesize }]
    end
  end
  pairs.sort_by { |key, _| key }
end

# --- out-of-line large values (large-values.md §12) -------------------------

def spillable?(type) = %w[text bytea decimal].include?(type)
def record_max(cap) = [(cap - 12) / 2, 0].max # RECORD_MAX = (C-12)/2 (format.md "Why the record cap")

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
  inline = cols.each_with_index.map { |c, i| encode_value(c[:type], row[i]).bytesize }
  forms = Array.new(cols.size, :inline)
  comps = Array.new(cols.size)
  cur = inline.dup
  size = 2 + key.bytesize + inline.sum
  max = record_max(cap)
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
      cur[i] > (forms[i] == :inline_comp ? EXTERNAL_COMP_PTR_LEN : EXTERNAL_PTR_LEN)
  end
  cand.sort_by! { |i| [-cur[i], i] }
  cand.each do |i|
    break if size <= max

    ptr = forms[i] == :inline_comp ? EXTERNAL_COMP_PTR_LEN : EXTERNAL_PTR_LEN
    forms[i] = forms[i] == :inline_comp ? :external_comp : :external
    size += ptr - cur[i]
    cur[i] = ptr
  end
  [forms, comps, size]
end

# Write a payload across a chain of overflow pages (cap-byte slabs, in order), allocating each via
# `alloc` and linking with next_page (0 terminates). Returns the first page index for the pointer.
def write_overflow_chain(payload, cap, alloc, pages)
  n = (payload.bytesize + cap - 1) / cap
  indices = Array.new(n) { alloc.call }
  n.times do |j|
    slab = payload.byteslice(j * cap, cap)
    nxt = j + 1 < n ? indices[j + 1] : 0
    pages[indices[j]] = [PAGE_OVERFLOW, slab.bytesize, slab, nxt]
  end
  indices[0]
end

# Emit one record's on-disk bytes (key_len | key | values), spilling external columns to overflow
# pages drawn via `alloc` (large-values.md §12/§13). `rec` is a plan hash
# { key:, row:, table:, forms:, comps: } from plan_record.
def emit_record(rec, cap, alloc, pages)
  out = +"".b
  out << u16(rec[:key].bytesize) << rec[:key]
  rec[:table][:columns].each_with_index do |c, i|
    case rec[:forms][i]
    when :external
      payload = value_payload(c[:type], rec[:row][i])
      first = write_overflow_chain(payload, cap, alloc, pages)
      out << [TAG_EXTERNAL].pack("C") << u32(first) << u32(payload.bytesize)
    when :inline_comp
      payload = value_payload(c[:type], rec[:row][i])
      comp = rec[:comps][i]
      out << [TAG_INLINE_COMP].pack("C") << u32(payload.bytesize) << u16(comp.bytesize) << comp
    when :external_comp
      payload = value_payload(c[:type], rec[:row][i])
      comp = rec[:comps][i]
      first = write_overflow_chain(comp, cap, alloc, pages) # the chain carries the COMPRESSED block
      out << [TAG_EXTERNAL_COMP].pack("C") << u32(first) << u32(comp.bytesize) << u32(payload.bytesize)
    else
      out << encode_value(c[:type], rec[:row][i])
    end
  end
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

def write_meta(image, ps, slot, page_size, txid, root, page_count)
  off = slot * ps
  image[off, 4] = MAGIC
  image[off + 4, 2] = u16(VERSION)
  image[off + 8, 4] = u32(page_size)
  image[off + 12, 8] = u64(txid)
  image[off + 20, 4] = u32(root)
  image[off + 24, 4] = u32(page_count)
  image[off + 32, 4] = u32(crc32(image[off, 32]))
end

def write_page(image, ps, index, type, item_count, next_page, payload)
  off = index * ps
  image[off, 1] = [type].pack("C")
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

# `recs[i]` is the record PLAN for keys[i] — a hash { key:, row:, table:, ext:, size: } whose
# `size` is the on-disk (post-spill) record size (large-values.md §12). The tree splits on `size`;
# the actual pointer bytes (with allocated overflow pages) are emitted later by serialize_tree.
def node_payload(node)
  s = node[:recs].sum { |r| r[:size] }
  s += 4 * node[:children].size unless node_leaf?(node) # (N+1) child pointers
  s
end

# Split an overflowing node 2-way, promoting one separator: [:split, left, sep_key, sep_rec,
# right]. Split point (format.md "Split point"): right_edge (the just-inserted record / promoted
# separator is the node's last) takes the append rule m = min(m_append, N-2) with m_append =
# largest m in [1,N-1] with leftpayload(m) <= C; anywhere else splits balanced,
# m = min(m_balanced, m_append, N-2) with m_balanced = smallest m with 2*leftpayload(m) >= payload.
# This builder only inserts in ascending key order (build_tree), so right_edge is always true
# here — the balanced arm is implemented so the reference states the whole contract.
def split_node(node, cap, right_edge)
  payload = node_payload(node)
  return [:whole, node] if payload <= cap

  interior = !node_leaf?(node)
  n = node[:keys].size
  best = 1
  balanced = 0
  (1...n).each do |m|
    lp = (interior ? 4 * (m + 1) : 0) + node[:recs][0, m].sum { |r| r[:size] }
    best = m if lp <= cap
    balanced = m if balanced.zero? && 2 * lp >= payload
  end
  best = balanced if !right_edge && balanced.positive? && balanced < best
  m = [best, n - 2].min
  left = { keys: node[:keys][0, m], recs: node[:recs][0, m],
           children: interior ? node[:children][0, m + 1] : [] }
  right = { keys: node[:keys][(m + 1)..], recs: node[:recs][(m + 1)..],
            children: interior ? node[:children][(m + 1)..] : [] }
  [:split, left, node[:keys][m], node[:recs][m], right]
end

# Insert (key, rec) into the subtree, rebuilding (copy-on-write) and splitting up the path.
def tree_insert(node, key, rec, cap)
  i = node[:keys].bsearch_index { |k| k >= key } || node[:keys].size
  raise "duplicate key in fixture" if i < node[:keys].size && node[:keys][i] == key

  if node_leaf?(node)
    return split_node({ keys: node[:keys].dup.insert(i, key),
                        recs: node[:recs].dup.insert(i, rec), children: [] }, cap,
                      i == node[:keys].size)
  end
  res = tree_insert(node[:children][i], key, rec, cap)
  if res[0] == :split
    _, left, sk, sr, right = res
    children = node[:children].dup
    children[i] = left
    children.insert(i + 1, right)
    split_node({ keys: node[:keys].dup.insert(i, sk), recs: node[:recs].dup.insert(i, sr),
                 children: children }, cap, i == node[:keys].size)
  else
    children = node[:children].dup
    children[i] = res[1]
    [:whole, { keys: node[:keys], recs: node[:recs], children: children }]
  end
end

# Build a table's B-tree from its (key, record) pairs in key order. nil for an empty table.
def build_tree(pairs, cap)
  root = nil
  max = record_max(cap)
  pairs.each do |key, rec|
    # rec is a record plan; rec[:size] is the post-spill on-disk size. A record still over
    # RECORD_MAX after externalizing every spillable value is genuinely unsupported (0A000).
    raise "record of #{rec[:size]}B exceeds RECORD_MAX #{max} after spilling" if rec[:size] > max

    if root.nil?
      root = { keys: [key], recs: [rec], children: [] }
      next
    end
    res = tree_insert(root, key, rec, cap)
    root = res[0] == :split ? { keys: [res[2]], recs: [res[3]], children: [res[1], res[4]] } : res[1]
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
  recs = root[:recs].map { |r| emit_record(r, cap, alloc, pages) }
  n = root[:keys].size
  pages[index] = if node_leaf?(root)
                   [PAGE_LEAF, n, recs.join.b, 0]
                 else
                   [PAGE_INTERIOR, n, child_pages.map { |cp| u32(cp) }.join.b + recs.join.b, 0]
                 end
  [index, next_index]
end

# A from-scratch image (format.md "Allocation & incremental commit"): the special case where
# every node is dirty — data B-trees post-order (per table, name order) from page 2, then the
# catalog chain, then both meta slots at txid 1.
def build_image(types, sequences, tables, page_size)
  ps = page_size
  cap = ps - PAGE_HEADER
  # Composite types in scope for the recursive value codec, keyed by lowercased name (§4).
  $ctypes = types.to_h { |t| [t[:name].downcase, t[:fields]] }
  sorted = tables.sort_by { |t| t[:name].downcase }
  sorted_types = types.sort_by { |t| t[:name].downcase }
  sorted_seqs = sequences.sort_by { |s| s[:name].downcase }

  data_pages = {} # index => [page_type, item_count, payload]
  root_data = Array.new(sorted.size, 0)
  index_roots = Array.new(sorted.size) { [] }
  next_index = ROOT_PAGE
  sorted.each_with_index do |t, ti|
    pairs = table_entries(t).map do |key, row|
      forms, comps, size = plan_record(t, key, row, cap)
      [key, { key: key, row: row, table: t, forms: forms, comps: comps, size: size }]
    end
    root_data[ti], next_index = serialize_tree(build_tree(pairs, cap), next_index, cap, data_pages)
    # The table's index trees follow its data tree, in catalog (name) order (format.md
    # "Allocation & incremental commit" / "From-scratch image").
    (t[:indexes] || []).each do |ix|
      entries = ix[:kind] == "gin" ? gin_index_entries(t, ix) : index_entries(t, ix)
      r, next_index = serialize_tree(build_tree(entries, cap), next_index, cap, data_pages)
      index_roots[ti] << r
    end
  end

  # Catalog entries are kind-tagged: composite-type entries (kind 1, name order) first, then
  # sequence entries (kind 2, name order, v12), then table entries (kind 0) — format.md.
  cat_root = next_index
  cat_entries = []
  sorted_types.each { |ct| cat_entries << ("\x01".b + composite_type_entry_bytes(ct)) }
  sorted_seqs.each { |s| cat_entries << ("\x02".b + sequence_entry_bytes(s)) }
  sorted.each_with_index { |t, ti| cat_entries << ("\x00".b + table_entry_bytes(t, root_data[ti], index_roots[ti])) }
  cat_groups = pack(cat_entries.map(&:bytesize), cap)
  page_count = cat_root + cat_groups.size

  image = "\x00".b * (page_count * ps)
  write_meta(image, ps, 0, page_size, TXID, cat_root, page_count)
  write_meta(image, ps, 1, page_size, TXID, cat_root, page_count)

  data_pages.each { |index, (type, count, payload, nxt)| write_page(image, ps, index, type, count, nxt || 0, payload) }

  cat_groups.each_with_index do |group, gi|
    index = cat_root + gi
    nxt = gi + 1 < cat_groups.size ? index + 1 : 0
    payload = group.map { |ei| cat_entries[ei] }.join.b
    write_page(image, ps, index, PAGE_CATALOG, group.size, nxt, payload)
  end

  image
end

# The bytes a fixture should contain (applying any torn-slot corruption).
def fixture_image(fx)
  image = build_image(fx[:types] || [], fx[:sequences] || [], fx[:tables], fx[:page_size])
  if fx[:corrupt_slot]
    off = fx[:corrupt_slot] * fx[:page_size] + 35 # last CRC byte of that slot
    image.setbyte(off, image.getbyte(off) ^ 0xFF)
  end
  image
end

# --- decoding (independent reader) ------------------------------------------

def take(buf, pos, n)
  raise "unexpected end of page data" if pos + n > buf.bytesize

  [buf.byteslice(pos, n), pos + n]
end

def read_meta(image, ps, slot)
  off = slot * ps
  return nil if off + ps > image.bytesize

  m = image.byteslice(off, ps)
  return nil unless m.byteslice(0, 4) == MAGIC
  return nil unless m.byteslice(4, 2).unpack1("n") == VERSION
  return nil unless m.getbyte(6).zero? && m.getbyte(7).zero?
  return nil unless m.byteslice(28, 4) == "\x00\x00\x00\x00".b
  return nil unless crc32(m.byteslice(0, 32)) == m.byteslice(32, 4).unpack1("N")

  { txid: m.byteslice(12, 8).unpack1("Q>"), root_page: m.byteslice(20, 4).unpack1("N") }
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

  p = image.byteslice(off, ps)
  # Verify the per-page checksum (v7) before trusting any header field (format.md *Page header*).
  raise "page checksum mismatch (corrupted page)" unless page_crc(p) == p.byteslice(12, 4).unpack1("N")
  { type: p.getbyte(0), item_count: p.byteslice(4, 4).unpack1("N"),
    next_page: p.byteslice(8, 4).unpack1("N"), payload: p.byteslice(PAGE_HEADER, ps - PAGE_HEADER) }
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
    fields << { name: fname, type: type, not_null: not_null, precision: precision, scale: scale }
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
    # (spec/design/composite.md §3). No default this slice.
    if tc.getbyte(0) == 14
      cflags, pos = take(buf, pos, 1)
      cf = cflags.getbyte(0)
      raise "reserved flag bit0 set (retired primary_key bit — v5)" if (cf & 0b01) != 0

      tnl, pos = take(buf, pos, 2)
      tname, pos = take(buf, pos, tnl.unpack1("n"))
      columns << { name: cname, type: tname, pk: false, not_null: (cf & 0b10) != 0,
                   precision: nil, scale: nil, default: :none, default_expr: nil }
      next
    end
    # An array column (v10, type_code 15): flags, then the element-type descriptor
    # (spec/design/array.md §3). The column type is the element type's string + "[]". No default.
    if tc.getbyte(0) == 15
      aflags, pos = take(buf, pos, 1)
      af = aflags.getbyte(0)
      raise "reserved flag bit0 set (retired primary_key bit — v5)" if (af & 0b01) != 0

      elem_type, pos = read_array_element_type(buf, pos)
      columns << { name: cname, type: "#{elem_type}[]", pk: false, not_null: (af & 0b10) != 0,
                   precision: nil, scale: nil, default: :none, default_expr: nil }
      next
    end
    flags, pos = take(buf, pos, 1)
    f = flags.getbyte(0)
    raise "reserved flag bit0 set (retired primary_key bit — v5)" if (f & 0b01) != 0

    type = CODETYPE.fetch(tc.getbyte(0))
    precision = nil
    scale = nil
    if type == "decimal"
      pb, pos = take(buf, pos, 2)
      sb, pos = take(buf, pos, 2)
      p = pb.unpack1("n")
      precision = p.zero? ? nil : p
      scale = p.zero? ? nil : sb.unpack1("n")
    end
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
    columns << { name: cname, type: type, pk: false, not_null: (f & 0b10) != 0,
                 precision: precision, scale: scale, default: default, default_expr: default_expr }
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
    checks << { name: cname, expr: expr }
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
      cols << ob.unpack1("n")
    end
    fb, pos = take(buf, pos, 1)
    raise "reserved index flag set (only bit0 unique is defined — v6)" if (fb.getbyte(0) & ~0b01) != 0
    kb, pos = take(buf, pos, 1) # v12: index_kind byte (0 = btree, 1 = GIN)
    raise "reserved index kind (only 0=btree, 1=gin defined — v12)" if kb.getbyte(0) > 1
    rb, pos = take(buf, pos, 4)
    indexes << { name: iname, cols: cols, unique: (fb.getbyte(0) & 1) != 0,
                 kind: kb.getbyte(0) == 1 ? "gin" : "btree", root_page: rb.unpack1("N") }
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
    raise "reserved fk action bits set / unsupported action (only NO ACTION/RESTRICT — v11)" \
      if (ab.getbyte(0) & ~0b1111) != 0 || (ab.getbyte(0) & 0b11) > 1 || ((ab.getbyte(0) >> 2) & 0b11) > 1
    fks << { name: fname, local: local, ref_table: rtable, ref: ref, actions: ab.getbyte(0) }
  end
  root, pos = take(buf, pos, 4)
  [{ name: name, columns: columns, pk: pk, checks: checks, indexes: indexes, fks: fks,
     root_data_page: root.unpack1("N") }, pos]
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
    coeff = coeff * 10_000 + gb.unpack1("n")
  end
  [render_decimal((fb.getbyte(0) & 1) != 0, scb.unpack1("n"), coeff), pos]
end

# Reconstruct a value from the P(v) content gathered from its overflow chain (large-values.md §12).
def value_from_payload(type, payload)
  case type
  when "text" then payload.dup.force_encoding("UTF-8")
  when "bytea" then payload.dup.force_encoding("ASCII-8BIT")
  when "decimal" then decode_decimal_body(payload, 0).first
  else raise "a non-spillable type was stored external"
  end
end

# Gather `len` bytes of an external value's payload by following its overflow chain from `first`:
# each page is page_type 4, carries item_count payload bytes, chained via next_page (0 terminates).
def read_overflow_chain(first, len, fetch)
  out = +"".b
  p = first
  while out.bytesize < len
    raise "overflow chain ended before the value length" if p.zero?

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
    raise "external value with no overflow reader" if fetch.nil?

    comp = read_overflow_chain(fpb.unpack1("N"), slb.unpack1("N"), fetch)
    return [value_from_payload(type, LZ4.decompress(comp, rlb.unpack1("N"))), pos]
  end
  raise "invalid value presence tag" unless t.zero?

  if (elem = array_elem(type))
    # An array value body (inverse of encode_array_body): ndim/flags/dims, optional bitmap, elements.
    return decode_array_body(elem, buf, pos)
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

def decode_record(columns, buf, pos, fetch)
  key_len, pos = take(buf, pos, 2)
  _key, pos = take(buf, pos, key_len.unpack1("n"))
  row = []
  columns.each do |c|
    v, pos = decode_value(c[:type], buf, pos, fetch)
    row << v
  end
  [row, pos]
end

# In-order walk of a table's B-tree -> rows in ascending key order (format.md interior layout:
# (N+1) child pointers, then N records). Independent of how the tree was built. An external value's
# chain is followed through `fetch` (large-values.md §12).
def read_tree_rows(image, ps, root_page, columns)
  rows = []
  fetch = ->(p) { read_page(image, ps, p) }
  walk = lambda do |idx|
    return if idx.zero?

    pg = read_page(image, ps, idx)
    case pg[:type]
    when PAGE_LEAF
      pos = 0
      pg[:item_count].times do
        row, pos = decode_record(columns, pg[:payload], pos, fetch)
        rows << row
      end
    when PAGE_INTERIOR
      n = pg[:item_count]
      children = (0..n).map { |i| pg[:payload].byteslice(i * 4, 4).unpack1("N") }
      pos = 4 * (n + 1)
      n.times do |i|
        walk.call(children[i])
        row, pos = decode_record(columns, pg[:payload], pos, fetch)
        rows << row
      end
      walk.call(children[n])
    else
      raise "expected a B-tree node page, got type #{pg[:type]}"
    end
  end
  walk.call(root_page)
  rows
end

# In-order walk of an INDEX B-tree -> entry keys in ascending order. An index record has an
# empty payload, so only the key is read (format.md "Index trees").
def read_tree_keys(image, ps, root_page)
  keys = []
  walk = lambda do |idx|
    return if idx.zero?

    pg = read_page(image, ps, idx)
    read_rec = lambda do |pos|
      klb, pos = take(pg[:payload], pos, 2)
      key, pos = take(pg[:payload], pos, klb.unpack1("n"))
      keys << key
      pos
    end
    case pg[:type]
    when PAGE_LEAF
      pos = 0
      pg[:item_count].times { pos = read_rec.call(pos) }
    when PAGE_INTERIOR
      n = pg[:item_count]
      children = (0..n).map { |i| pg[:payload].byteslice(i * 4, 4).unpack1("N") }
      pos = 4 * (n + 1)
      n.times do |i|
        walk.call(children[i])
        pos = read_rec.call(pos)
      end
      walk.call(children[n])
    else
      raise "expected a B-tree node page, got type #{pg[:type]}"
    end
  end
  walk.call(root_page)
  keys
end

def decode_image(image)
  ps = image.byteslice(8, 4).unpack1("N")
  meta = select_meta(image, ps)
  types = []
  sequences = []
  tables = []
  # Composite types in scope for the recursive value codec; populated as the (types-first) catalog
  # is read, so every composite a table row references is registered before its rows are decoded.
  $ctypes = {}
  cat = meta[:root_page]
  while cat != 0
    pg = read_page(image, ps, cat)
    raise "expected a catalog page" unless pg[:type] == PAGE_CATALOG

    pos = 0
    pg[:item_count].times do
      kb, pos = take(pg[:payload], pos, 1) # entry_kind: 0 table, 1 composite type, 2 sequence (v12)
      kind = kb.getbyte(0)
      if kind == 1
        ct, pos = decode_composite_type_entry(pg[:payload], pos)
        types << ct
        $ctypes[ct[:name].downcase] = ct[:fields]
        next
      end
      if kind == 2
        s, pos = decode_sequence_entry(pg[:payload], pos)
        sequences << s
        next
      end
      raise "unknown catalog entry kind #{kind}" unless kind.zero?

      entry, pos = decode_table_entry(pg[:payload], pos)
      rows = read_tree_rows(image, ps, entry[:root_data_page], entry[:columns])
      indexes = entry[:indexes].map do |ix|
        { name: ix[:name], cols: ix[:cols],
          entries: read_tree_keys(image, ps, ix[:root_page]).map { |k| k.unpack1("H*") } }
      end
      tables << { name: entry[:name], columns: entry[:columns], pk: entry[:pk],
                  checks: entry[:checks], indexes: indexes, rows: rows }
    end
    cat = pg[:next_page]
  end
  { types: types, sequences: sequences, tables: tables }
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
  (fx[:sequences] || []).sort_by { |s| s[:name].downcase }
end

# The composite-type content a fixture should decode to (name-sorted, normalized fields).
def expected_types(fx)
  (fx[:types] || []).sort_by { |t| t[:name].downcase }.map do |t|
    { name: t[:name],
      fields: t[:fields].map do |f|
        { name: f[:name], type: f[:type], not_null: f[:not_null] || false,
          precision: f[:precision], scale: f[:scale] }
      end }
  end
end

# The logical content a fixture should decode to (torn fixtures decode to the
# underlying pk_table content via the valid slot).
def expected_tables(fx)
  fx[:tables].sort_by { |t| t[:name].downcase }.map do |t|
    { name: t[:name],
      columns: t[:columns].map do |c|
        { name: c[:name], type: c[:type], pk: c[:pk], not_null: c[:not_null],
          precision: c[:precision], scale: c[:scale], default: c[:default],
          default_expr: c[:default_expr] }
      end,
      pk: pk_order(t),
      checks: (t[:checks] || []).map { |ck| { name: ck[:name], expr: ck[:expr] } },
      indexes: (t[:indexes] || []).map do |ix|
        ent = ix[:kind] == "gin" ? gin_index_entries(t, ix) : index_entries(t, ix)
        { name: ix[:name], cols: ix[:cols],
          entries: ent.map { |key, _| key.unpack1("H*") } }
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
  puts "wrote lz4_vectors.toml (#{LZ4_VECTORS.size} vectors)"
  puts "Generated #{FIXTURES.size} fixtures in #{dir}"
end

def verify
  fail!("CRC32 self-test failed") unless crc32("123456789") == 0xCBF43926

  FIXTURES.each do |fx|
    path = File.join(fixtures_dir, fx[:file])
    fail!("#{fx[:file]}: missing (run `ruby spec/fileformat/verify.rb --generate`)") unless File.exist?(path)

    on_disk = File.binread(path)
    reference = fixture_image(fx)
    unless on_disk == reference
      fail!("#{fx[:file]}: bytes differ from the reference encoder " \
            "(disk #{on_disk.bytesize}B vs reference #{reference.bytesize}B)")
    end

    decoded = decode_image(on_disk)
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
  end

  verify_lz4_vectors
  puts "OK: #{FIXTURES.size} file-format fixtures verified (byte-exact + independent decode); " \
       "#{LZ4_VECTORS.size} LZ4 vectors verified"
end

if ARGV.include?("--generate")
  generate
else
  verify
end
