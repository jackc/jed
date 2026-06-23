# frozen_string_literal: true

module Jed
  # Encodes `$N` bind parameters into the native param buffer (spec/design/ruby.md §3a). Only the
  # unambiguous Ruby scalars map this slice — `nil`, `Integer`, `Float`, `true`/`false`, `String`;
  # richer typed binds (`BigDecimal`, `Time`, arrays, …) are a follow-on (ruby.md §6). The engine
  # context-types each `$N` and coerces/range-checks the bound value, so an `Integer` binds to an
  # `i16`/`i32`/`i64`/`decimal` site alike (an out-of-range value traps `22003` at bind).
  module Params
    module_function

    TAG_NULL = 0
    TAG_INT = 1
    TAG_FLOAT = 2
    TAG_BOOL = 3
    TAG_TEXT = 4

    I64_MIN = -(2**63)
    I64_MAX = (2**63) - 1

    # Encode `params` (an Array) into a binary String, or `nil` when there are none.
    def encode(params)
      return nil if params.empty?

      buf = String.new(encoding: Encoding::BINARY)
      buf << [params.length].pack("L<")
      params.each { |p| append(buf, p) }
      buf
    end

    def append(buf, param)
      case param
      when nil
        buf << [TAG_NULL].pack("C")
      when Integer
        buf << [TAG_INT].pack("C") << pack_i64(param)
      when Float
        buf << [TAG_FLOAT].pack("C") << [param].pack("E") # little-endian IEEE-754 double
      when true
        buf << [TAG_BOOL].pack("C") << [1].pack("C")
      when false
        buf << [TAG_BOOL].pack("C") << [0].pack("C")
      when String
        bytes = param.encode(Encoding::UTF_8).b
        buf << [TAG_TEXT].pack("C") << [bytes.bytesize].pack("L<") << bytes
      else
        raise ArgumentError,
          "unsupported bind-parameter type #{param.class} (#{param.inspect}); bind nil/Integer/" \
          "Float/true/false/String — richer typed params are a follow-on (spec/design/ruby.md §6)"
      end
    end

    # Pack an Integer as i64, raising a clear error if it overflows. NOTE: `Array#pack("q<")`
    # silently *wraps* an out-of-range Integer (e.g. 2**70 → 0) rather than raising, so we must
    # range-check explicitly — a silent wrap would bind a wrong value. Binding a larger integer
    # needs a decimal/text param (the richer-types follow-on, ruby.md §6).
    def pack_i64(n)
      unless n >= I64_MIN && n <= I64_MAX
        raise ArgumentError,
          "integer bind parameter #{n} is out of range for a 64-bit integer; bind it as a String " \
          "for a decimal/text column (richer typed params are a follow-on, spec/design/ruby.md §6)"
      end
      [n].pack("q<")
    end
  end
end
