# frozen_string_literal: true

require "set"
require "zlib"
require_relative "lz4"
require_relative "page_buffer"
require_relative "persistent_tree"
require_relative "paged_rows"
require_relative "stored_value"
require_relative "crypto"

module JedNative
  # Experimental v49 codec. Adapted from the independently handwritten Ruby
  # format verifier; the verifier remains separate and is never loaded here.
  # Instances hold their own composite catalog and decode reachability state.
  class FileFormat
    include PersistentTrees
    MAGIC = "JEDB".b
    VERSION = 50
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
    
    def valid_page_size?(size)
      size.is_a?(Integer) && size.between?(256, 65_536) && (size & (size - 1)).zero?
    end
    
    def build_image(types, sequences, tables, page_size, collations = [], **options)
      build_pages(types, sequences, tables, page_size, collations, **options).to_s
    end

    def u16(v) = [v].pack("n")
    def length16(bytes)
      raise Error.new('0A000', 'encoded value exceeds its 16-bit storage length') if bytes.bytesize > 0xFFFF
      u16(bytes.bytesize)
    end
    def u32(v) = [v].pack("N")
    def u64(v) = [v].pack("Q>")
    
    CRC64_TABLE = Array.new(256) do |byte|
      value = byte << 56
      8.times do
        high = value & 0x8000000000000000
        value = (value << 1) & 0xFFFFFFFFFFFFFFFF
        value ^= 0x42F0E1EBA9EA3693 unless high.zero?
      end
      value
    end.freeze
    CRC64_HIGH = CRC64_TABLE.map { |value| value >> 32 }.freeze
    CRC64_LOW = CRC64_TABLE.map { |value| value & 0xFFFFFFFF }.freeze

    # The shared format uses standard zlib CRC-32/IEEE. The optional finalized
    # state continues a previous span, including across the skipped checksum field.
    def crc32(data, crc = 0) = Zlib.crc32(data, crc)
    
    # The per-page checksum (v7, format.md *Page header*): CRC-32/IEEE over a body page's bytes
    # EXCLUDING its own 4-byte crc32 field at [12,16) — i.e. [0,12) then [16,ps).
    # Continue the first span's finalized CRC to avoid allocating their concatenation.
    def page_crc(page)
      crc32(page.byteslice(PAGE_HEADER, page.bytesize - PAGE_HEADER), crc32(page.byteslice(0, 12)))
    end
    
    def meta_crc(page)
      crc32(page.byteslice(36, page.bytesize - 36), crc32(page.byteslice(0, 32)))
    end
    
    # CRC-64/ECMA-182: unreflected, init = xorout = 0. A supplied initial state
    # continues a manifest-chain digest without constructing a combined buffer.
    def crc64(data, crc = 0)
      # Two 32-bit halves keep each byte step in immediate Ruby integers rather
      # than allocating arbitrary-precision integers for the 64-bit shift.
      high = crc >> 32
      low = crc & 0xFFFFFFFF
      data.each_byte do |byte|
        index = (high >> 24) ^ byte
        high = ((high << 8) & 0xFFFFFFFF) ^ (low >> 24) ^ CRC64_HIGH[index]
        low = ((low << 8) & 0xFFFFFFFF) ^ CRC64_LOW[index]
      end
      (high << 32) | low
    end
    
    # int-be-signflip: add bias 2^(bits-1), emit unsigned big-endian (encoding.md).
    INTEGER_PACK = { 2 => 'n', 4 => 'N', 8 => 'Q>' }.freeze
    INTEGER_BIAS = { 2 => 1 << 15, 4 => 1 << 31, 8 => 1 << 63 }.freeze
    def encode_int(width, value)
      if value.is_a?(Integer) && (format = INTEGER_PACK[width])
        return [value + INTEGER_BIAS.fetch(width)].pack(format)
      end
      bias = 1 << (width * 8 - 1)
      u = value + bias
      Array.new(width) { |i| (u >> (8 * (width - 1 - i))) & 0xFF }.pack("C*")
    end
    
    def decode_int(width, bytes)
      if (format = INTEGER_PACK[width]) && bytes.bytesize == width
        return bytes.unpack1(format) - INTEGER_BIAS.fetch(width)
      end
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
    def collated_sort_key(coll_name, string)
      raise NotImplementedError, "non-C collation key provider is required" unless @collation_key
      @collation_key.call(coll_name, string)
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
    def initialize(collation_key: nil, expression: nil, crypto: nil)
      @ctypes = {}
      @collation_key = collation_key
      @expression = expression
      @crypto = crypto
    end

    # --- the encrypted variant's layout (spec/fileformat/encrypted.md §1–§2) ----------------------
    #
    # @crypto is the PageCodec of the encrypted database whose pages this codec reads and writes (nil:
    # plaintext). Sealing records each written page's tag in it, and reading a pointer records the tag
    # it binds, so every pointer a writer emits and every page a reader follows is checked against it.
    attr_accessor :crypto

    def encrypted? = !@crypto.nil?

    # A codec for the same database with the same collation and expression hooks.
    def reader(crypto: @crypto)
      FileFormat.new(collation_key: @collation_key, expression: @expression, crypto: crypto)
    end

    # The payload capacity C of a body page at page size ps in this codec's variant.
    def payload_capacity(ps) = ps - PAGE_HEADER - (encrypted? ? Crypto::RESERVE : 0)
    def first_body = encrypted? ? Crypto::FIRST_BODY : ROOT_PAGE

    # A capacity names its variant: C is page_size − 16 in a plaintext page and page_size − 56 in an
    # encrypted one, and page sizes are powers of two, so the two never coincide. The tree and record
    # rules that take only C (split points, record dispositions, chain slabs) read the pointer width
    # from it, so an in-memory tree edit planned for an encrypted database splits as its pages will.
    def cap_tag(cap)
      full = cap + PAGE_HEADER + Crypto::RESERVE
      full >= Crypto::MIN_PAGE_SIZE && (full & (full - 1)).zero? ? Crypto::TAG : 0
    end

    def chain_capacity(cap) = cap - cap_tag(cap)
    def external_ptr_len(cap) = EXTERNAL_PTR_LEN + cap_tag(cap)
    def external_comp_ptr_len(cap) = EXTERNAL_COMP_PTR_LEN + cap_tag(cap)
    def page_size_of(cap) = cap + PAGE_HEADER + (cap_tag(cap).zero? ? 0 : Crypto::RESERVE)

    # The tag a pointer to page binds (empty in a plaintext file; zero for page 0).
    def tag_of(page)
      return "".b unless @crypto
      tag = @crypto.tag(page)
      raise Error.new("XX001", "no tag is known for page #{page}") unless tag
      tag
    end

    # A page pointer in this codec's layout: u32 index, then its tag when encrypted.
    def pointer(page) = u32(page) + tag_of(page)

    # Read the tag that ends a pointer at pos and record it for page.
    def read_pointer_tag(buf, pos, page)
      return pos unless @crypto
      tag, pos = take(buf, pos, Crypto::TAG)
      if page.zero?
        raise "a pointer to no page carries a tag" unless tag == Crypto::ZERO_TAG
      else
        @crypto.note(page, tag)
      end
      pos
    end

    # Record the overflow-chain tag of a leaf value's external pointer (its stored span).
    def note_external(span)
      return unless @crypto
      case span.getbyte(0)
      when TAG_EXTERNAL then @crypto.note(span.byteslice(1, 4).unpack1("N"), span.byteslice(EXTERNAL_PTR_LEN, Crypto::TAG))
      when TAG_EXTERNAL_COMP then @crypto.note(span.byteslice(1, 4).unpack1("N"), span.byteslice(EXTERNAL_COMP_PTR_LEN, Crypto::TAG))
      end
    end

    CHAIN_TYPES = [1, 4, 7].freeze

    # A body page's stored bytes in the encrypted layout (encrypted.md §4): header with a zero CRC,
    # a chain page's successor tag leading the payload, zero fill, sealed under the page key.
    def seal_page(ps, index, type, item_count, next_page, payload, flags: 0)
      lead = CHAIN_TYPES.include?(type) ? tag_of(next_page) : "".b
      if lead.bytesize + payload.bytesize > payload_capacity(ps)
        raise Error.new("0A000", "page payload exceeds page capacity")
      end
      plain = [type, flags, 0, item_count, next_page, 0].pack("CCnNNN") + lead + payload
      @crypto.seal_body(index, plain.ljust(ps - Crypto::RESERVE, "\0"))
    end

    # Emit a body page into a page sink: a descriptor a later write_descriptor writes in a plaintext
    # file, or the sealed bytes at once in an encrypted one — sealing happens in emission order, which
    # is the seal order of encrypted.md §7 (every child before the page that points at it).
    def page_out(pages, ps, index, type, item_count, payload, next_page = 0, flags = 0)
      pages[index] = @crypto ? seal_page(ps, index, type, item_count, next_page, payload, flags: flags) : [type, item_count, payload, next_page, flags]
    end

    def write_descriptor(image, ps, index, descriptor)
      if descriptor.is_a?(String)
        image[index * ps, ps] = descriptor
      else
        type, count, payload, following, flags = descriptor
        write_page(image, ps, index, type, count, following || 0, payload, flags: flags || 0)
      end
    end

    # Sealed catalog pages for layout at ids, last page first (each needs its successor's tag).
    def catalog_out(pages, ps, ids, layout)
      (layout.length - 1).downto(0) do |index|
        page = layout[index]
        page_out(pages, ps, ids[index], PAGE_CATALOG, page[:count], page[:payload], ids[index + 1] || 0, page[:continued] ? 1 : 0)
      end
    end
    
    # The field list of the composite type named `type` (case-insensitive), or nil for a scalar type.
    def composite_fields(type) = @ctypes[type.to_s]
    
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
        out << [14].pack("C") << length16(elem_type) << elem_type.b
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
        name, pos = take_str(buf, pos)
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
      out << length16(table[:name]) << table[:name].b
      out << u16(table[:columns].size)
      table[:columns].each do |c|
        out << length16(c[:name]) << c[:name].b
        # A composite column (v9): type_code 14, flags, then the type NAME in the typmod slot
        # (spec/design/composite.md §3). A composite DEFAULT is always an expression default (v44, flags
        # bit3): its expr-text follows the type name. Bit2 (a constant default) is never set.
        if composite_fields(c[:type])
          out << [14].pack("C")
          cflags = c[:not_null] ? 0b10 : 0
          cflags |= 0b1000 if c[:default_expr]
          out << [cflags].pack("C")
          out << length16(c[:type]) << c[:type].b
          out << length16(c[:default_expr]) << c[:default_expr].b if c[:default_expr]
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
          out << length16(c[:collation]) << c[:collation].b if c[:collation]
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
          out << length16(c[:default_expr]) << c[:default_expr].b
        end
        # The effective collation name (v17, flags bit6) — last in the per-column entry (collation.md §5).
        out << length16(c[:collation]) << c[:collation].b if c[:collation]
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
        out << length16(ck[:name]) << ck[:name].b
        out << length16(ck[:expr]) << ck[:expr].b
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
        out << length16(ix[:name]) << ix[:name].b
        out << u16(ix[:cols].size)
        # Each key element is a column ordinal (u16) OR, for an EXPRESSION key (v26 — indexes.md §6),
        # the 0xFFFF sentinel + u16 length + canonical text. A fixture writes an expression element as
        # a Hash `{ expr: "<canonical text>" }`.
        ix[:cols].each do |c|
          if c.is_a?(Hash)
            out << u16(0xFFFF)
            out << length16(c[:expr]) << c[:expr].b
          else
            out << u16(c)
          end
        end
        # index_flags: bit0 unique (v6), bit1 has_predicate (v27 — a partial index, indexes.md §9),
        # bit2 has_host_deps (v31 — a host-function index dependency, extensibility.md §8.1), bit3
        # has_timezone_deps (v32), bit4 has_key_options (v46), bit5 is_constraint (v50 — constraints.md §5.5).
        host_deps = ix[:host_deps] || []
        timezone_deps = ix[:timezone_deps]
        has_timezone_deps = timezone_deps && (timezone_deps[:dynamic] || !timezone_deps[:zones].empty?)
        orders = index_orders(ix)
        has_key_options = orders.any? { |o| o != [false, false] }
        out << [(ix[:unique] ? 1 : 0) | (ix[:predicate] ? 2 : 0) | (host_deps.empty? ? 0 : 4) | (has_timezone_deps ? 8 : 0) |
                (has_key_options ? 16 : 0) | (ix[:unique] && ix[:constraint] ? 32 : 0)].pack("C")
    
        # v13: index_kind byte (0 = btree, 1 = GIN); v20: 2 = GiST (gist.md §8).
        out << [{ "gin" => 1, "gist" => 2 }.fetch(ix[:kind], 0)].pack("C")
        out << pointer(index_roots[k])
        # v27: a partial index's predicate canonical text (u16 len + UTF-8) after index_root_page.
        out << length16(ix[:predicate]) << ix[:predicate].b if ix[:predicate]
        # v31: the host-function dependency list (extensibility.md §8.1) after the predicate — only when
        # bit2 is set. Already in ascending (name, arg-type codes) order (a fixture writes them sorted).
        write_host_deps(out, host_deps) unless host_deps.empty?
        if has_timezone_deps
          out << [timezone_deps[:dynamic] ? 1 : 0].pack("C") << u16(timezone_deps[:zones].size)
          timezone_deps[:zones].each do |dep|
            out << length16(dep[:name]) << dep[:name].b
            out << length16(dep[:version]) << dep[:version].b
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
        out << length16(fk[:name]) << fk[:name].b
        out << u16(fk[:local].size)
        fk[:local].each { |i| out << u16(i) }
        out << length16(fk[:ref_table]) << fk[:ref_table].b
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
        out << length16(ex[:name]) << ex[:name].b
        out << length16(ex[:index]) << ex[:index].b
        out << u16(ex[:elements].size)
        ex[:elements].each do |col, op|
          out << u16(col) << [{ "&&" => 0, "=" => 1 }.fetch(op)].pack("C")
        end
      end
      out << pointer(root_data_page)
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
        summary << length16(table[:name]) << table[:name].b << u16(s[:column])
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
          item << length16(table[:name]) << table[:name].b << u16(s[:column])
          item << u16(ordinal) << u32(frequency) << length16(encoded) << encoded
          out << item
        end
        s[:histogram].each_with_index do |value, ordinal|
          encoded = encode_value(col[:type], value)
          item = +"\x04\x02".b
          item << length16(table[:name]) << table[:name].b << u16(s[:column])
          item << u16(ordinal) << length16(encoded) << encoded
          out << item
        end
        (terms ? terms[:entries] : []).each_with_index do |(term, rows), ordinal|
          item = +"\x04\x03".b
          item << length16(table[:name]) << table[:name].b << u16(s[:column])
          item << u16(ordinal) << [rows].pack("q>") << length16(term) << term.b
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
      out << length16(ct[:name]) << ct[:name].b
      out << u16(ct[:fields].size)
      ct[:fields].each do |f|
        out << length16(f[:name]) << f[:name].b
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
          out << [14].pack("C") << length16(f[:type]) << f[:type].b # nested composite, by name
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
      out << length16(s[:name]) << s[:name].b
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
        out << length16(o[0]) << o[0].b
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
      str = ->(x) { length16(x) + x.b }
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
    
    # Serialize a collation reference entry's BODY (after its entry_kind = 3 byte, v18): a flags byte
    # (bit0 is_default), then the metadata — name + unicode_version + cldr_version + description, each
    # u16-len + UTF-8. NO table: it is vendored into the binary and resolved by name on open
    # (spec/design/collation.md §2/§5/§9, format.md *Collation entry*).
    def collation_entry_bytes(c)
      out = +"".b
      out << [c[:default] ? 0b1 : 0].pack("C")
      [c[:name], c[:unicode], c[:cldr], c[:desc]].each { |s| out << length16(s) << s.b }
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
      # Committed rows already carry their encoded storage key in tree order.
      # Keep this a cursor so an index build need not retain a second complete
      # inventory of row tokens (and their demand-loaded values).
      return table[:rows].each_entry if defined?(PagedRows) && table[:rows].is_a?(PagedRows)

      pk_idxs = pk_order(table)
      pairs = table[:rows].each_with_index.map do |row, i|
        key = if pk_idxs.empty?
                table[:storage_keys] ? table[:storage_keys].fetch(i) : encode_int(8, i)
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
        v = if ci.is_a?(Hash)
              raise NotImplementedError, "expression index needs an evaluator" unless ci[:evaluate]
              ci[:evaluate].call(row)
            else
              row[ci]
            end
        if v.nil?
          out << [null].pack("C")
        else
          out << [present].pack("C")
          # A fixture may force a non-collation ("C" / byte-order) key encoding for an index over a
          # COLLATE-annotated column via `key_collation: "C"`, to author a version-SKEWED index whose
          # stored order is deliberately WRONG for the loaded collation (the skew read-safety regression
          # fixture, spec/design/collation.md §12/§14). Absent the override, the index keys use the
          # column's frozen collation, as every other fixture does.
          col = ci.is_a?(Hash) ? ci : table[:columns][ci]
          coll = col[:collation]
          coll = nil if coll == "C"
          body = key_body(col[:type], v, coll)
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
      if ix[:predicate] && !ix[:where]
        raise NotImplementedError, "partial index needs a predicate evaluator"
      end
      ix[:where].nil? || ix[:where].call(row)
    end
    
    # An index's (entry_key, record-plan) pairs in entry-key order. An index record has an EMPTY
    # payload (key only — format.md "Index trees"), so its plan is trivially inline. `forged_empty: true`
    # authors an index whose tree is empty although the table has rows: the stand-in for an index a newer
    # jed maintained under text this reference cannot evaluate (unparsable_definitions.jed). A reader that
    # wrongly trusts it loses rows.
    def index_entries(table, ix)
      empty = [].freeze
      shape = { columns: empty }.freeze
      table_entries(table).filter_map do |storage_key, row|
        next unless index_admits?(ix, row)
        key = index_entry_key(table, ix, storage_key, row)
        [key, { key: key, row: empty, table: shape, forms: empty, comps: empty, size: key.bytesize }]
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
    
    # Convex hull under the subtype's canonical key order. Infinite bounds and
    # inclusive ties retain the same range semantics for every supported subtype.
    def gist_range_union(a, b, elem)
      return b if a == :empty
      return a if b == :empty
      if a[:lower].nil? || b[:lower].nil?
        lower, lower_inc = nil, false
      else
        comparison = key_body(elem, a[:lower]) <=> key_body(elem, b[:lower])
        lower = comparison <= 0 ? a[:lower] : b[:lower]
        lower_inc = comparison.zero? ? a[:lower_inc] || b[:lower_inc] : (comparison < 0 ? a[:lower_inc] : b[:lower_inc])
      end
      if a[:upper].nil? || b[:upper].nil?
        upper, upper_inc = nil, false
      else
        comparison = key_body(elem, a[:upper]) <=> key_body(elem, b[:upper])
        upper = comparison >= 0 ? a[:upper] : b[:upper]
        upper_inc = comparison.zero? ? a[:upper_inc] || b[:upper_inc] : (comparison > 0 ? a[:upper_inc] : b[:upper_inc])
      end
      { lower: lower, upper: upper, lower_inc: lower_inc, upper_inc: upper_inc }
    end

    # union of two COMPONENT bounds: the convex hull for a range; componentwise [min(min), max(max)]
    # (byte-wise, the order-preserving key order) for a scalar.
    def gist_comp_union(op, a, b)
      return gist_range_union(a, b, op[:elem]) if op[:kind] == :range
    
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
      return bound if bound.is_a?(String)
      out = +"".b
      ops.each_index do |k|
        op = ops[k]
        if op[:kind] == :range
          out << encode_range_body(op[:elem], bound[k])
        else
          min, max = bound[k]
          out << length16(min) << min << length16(max) << max
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
      node[:min_skey] ||= if node[:leaf]
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
      # Child sorting needs each subtree's minimum key. Only this insertion
      # path changes; walking every other subtree on every insert is quadratic.
      node.delete(:min_skey)
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
          payload << length16(b) << b << length16(skey) << skey
        end
        index = next_index
        next_index += 1
        page_out(pages, @gist_page_size, index, PAGE_GIST_LEAF, node[:entries].size, payload)
        [index, next_index]
      else
        child_indices = node[:children].map do |_, c|
          ci, next_index = gist_serialize(ops, c, next_index, pages)
          ci
        end
        payload = +"".b
        node[:children].each_with_index do |(u, _), i|
          b = gist_encode_bound(ops, u)
          payload << length16(b) << b << pointer(child_indices[i])
        end
        index = next_index
        next_index += 1
        page_out(pages, @gist_page_size, index, PAGE_GIST_INTERIOR, node[:children].size, payload)
        [index, next_index]
      end
    end
    
    # Build + serialize a GiST index's R-tree from a table's rows; returns [root_index, next_index].
    # One leaf entry per row with NO NULL indexed column (any NULL → the row is not indexed, gist.md
    # §4.1/§7). An empty index serializes nothing and returns root 0 (the empty-index convention).
    def serialize_gist_index(table, ix, next_index, pages)
    
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
    
    # Walk a persisted GiST R-tree (page types 5/6), returning its leaf keys (bound ‖ skey) — the
    # inverse of serialize_gist_index for the round-trip read.
    def read_gist_keys(image, ps, root_page)
      keys = []
      seen = Set.new
      walk = lambda do |idx|
        return if idx.zero?
    
        raise "GiST page reached twice" unless seen.add?(idx)
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
            cp, pos = take(pg[:payload], pos, 4)
            children << cp.unpack1("N")
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
    
    def spillable?(type) = fixed_width(type).nil?
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
      v = v.resolve if v.is_a?(StoredValue)
      case type
      when "text", "bytea", "json" then v.b
      when "decimal" then encode_decimal(v) # the body (no presence tag)
      else encode_value(type, v).byteslice(1..)
      end
    end
    
    # Decide each column's disposition for a record: [forms, comps, on-disk record size], where
    # forms[i] is :inline / :inline_comp / :external / :external_comp and comps[i] holds the LZ4
    # block or a length-bearing deferred block for a compressed form. Spill only when forced
    # (record > RECORD_MAX); then COMPRESS the
    # largest eligible values first (store-smaller rule), then EXTERNALIZE the largest remaining —
    # both passes ties-by-column-index. Deterministic, mirrors the cores' plan_dispositions
    # (large-values.md §12/§13; format.md "Large values").
    def plan_record(table, key, row, cap, compression_units: nil)
      cols = table[:columns]
      # Each column's inline-plain contribution to record_size (the v24 basis — format.md "Record"):
      # a fixed-width column always its width (NULL occupies a zero-filled slot); a variable column 0
      # when NULL (a zero-length span) else its tagged inline encoding.
      inline = cols.each_with_index.map do |c, i|
        if (w = fixed_width(c[:type]))
          w
        else
          value = row[i]
          value.is_a?(StoredValue) ? value.plain_size : value.nil? ? 0 : encode_value(c[:type], value).bytesize
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
        value = row[i]
        spillable?(cols[i][:type]) && !value.nil? &&
          (value.is_a?(StoredValue) ? value.payload_length : value_payload(cols[i][:type], value).bytesize) >= S_COMPRESS
      end
      cand.sort_by! { |i| [-inline[i], i] }
      cand.each do |i|
        break if size <= max
    
        value = row[i]
        if value.is_a?(StoredValue)
          compression_units&.call((value.payload_length + cap - 1) / cap)
          next if value.compression_outcome == :rejected
          comp = value.compressed? ? value.compressed_block : LZ4.compress(value.planning_payload)
        else
          payload = value_payload(cols[i][:type], value)
          compression_units&.call((payload.bytesize + cap - 1) / cap)
          comp = LZ4.compress(payload)
        end
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
          cur[i] > (forms[i] == :inline_comp ? external_comp_ptr_len(cap) : external_ptr_len(cap))
      end
      cand.sort_by! { |i| [-cur[i], i] }
      cand.each do |i|
        break if size <= max
    
        ptr = forms[i] == :inline_comp ? external_comp_ptr_len(cap) : external_ptr_len(cap)
        forms[i] = forms[i] == :inline_comp ? :external_comp : :external
        size += ptr - cur[i]
        cur[i] = ptr
      end
      # RECORD_MAX controls disposition, not admission. A wide fixed-width row
      # cannot shrink to that target, but can still occupy a legal single-row
      # leaf. Include the real column directories and bitmaps in this check.
      raise Error.new('0A000', 'record exceeds page capacity after spilling') if size + leaf_overhead(1, cols) > cap

      [forms, comps, size]
    end

    def compression_units(table, key, row, cap)
      units = 0
      plan_record(table, key, row, cap, compression_units: ->(count) { units += count })
      units
    end
    
    # Write a payload across a chain of overflow pages (cap-byte slabs, in order), allocating each via
    # `alloc` and linking with next_page (0 terminates). Returns the first page index for the pointer.
    def write_overflow_chain(payload, cap, alloc, pages)
      slab_size = chain_capacity(cap)
      n = (payload.bytesize + slab_size - 1) / slab_size
      indices = Array.new(n) { alloc.call }
      # Last page first: an encrypted chain page binds its successor's tag (encrypted.md §7).
      (n - 1).downto(0) do |j|
        slab = payload.byteslice(j * slab_size, slab_size)
        nxt = j + 1 < n ? indices[j + 1] : 0
        page_out(pages, page_size_of(cap), indices[j], PAGE_OVERFLOW, slab.bytesize, slab, nxt)
      end
      indices[0]
    end
    
    # Emit one record's on-disk bytes (key_len | key | values), spilling external columns to overflow
    # pages drawn via `alloc` (large-values.md §12/§13). `rec` is a plan hash
    # { key:, row:, table:, forms:, comps: } from plan_record.
    # One value's on-disk body given its disposition (the value codec, unchanged across row-major and PAX):
    # inline-plain is encode_value; the large-value forms carry a pointer / inline block and allocate
    # overflow chains via `alloc` (large-values.md §12/§13). `comp` is the LZ4 block for a compressed form.
    def emit_value(c, v, form, comp, cap, alloc, pages, retain: nil)
      if v.is_a?(StoredValue)
        if v.form == form
          return v.stored_bytes unless v.external?
          if retain
            retain.call(v)
            return v.stored_bytes
          end
          first = write_overflow_chain(v.stored_payload, cap, alloc, pages)
          bytes = v.stored_bytes.dup
          bytes[1, 4] = u32(first)
          bytes[form == :external ? EXTERNAL_PTR_LEN : EXTERNAL_COMP_PTR_LEN, Crypto::TAG] = tag_of(first) if @crypto
          return bytes
        end
        v = v.resolve
      end
      comp = comp.to_s if comp.is_a?(StoredValue::CompressedBlock)
      case form
      when :external
        payload = value_payload(c[:type], v)
        first = write_overflow_chain(payload, cap, alloc, pages)
        [TAG_EXTERNAL].pack("C") + u32(first) + u32(payload.bytesize) + tag_of(first)
      when :inline_comp
        payload = value_payload(c[:type], v)
        [TAG_INLINE_COMP].pack("C") + u32(payload.bytesize) + length16(comp) + comp
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
      child_pages.each { |cp| out << pointer(cp) }
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
    def emit_leaf_pax(recs, cap, alloc, pages, retain: nil)
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
              emit_value(c, v, rec[:forms][ci], rec[:comps][ci], cap, alloc, pages, retain: retain)
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
    def write_meta(image, ps, slot, page_size, txid, root, page_count, live = page_count - first_body, seal: true)
      off = slot * ps
      image[off, ps] = "\x00".b * ps
      image[off, 4] = MAGIC
      image[off + 4, 2] = u16(VERSION)
      image[off + 6, 2] = [Crypto::SUITE, Crypto::RESERVE].pack("CC") if @crypto
      image[off + 8, 4] = u32(page_size)
      image[off + 12, 8] = u64(txid)
      image[off + 20, 4] = u32(root)
      image[off + 24, 4] = u32(page_count)
      image[off + 56, 4] = u32(live) # v34: the exact live page count
      image[off + 64, Crypto::TAG] = tag_of(root) if @crypto
      if seal
        finish_meta(image, ps, slot)
      else
        image[off + 32, 4] = u32(meta_crc(image.byteslice(off, ps)))
      end
    end

    # The inline manifest's offset and capacity in a meta page (encrypted.md §6).
    def manifest_offset = encrypted? ? 96 : 64
    def inline_manifest_capacity(ps) = encrypted? ? (ps - 96 - Crypto::RESERVE) / 12 : (ps - 64) / 12

    # A commit's resolution digest (spec/pageservice/README.md §3.5): the CRC-64 of its meta page, with
    # the CRC field and the reserve zeroed in the encrypted variant, so the meta a client sent and the
    # one its owner resealed for the slot it wrote resolve alike (encrypted.md §14).
    def resolution_digest(meta)
      return crc64(meta) if meta.getbyte(6).to_i.zero?

      zeroed = meta.dup
      zeroed[32, 4] = "\0" * 4
      zeroed[zeroed.bytesize - Crypto::RESERVE, Crypto::RESERVE] = "\0" * Crypto::RESERVE
      crc64(zeroed)
    end

    # Record the free-list head (with its tag) in meta slot `slot`.
    def write_meta_free_head(image, ps, slot, head)
      image[slot * ps + 28, 4] = u32(head)
      image[slot * ps + 80, Crypto::TAG] = tag_of(head) if @crypto
    end

    # Close meta slot `slot` once every field is written: its CRC, or in an encrypted file its seal
    # over the manifest-overflow pages (id, stored bytes) in chain order, then its CRC.
    def finish_meta(image, ps, slot, manifest = [])
      off = slot * ps
      if @crypto
        bytes = image.byteslice(off, ps).dup
        image[off, ps] = @crypto.seal_meta(slot, bytes, manifest)
      else
        image[off + 32, 4] = u32(meta_crc(image.byteslice(off, ps)))
      end
    end
    
    def write_page(image, ps, index, type, item_count, next_page, payload, flags: 0)
      return image[index * ps, ps] = seal_page(ps, index, type, item_count, next_page, payload, flags: flags) if @crypto
      raise Error.new('0A000', 'page payload exceeds page capacity') if payload.bytesize > ps - PAGE_HEADER
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
    def node_payload(node, cap = nil)
      if node_leaf?(node)
        node[:recs].sum { |r| r[:size] } + leaf_overhead(node[:keys].size, leaf_cols(node))
      else
        ptr = 4 + (cap ? cap_tag(cap) : 0)
        (ptr + 4) * node[:keys].size + ptr + node[:keys].sum(&:bytesize)
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
      payload = node_payload(node, cap)
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
              ptr = 4 + cap_tag(cap)
              leftp = ->(mm) { (ptr + 4) * mm + ptr + prefix[mm] }
              rightp = ->(mm) { (ptr + 4) * (n - 1 - mm) + ptr + (prefix[n] - prefix[mm + 1]) }
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
      pairs.each do |key, rec|
        # The packing target may be smaller than a legal single-record leaf,
        # notably for wide rows and secondary keys larger than half a page.
        if rec[:size] + leaf_overhead(1, rec[:table][:columns]) > cap
          raise Error.new('0A000', 'record exceeds page capacity after spilling')
        end
    
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
        page_out(pages, page_size_of(cap), index, PAGE_LEAF, n, emit_leaf_pax(root[:recs], cap, alloc, pages)) # column-major (v24)
      else
        page_out(pages, page_size_of(cap), index, PAGE_INTERIOR, n, emit_interior(root[:keys], child_pages)) # record-free (v24)
      end
      [index, next_index]
    end
    
    # A from-scratch image (format.md "Allocation & incremental commit"): the special case where
    # every node is dirty — data B-trees post-order (per table, name order) from page 2, then the
    # catalog chain, then both meta slots at txid 1.
    def build_pages(types, sequences, tables, page_size, collations = [], start_page: nil, views: [], txid: TXID)
      raise ArgumentError, "invalid page size" unless valid_page_size?(page_size)
      raise Error.new("0A000", "an encrypted database needs a page size of at least 1024") if @crypto && page_size < Crypto::MIN_PAGE_SIZE
      ps = page_size
      start_page ||= first_body
      cap = payload_capacity(ps)
      @gist_page_size = ps
      # Composite types in scope for the recursive value codec, keyed by lowercased name (§4).
      @ctypes = types.to_h { |t| [t[:name], t[:fields]] }
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
        tree = t.key?(:physical_tree) ? t[:physical_tree] : build_tree(pairs, cap)
        root_data[ti], next_index = serialize_tree(tree, next_index, cap, data_pages)
        # The table's index trees follow its data tree, in catalog (name) order (format.md
        # "Allocation & incremental commit" / "From-scratch image").
        (t[:indexes] || []).each do |ix|
          r, next_index =
            if ix[:kind] == "gist"
              # The on-disk form is the R-tree (pages 5/6), not the flat leaf B-tree (gist.md §4.1).
              if ix.key?(:physical_gist)
                ix[:physical_gist] ? gist_serialize(gist_opclasses_for(t, ix), ix[:physical_gist], next_index, data_pages) : [0, next_index]
              else
                serialize_gist_index(t, ix, next_index, data_pages)
              end
            else
              tree = if ix.key?(:physical_tree)
                ix[:physical_tree]
              else
                entries = ix[:kind] == "gin" ? gin_index_entries(t, ix) : index_entries(t, ix)
                build_tree(entries, cap)
              end
              serialize_tree(tree, next_index, cap, data_pages)
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
      cat_layout = catalog_layout(cat_entries, chain_capacity(cap))
      page_count = cat_root + cat_layout.size
      catalog_out(data_pages, ps, (cat_root...page_count).to_a, cat_layout)

      image = PageBuffer.new(ps, page_count)
      if @crypto
        block = @crypto.key_block_page(ps)
        image[2 * ps, ps] = block
        image[3 * ps, ps] = block
      end
      data_pages.each { |index, descriptor| write_descriptor(image, ps, index, descriptor) }
      write_meta(image, ps, 0, page_size, txid, cat_root, page_count, page_count - start_page)
      write_meta(image, ps, 1, page_size, txid, cat_root, page_count, page_count - start_page)
      image
    end
    
    # The bytes a fixture should contain (applying any torn-slot corruption).
    
    def write_host_deps(out, deps)
      out << u16(deps.size)
      deps.each do |dep|
        out << length16(dep[:name]) << dep[:name].b
        out << u16(dep[:arg_types].size)
        dep[:arg_types].each { |tc| out << [tc].pack("C") }
        out << [dep[:result]].pack("C")
        out << length16(dep[:component_id]) << dep[:component_id].b
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
      raise "negative byte extent" if pos.negative? || n.negative?
      raise "unexpected end of page data" if pos + n > buf.bytesize
    
      [buf.byteslice(pos, n), pos + n]
    end
    
    # Read a u16-length-prefixed UTF-8 string (the catalog's name/string encoding).
    def take_str(buf, pos)
      nl, pos = take(buf, pos, 2)
      s, pos = take(buf, pos, nl.unpack1("n"))
      [s.force_encoding("UTF-8"), pos]
    end
    
    def read_meta(image, ps, slot, manifest: true, validate_body: true)
      off = slot * ps
      return nil if off + ps > image.bytesize
    
      m = image.byteslice(off, ps)
      return nil unless m.byteslice(0, 4) == MAGIC
      return nil unless m.byteslice(4, 2).unpack1("n") == VERSION
      return nil unless [m.getbyte(6), m.getbyte(7)] == (@crypto ? [Crypto::SUITE, Crypto::RESERVE] : [0, 0])
      return nil unless m.byteslice(8, 4).unpack1("N") == ps
      return nil unless meta_crc(m) == m.byteslice(32, 4).unpack1("N")
    
      meta = { txid: m.byteslice(12, 8).unpack1("Q>"), root_page: m.byteslice(20, 4).unpack1("N"),
               page_count: m.byteslice(24, 4).unpack1("N"), free_head: m.byteslice(28, 4).unpack1("N"),
               live: m.byteslice(56, 4).unpack1("N") }
      first = first_body
      return nil unless meta[:page_count] >= first + 1 && meta[:page_count] <= image.bytesize / ps
      return nil unless (first...meta[:page_count]).cover?(meta[:root_page])
      return nil unless meta[:live] <= meta[:page_count] - first
      return nil unless meta[:free_head].zero? || (first...meta[:page_count]).cover?(meta[:free_head])
      if @crypto
        meta[:root_tag] = m.byteslice(64, Crypto::TAG)
        meta[:free_tag] = m.byteslice(80, Crypto::TAG)
        return nil if meta[:free_head].zero? && meta[:free_tag] != Crypto::ZERO_TAG
      end
      meta[:slot_index] = slot
      if manifest
        return nil unless valid_manifest?(image, ps, m, meta, validate_body: validate_body)
      else
        meta[:protected] = []
      end
    
      meta
    end
    
    def valid_manifest?(image, ps, m, meta, validate_body: true)
      head, count, chain_count = m.byteslice(36, 12).unpack("N3")
      digest = m.byteslice(48, 8).unpack1("Q>")
      first = first_body
      return false unless m.byteslice(60, 4) == "\x00".b * 4
      return false if count + chain_count > meta[:page_count] - first
      return false unless (chain_count.zero? && head.zero? && digest.zero?) ||
                          (chain_count.positive? && head >= first && head < meta[:page_count])
      return false if count.zero? && (!chain_count.zero? || !head.zero? || !digest.zero?)

      base = manifest_offset
      limit = ps - (@crypto ? Crypto::RESERVE : 0)
      inline = [count, inline_manifest_capacity(ps)].min
      refs = m.byteslice(base, inline * 12).unpack("NQ>" * inline).each_slice(2).to_a
      return false unless m.byteslice(base + inline * 12, limit - base - inline * 12).bytes.all?(&:zero?)
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
        next true unless validate_body
        block = image.byteslice(index * ps, ps)
        # An encrypted body page is ciphertext: its manifest identity alone checks it (encrypted.md §6).
        next crc64(block) == expected if @crypto
        (1..7).cover?(block.getbyte(0)) && page_crc(block) == block.byteslice(12, 4).unpack1("N") &&
          crc64(block) == expected
      end
      meta[:protected] = ids + chain
      meta[:manifest] = refs
      meta[:manifest_pages] = chain
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
        reached, @reached = @reached, nil
        pg = read_page(image, ps, page)
        @reached = reached
        raise "invalid free-list page" unless pg[:type] == 7
        entries = pg[:item_count]
        raise "invalid free-list count" if entries * 4 > pg[:payload].bytesize
        free_pages.concat(pg[:payload].byteslice(0, entries * 4).unpack("N*"))
        seen << page
        page = pg[:next_page]
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
      selected = a && b ? (b[:txid] > a[:txid] ? b : a) : a || b
      raise "no valid meta page" unless selected

      authenticate_meta(image, ps, selected)
    end

    # The selected meta of an encrypted file must authenticate under the held key — never a fallback
    # to the other slot (encrypted.md §6) — and its root and free-list tags become the pointers' tags.
    def authenticate_meta(image, ps, meta, slots: [meta[:slot_index]])
      return meta unless @crypto
      stored = image.byteslice(meta[:slot_index] * ps, ps)
      manifest = (meta[:manifest_pages] || []).map { |page| [page, image.byteslice(page * ps, ps)] }
      unless slots.any? { |slot| @crypto.meta_authentic?(slot, stored, manifest) }
        raise Error.new("XX001", "meta page fails authentication")
      end
      @crypto.note(meta[:root_page], meta[:root_tag])
      @crypto.note(meta[:free_head], meta[:free_tag])
      meta
    end
    
    def read_page(image, ps, index)
      off = index * ps
      raise "page index out of range" if off + ps > image.bytesize
      return read_sealed_page(image.byteslice(off, ps), ps, index) if @crypto

      p = image.byteslice(off, ps)
      # Verify the per-page checksum (v7) before trusting any header field (format.md *Page header*).
      raise "page checksum mismatch (corrupted page)" unless page_crc(p) == p.byteslice(12, 4).unpack1("N")
      @reached << index if @reached
      { type: p.getbyte(0), flags: p.getbyte(1), item_count: p.byteslice(4, 4).unpack1("N"),
        next_page: p.byteslice(8, 4).unpack1("N"), payload: p.byteslice(PAGE_HEADER, ps - PAGE_HEADER) }
    end
    
    # An encrypted body page, checked against the tag its pointer bound and opened, returned in the
    # plaintext page's shape: a chain page without its successor's tag, an interior's and a GiST
    # interior's child pointers without theirs — each tag recorded as the pointer's (encrypted.md §2).
    def read_sealed_page(stored, ps, index)
      raise "page index below the first body page" if index < Crypto::FIRST_BODY
      plain = @crypto.open_body(index, stored, @crypto.tag(index))
      type, flags, reserved, item_count, next_page = plain.unpack("CCnNN")
      raise "reserved page header bytes are nonzero" unless reserved.zero?
      payload = plain.byteslice(PAGE_HEADER, ps - PAGE_HEADER - Crypto::RESERVE)
      if CHAIN_TYPES.include?(type)
        payload = payload.byteslice(read_pointer_tag(payload, 0, next_page)..)
      elsif type == PAGE_INTERIOR
        pos = 0
        children = Array.new(item_count + 1) do
          child, pos = take(payload, pos, 4)
          child = child.unpack1("N")
          pos = read_pointer_tag(payload, pos, child)
          child
        end
        payload = children.pack("N*") + payload.byteslice(pos..)
      elsif type == PAGE_GIST_INTERIOR
        pos = 0
        out = +"".b
        item_count.times do
          bl, pos = take(payload, pos, 2)
          bound, pos = take(payload, pos, bl.unpack1("n"))
          child, pos = take(payload, pos, 4)
          pos = read_pointer_tag(payload, pos, child.unpack1("N"))
          out << bl << bound << child
        end
        payload = out + payload.byteslice(pos..)
      end
      @reached << index if @reached
      { type: type, flags: flags, item_count: item_count, next_page: next_page, payload: payload }
    end

    # Decode a composite-type catalog entry's body (inverse of composite_type_entry_bytes); the caller
    # has consumed the entry_kind byte. A nested-composite field stores the referenced type's NAME.
    def decode_composite_type_entry(buf, pos)
      name, pos = take_str(buf, pos)
      fc, pos = take(buf, pos, 2)
      fields = []
      fc.unpack1("n").times do
        fname, pos = take_str(buf, pos)
        tcb, pos = take(buf, pos, 1)
        code = tcb.getbyte(0)
        precision = nil
        scale = nil
        if code == 14
          tname, pos = take_str(buf, pos)
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
      name, pos = take_str(buf, pos)
      col_count, pos = take(buf, pos, 2)
      columns = []
      col_count.unpack1("n").times do
        cname, pos = take_str(buf, pos)
        tc, pos = take(buf, pos, 1)
        # A composite column (v9, type_code 14): flags, then the type NAME in the typmod slot
        # (spec/design/composite.md §3), then (v44, flags bit3) the expression DEFAULT's text. Only
        # bit1 (not_null) and bit3 (default_is_expr) may be set.
        if tc.getbyte(0) == 14
          cflags, pos = take(buf, pos, 1)
          cf = cflags.getbyte(0)
          raise "reserved flag bit0 set (retired primary_key bit — v5)" if (cf & 0b01) != 0
          raise "invalid composite column flags (v44)" if (cf & ~0b1010 & 0xff) != 0
    
          tname, pos = take_str(buf, pos)
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
        cname, pos = take_str(buf, pos)
        expr, pos = take_str(buf, pos)
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
        iname, pos = take_str(buf, pos)
        kc, pos = take(buf, pos, 2)
        cols = []
        kc.unpack1("n").times do
          ob, pos = take(buf, pos, 2)
          ord = ob.unpack1("n")
          if ord == 0xFFFF
            # An expression key element (v26): the sentinel + u16 length + canonical text.
            et, pos = take_str(buf, pos)
            cols << { expr: et }
          else
            cols << ord
          end
        end
        fb, pos = take(buf, pos, 1)
        # bit0 unique (v6), bit1 has_predicate (v27), bit2 has_host_deps (v31 — extensibility.md §8.1).
        raise "reserved index flag set" if (fb.getbyte(0) & ~0b11_1111) != 0
        # bit5 is_constraint (v50): only a unique index backs a UNIQUE constraint (constraints.md §5.5).
        raise "a constraint index must be unique" if (fb.getbyte(0) & 0b10_0000) != 0 && (fb.getbyte(0) & 1).zero?
        kb, pos = take(buf, pos, 1) # v13: index_kind byte (0 = btree, 1 = GIN); v20: 2 = GiST
        raise "reserved index kind (only 0=btree, 1=gin, 2=gist defined — v20)" if kb.getbyte(0) > 2
        has_predicate = (fb.getbyte(0) & 0b10) != 0
        # v45: a GiST index may be partial (the backing index of EXCLUDE … WHERE, gist.md §7.1).
        raise "a gin index cannot be partial (v27)" if has_predicate && kb.getbyte(0) == 1
        has_host_deps = (fb.getbyte(0) & 0b100) != 0
        raise "a non-btree index cannot have host-function dependencies (v31)" if has_host_deps && kb.getbyte(0) != 0 && !(kb.getbyte(0) == 2 && has_predicate)
        rb, pos = take(buf, pos, 4)
        index_root = rb.unpack1("N")
        pos = read_pointer_tag(buf, pos, index_root)
        # v27: the partial-index predicate canonical text follows index_root_page when bit1 is set.
        predicate = nil
        if has_predicate
          predicate, pos = take_str(buf, pos)
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
        indexes << { name: iname, cols: cols, unique: (fb.getbyte(0) & 1) != 0, constraint: (fb.getbyte(0) & 0b10_0000) != 0, orders: orders,
                     kind: { 1 => "gin", 2 => "gist" }.fetch(kb.getbyte(0), "btree"),
                     root_page: index_root, predicate: predicate, host_deps: host_deps, timezone_deps: timezone_deps }
      end
      # Foreign keys (v11): name + local ordinals + referenced table + referenced ordinals + the
      # actions byte, in name order. An FK owns no B-tree (no root page).
      fks = []
      fc, pos = take(buf, pos, 2)
      fc.unpack1("n").times do
        fname, pos = take_str(buf, pos)
        lc, pos = take(buf, pos, 2)
        local = []
        lc.unpack1("n").times do
          ob, pos = take(buf, pos, 2)
          local << ob.unpack1("n")
        end
        rtable, pos = take_str(buf, pos)
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
        ename, pos = take_str(buf, pos)
        iname, pos = take_str(buf, pos)
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
      root, pos = take(buf, pos, 4)
      pos = read_pointer_tag(buf, pos, root.unpack1("N"))
      count_raw, pos = take(buf, pos, 8)
      row_count = count_raw.unpack1("q>")
      raise "negative table row_count" if row_count.negative?
      raise "root_data_page/row_count invariant violated" unless root.unpack1("N").zero? == row_count.zero?
    
      [{ name: name, columns: columns, pk: pk, checks: checks, indexes: indexes, fks: fks,
         exclusions: exclusions, root_data_page: root.unpack1("N"), row_count: row_count }, pos]
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
      raise "decimal flags have reserved bits" unless (fb.getbyte(0) & ~1).zero?

      [render_decimal((fb.getbyte(0) & 1) != 0, scb.unpack1("n"), coeff), pos]
    end
    
    # Reconstruct a value from the P(v) content gathered from its overflow chain (large-values.md §12).
    def decode_utf8(bytes)
      value = bytes.dup.force_encoding('UTF-8')
      raise Error.new('XX001', 'stored text is not valid UTF-8') unless value.valid_encoding?
      value
    end

    def value_from_payload(type, payload)
      case type
      when "text" then decode_utf8(payload)
      when "bytea" then payload.dup.force_encoding("ASCII-8BIT")
      when "json" then decode_utf8(payload)
      when "jsonb" then decode_jsonb_body(payload, 0).first
      when "decimal" then decode_decimal_body(payload, 0).first
      else decode_value(type, "\x00".b + payload, 0).first
      end
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
        raise "overflow page #{p} is referenced twice" if @claimed && !@claimed.add?(p)
    
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
        pos = read_pointer_tag(buf, pos, fpb.unpack1("N"))
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
        pos = read_pointer_tag(buf, pos, fpb.unpack1("N"))
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
        [decode_utf8(sb), pos]
      when "boolean"
        bb, pos = take(buf, pos, 1)
        raise "invalid boolean value" unless bb.getbyte(0) <= 1

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
        [decode_utf8(sb), pos]
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
      raise "PAX leaf directory overruns page" if n > payload.bytesize / 4
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
      raise "PAX leaf column extent invalid" unless col_start.each_cons(2).all? { |a, b| a <= b } && col_start.last <= payload.bytesize
    
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
          raise "PAX value directory not ascending" unless ends.each_cons(2).all? { |a, b| a <= b }
          body = start + 1 + 4 * n
          raise "variable region extent mismatch" unless body + (ends.last || 0) == col_start[ci + 1]
    
          { ends: ends, body: payload.byteslice(body, (ends.last || 0)) }
        end
      end
      [keys, regions]
    end
    
    # Decode value (record i, column c) from a parsed v24 leaf region: nil from the bitmap (fixed) or
    # the zero-length span (variable). Fixed regions have already had their extent
    # validated by parse_pax_leaf; read their untagged slots directly, without
    # constructing a synthetic tag and running the container/overflow decoder.
    def leaf_value(region, type, i, fetch)
      if region[:width]
        w = region[:width]
        return nil if (region[:bitmap].getbyte(i / 8) & (0x80 >> (i % 8))) != 0
    
        body, offset = region[:body], i * w
        raise "negative byte extent" if offset.negative?
        raise "unexpected end of page data" if offset + w > body.bytesize
        case type
        when "i16" then body.unpack1('n', offset: offset) - (1 << 15)
        when "i32", "date" then body.unpack1('N', offset: offset) - (1 << 31)
        when "i64", "timestamp", "timestamptz" then body.unpack1('Q>', offset: offset) - INTEGER_BIAS[8]
        when "boolean"
          value = body.getbyte(offset)
          raise "invalid boolean value" unless value <= 1
          value == 1
        when "f32" then body.unpack1('g', offset: offset)
        when "f64" then body.unpack1('G', offset: offset)
        when "interval" then body.unpack('l>l>q>', offset: offset)
        when "uuid" then uuid_from_bytes(body.byteslice(offset, w))
        else
          decode_value(type, "\x00".b + body.byteslice(offset, w), 0, fetch).first
        end
      else
        lo = i.zero? ? 0 : region[:ends][i - 1]
        len = region[:ends][i] - lo
        return nil if len.zero?
    
        body = region[:body]
        if (type == "text" || type == "bytea" || type == "json") && body.getbyte(lo) == 0
          # Inline strings have no recursive type work. Keep their value extent
          # check local: a corrupt length must never read the following slot.
          raise "unexpected end of page data" if len < 3
          size = body.unpack1('n', offset: lo + 1)
          raise "unexpected end of page data" if size + 3 > len
          value = body.byteslice(lo + 3, size)
          if type == "bytea"
            value.force_encoding('ASCII-8BIT')
          else
            value.force_encoding('UTF-8')
            raise Error.new('XX001', 'stored text is not valid UTF-8') unless value.valid_encoding?
            value
          end
        else
          decode_value(type, body.byteslice(lo, len), 0, fetch).first
        end
      end
    end
    
    # In-order walk of a table's B+tree -> rows in ascending key order (format.md v24: interior nodes
    # are a record-free routing skeleton; all records live in leaves). Independent of how the tree was
    # built. An external value's chain is followed through `fetch` (large-values.md §12).
    def read_tree_rows(image, ps, root_page, columns, keys_out = nil, profile = nil)
      rows = []
      seen = Set.new
      fetch = ->(p) { read_page(image, ps, p) }
      height = 0
      scan_units = {}
      walk = lambda do |idx, depth|
        return nil if idx.zero?
        height = [height, depth].max
        raise "B-tree page reached twice" unless seen.add?(idx)
    
        pg = read_page(image, ps, idx)
        case pg[:type]
        when PAGE_LEAF
          n = pg[:item_count]
          overflow_pages = []
          leaf_fetch = ->(page) { overflow_pages << page; fetch.call(page) }
          leaf_keys, regions = parse_pax_leaf(pg[:payload], n, columns)
          keys_out.concat(leaf_keys) if keys_out
          (0...n).each do |i|
            if profile
              units = {}
              regions.each_with_index do |region, column|
                next if region[:width]
                lo = i.zero? ? 0 : region[:ends][i - 1]
                length = region[:ends][i] - lo
                next if length.zero?
                bytes = region[:body].byteslice(lo, length)
                capacity = payload_capacity(ps)
                slab = chain_capacity(capacity)
                pages, slabs = case bytes.getbyte(0)
                               when TAG_EXTERNAL then [(bytes.byteslice(5, 4).unpack1('N') + slab - 1) / slab, 0]
                               when TAG_INLINE_COMP then [0, (bytes.byteslice(1, 4).unpack1('N') + capacity - 1) / capacity]
                               when TAG_EXTERNAL_COMP then [(bytes.byteslice(5, 4).unpack1('N') + slab - 1) / slab,
                                                            (bytes.byteslice(9, 4).unpack1('N') + capacity - 1) / capacity]
                               else [0, 0]
                               end
                units[column] = [pages, slabs].freeze unless pages.zero? && slabs.zero?
              end
              scan_units[rows.length] = units.freeze unless units.empty?
            end
            rows << columns.each_with_index.map { |c, ci| leaf_value(regions[ci], c[:type], i, leaf_fetch) }
          end
          { keys: leaf_keys.freeze, children: [].freeze, page: idx, overflow_pages: overflow_pages.freeze }.freeze if profile
        when PAGE_INTERIOR
          n = pg[:item_count]
          children = (0..n).map { |i| pg[:payload].byteslice(i * 4, 4).unpack1("N") }
          child_nodes = children.map { |cp| walk.call(cp, depth + 1) }
          { keys: interior_keys(pg).freeze, children: child_nodes.freeze, page: idx }.freeze if profile
        else
          raise "expected a B-tree node page, got type #{pg[:type]}"
        end
      end
      tree = walk.call(root_page, 1)
      profile.merge!(tree: tree, node_count: seen.length, height: height, scan_units: scan_units.freeze) if profile
      rows
    end

    def interior_keys(page)
      count = page[:item_count]
      directory = 4 * (count + 1)
      start = directory + 4 * count
      previous = 0
      Array.new(count) do |index|
        finish = page[:payload].byteslice(directory + 4 * index, 4).unpack1('N')
        raise 'invalid interior separator directory' if finish < previous || start + finish > page[:payload].bytesize
        key = page[:payload].byteslice(start + previous, finish - previous)
        previous = finish
        key
      end
    end
    
    # In-order walk of an INDEX B+tree -> entry keys in ascending order. An index record is its key
    # alone (format.md "Index trees"); interior separators are routing copies, not entries.
    def read_tree_keys(image, ps, root_page, profile = nil)
      keys = []
      seen = Set.new
      height = 0
      walk = lambda do |idx, depth|
        return nil if idx.zero?
        height = [height, depth].max
        raise "B-tree page reached twice" unless seen.add?(idx)
    
        pg = read_page(image, ps, idx)
        case pg[:type]
        when PAGE_LEAF
          # An index leaf is a v24 leaf with K=0 value columns: key directory + key blob + a 1-entry
          # column directory. parse_pax_leaf returns the keys.
          leaf_keys, = parse_pax_leaf(pg[:payload], pg[:item_count], [])
          leaf_keys.each { |kk| keys << kk }
          { keys: leaf_keys.freeze, children: [].freeze, page: idx }.freeze if profile
        when PAGE_INTERIOR
          n = pg[:item_count]
          children = (0..n).map { |i| pg[:payload].byteslice(i * 4, 4).unpack1("N") }
          child_nodes = children.map { |cp| walk.call(cp, depth + 1) }
          { keys: interior_keys(pg).freeze, children: child_nodes.freeze, page: idx }.freeze if profile
        else
          raise "expected a B-tree node page, got type #{pg[:type]}"
        end
      end
      tree = walk.call(root_page, 1)
      profile.merge!(tree: tree, node_count: seen.length, height: height) if profile
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
    
    def decode_image(image, page_size: nil, lazy: false, meta: nil)
      raise "truncated database header" if image.bytesize < 12
      ps = page_size || image.byteslice(8, 4).unpack1("N")
      raise "invalid page size" unless valid_page_size?(ps)
      meta ||= select_meta(image, ps)
      free_pages = validate_free_list!(image, ps, meta)
      pool = LeafPool.new(self, image, ps, image.respond_to?(:cache_pages) ? image.cache_pages : meta[:page_count]) if lazy
      # Every page the decode reads from the catalog root is live (v34): the catalog chain, each tree
      # and GiST node, and each overflow page. The meta's count must equal that reachable set.
      @reached = Set.new
      @claimed = Set.new
      types = []
      sequences = []
      collations = []
      tables = []
      views = []
      statistics = []
      statistics_expected = {}
      # Composite types in scope for the recursive value codec; populated as the (types-first) catalog
      # is read, so every composite a table row references is registered before its rows are decoded.
      @ctypes = {}
      cat = meta[:root_page]
      seen_catalog = Set.new
      while cat != 0
        raise "catalog chain revisits a page" unless seen_catalog.add?(cat)
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
    
            raise "catalog continuation revisits a page" unless seen_catalog.add?(nxt)
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
            @ctypes[ct[:name]] = ct[:fields]
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
          if lazy
            rows = PagedRows.new(pool, entry[:root_data_page], entry[:columns], entry[:row_count])
            entry[:indexes].each do |index|
              if index[:kind] == 'gist'
                operations = index[:cols].all? { |column| column.is_a?(Integer) } ? gist_opclasses_for(entry, index) : nil
                index[:gist_profile] = read_gist_tree(image, ps, index[:root_page], operations)
              else
                index_rows = PagedRows.new(pool, index[:root_page], [], nil)
                index[:tree_profile] = index_rows.profile
              end
            end
            tables << entry.merge(pk_order: entry[:pk], rows: rows, storage_keys: rows.keys, tree_profile: rows.profile)
            next
          end
          storage_keys = []
          profile = {}
          rows = read_tree_rows(image, ps, entry[:root_data_page], entry[:columns], storage_keys, profile)
          raise "persisted row_count does not match decoded rows" unless entry[:row_count] == rows.length
          entry[:indexes].each do |ix|
            if ix[:kind] == "gist"
              read_gist_keys(image, ps, ix[:root_page])
            else
              ix[:tree_profile] = {}
              read_tree_keys(image, ps, ix[:root_page], ix[:tree_profile])
            end
          end
          tables << entry.merge(pk_order: entry[:pk], rows: rows, storage_keys: storage_keys, tree_profile: profile)
        end
        cat = nxt
      end
      validate_catalog(types, tables)
      statistics_expected.each do |(table, column), (mcv_count, histogram_count, term_count)|
        item = statistics.find { |s| s[:table] == table && s[:column] == column }
        raise "incomplete statistics group" unless item && item[:mcv].size == mcv_count &&
                                                   item[:histogram].size == histogram_count &&
                                                   (item[:terms] ? item[:terms][:entries].size : 0) == term_count
      end
      tables.each { |table| table[:statistics] = statistics.select { |item| item[:table] == table[:name] } }
      reached = @reached
      @reached = nil
      @claimed = nil
      paged = tables.any? { |table| table[:rows].is_a?(PagedRows) }
      raise "live page count #{meta[:live]} does not match #{reached.size} reachable pages" unless paged || meta[:live] == reached.size
      raise "a free-list page is reachable" unless (free_pages & reached.to_a).empty?
      { types: types, sequences: sequences, collations: collations, tables: tables,
        statistics: statistics, views: views, meta: meta, free_pages: free_pages,
        live_pages: paged ? DeferredLivePages.new(reached, tables) : reached.to_a }
    end

    def validate_catalog(types, tables)
      by_name = tables.to_h { |table| [table[:name], table] }
      raise "duplicate table name" unless by_name.size == tables.size
      raise "duplicate composite type name" unless types.map { |type| type[:name] }.uniq.size == types.size

      fields = types.flat_map { |type| type[:fields] } + tables.flat_map { |table| table[:columns] }
      fields.each do |field|
        type = array_elem(field[:type]) || field[:type]
        raise "missing composite type #{type}" unless TYPECODE.key?(type) || range_elem(type) || @ctypes.key?(type)
      end
      tables.each do |table|
        width = table[:columns].length
        raise "invalid primary-key ordinal" unless table[:pk].uniq == table[:pk] && table[:pk].all? { |column| column < width }
        unless table[:rows].is_a?(PagedRows)
          keys = table[:storage_keys]
          raise "storage keys are not strictly ascending" unless keys.each_cons(2).all? { |left, right| left < right }
          if table[:pk].empty?
            raise "invalid synthetic rowid" unless keys.all? { |key| key.bytesize == 8 && decode_int(8, key) >= 0 }
          end
        end
        table[:fks].each do |fk|
          parent = by_name[fk[:ref_table]]
          raise "foreign key references missing table" unless parent
          raise "foreign key has no columns" if fk[:local].empty?
          fk[:local].zip(fk[:ref]).each do |local, remote|
            local_column = table[:columns][local]
            remote_column = parent[:columns][remote]
            local_type, remote_type = local_column&.dig(:type), remote_column&.dig(:type)
            integers = %w[i16 i32 i64]
            comparable = local_type && remote_type && (local_type == remote_type ||
              integers.include?(local_type) && (integers.include?(remote_type) || remote_type == 'decimal') ||
              %w[f32 f64].include?(local_type) && %w[f32 f64].include?(remote_type))
            unless comparable
              raise "foreign key column type mismatch"
            end
          end
        end
      end
    end
    
    
    def decode_sequence_entry(buf, pos)
      name, pos = take_str(buf, pos)
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
        table, pos = take_str(buf, pos)
        col_raw, pos = take(buf, pos, 2)
        owned_by = [table, col_raw.unpack1("n")]
      end
    
      [{ name: name, increment: fields[:increment], min_value: fields[:min_value],
         max_value: fields[:max_value], start: fields[:start], cache: fields[:cache],
         cycle: (flags & 0b1) != 0, last_value: fields[:last_value],
         is_called: (flags & 0b10) != 0, owned_by: owned_by }, pos]
    end
    
  end
end

require_relative "checksum_kernels"
