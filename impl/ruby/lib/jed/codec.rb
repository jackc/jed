# frozen_string_literal: true

module Jed
  # Decodes the native result buffer (spec/design/ruby.md §3). {take} copies a buffer out of native
  # memory, frees it, and returns a parsed Hash — so no native allocation outlives the call.
  module Codec
    module_function

    TAG_ERROR = 0
    TAG_STATEMENT = 1
    TAG_QUERY = 2
    TAG_HANDLE = 3
    TAG_UNIT = 4

    # Copy the result buffer at `ptr` (a Fiddle::Pointer) into a Ruby String, free the native
    # allocation, and return the parsed Hash. The first 8 bytes carry the total length.
    def take(ptr)
      raise Jed::LoadError, "null result from the native library" if ptr.null?

      total = ptr[0, 8].unpack1("Q<")
      bytes = ptr[0, total]
      Jed::FFI::FREE.call(ptr)
      parse(bytes)
    end

    # Parse a copied result buffer into a Hash keyed by `:kind`.
    def parse(bytes)
      cur = Cursor.new(bytes)
      cur.skip(8) # total-length header
      tag = cur.u8
      case tag
      when TAG_ERROR
        { kind: :error, sqlstate: cur.ascii(5), message: cur.lstr }
      when TAG_STATEMENT
        has = cur.u8
        rows_affected = cur.i64
        cost = cur.i64
        { kind: :statement, rows_affected: (has == 1 ? rows_affected : nil), cost: cost }
      when TAG_QUERY
        cost = cur.i64
        ncols = cur.u32
        names = Array.new(ncols) { nil }
        types = Array.new(ncols) { nil }
        ncols.times do |i|
          names[i] = cur.lstr
          types[i] = cur.lstr
        end
        nrows = cur.u32
        rows = Array.new(nrows) do
          Array.new(ncols) do
            cur.u8 == 1 ? nil : cur.lstr
          end
        end
        { kind: :query, columns: names, types: types, rows: rows, cost: cost }
      when TAG_HANDLE
        { kind: :handle, ptr: cur.u64 }
      when TAG_UNIT
        { kind: :unit }
      else
        raise Jed::LoadError, "unknown result tag #{tag} from the native library"
      end
    end

    # A little-endian byte cursor over a binary String (matches the cdylib's `Buf` writer).
    class Cursor
      def initialize(bytes)
        @b = bytes.dup.force_encoding(Encoding::BINARY)
        @i = 0
      end

      def skip(n)
        @i += n
      end

      def bytes(n)
        s = @b.byteslice(@i, n)
        @i += n
        s
      end

      def u8 = bytes(1).unpack1("C")
      def u32 = bytes(4).unpack1("L<")
      def i64 = bytes(8).unpack1("q<")
      def u64 = bytes(8).unpack1("Q<")

      # A fixed-width ASCII field (the SQLSTATE).
      def ascii(n) = bytes(n).force_encoding(Encoding::UTF_8)

      # A length-prefixed UTF-8 string (`lstr`).
      def lstr
        n = u32
        bytes(n).force_encoding(Encoding::UTF_8)
      end
    end
  end
end
