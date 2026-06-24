# frozen_string_literal: true

require "bigdecimal"
require "date"

module Jed
  # Encodes `$N` bind parameters into the native param buffer (spec/design/ruby.md §3a). Maps the
  # Ruby scalars with a faithful jed counterpart — `nil`, `Integer`, `Float`, `true`/`false`,
  # `String`, plus (mirroring ActiveRecord) `BigDecimal`→decimal, `Date`→date, `Time`/`DateTime`→
  # timestamptz. The engine context-types each `$N` and coerces/range-checks the bound value, so a
  # `BigDecimal` binds to a `decimal` site, an `Integer` to `i16`/`i32`/`i64`/`decimal`, etc.
  module Params
    module_function

    TAG_NULL = 0
    TAG_INT = 1
    TAG_FLOAT = 2
    TAG_BOOL = 3
    TAG_TEXT = 4
    TAG_DECIMAL = 5
    TAG_DATE = 6
    TAG_TIMESTAMPTZ = 7

    I64_MIN = -(2**63)
    I64_MAX = (2**63) - 1
    # jed reserves the i32/i64 extremes for the date/timestamp ±infinity sentinels, so a bound finite
    # value must stay strictly inside them.
    I32_FINITE_MIN = -(2**31) + 1
    I32_FINITE_MAX = (2**31) - 2
    I64_FINITE_MIN = I64_MIN + 1
    I64_FINITE_MAX = I64_MAX - 1

    DATE_EPOCH = Date.new(1970, 1, 1)
    TIME_EPOCH = Time.utc(1970, 1, 1)

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
      when BigDecimal
        append_decimal(buf, param)
      # DateTime is a subclass of Date, so it must precede the Date branch; it is an instant, so it
      # binds as a timestamp like Time (not as a Date).
      when DateTime
        append_timestamp(buf, param.to_time)
      when Date
        buf << [TAG_DATE].pack("C") << [day_count(param)].pack("l<")
      when Time
        append_timestamp(buf, param)
      when String
        bytes = param.encode(Encoding::UTF_8).b
        buf << [TAG_TEXT].pack("C") << [bytes.bytesize].pack("L<") << bytes
      else
        raise ArgumentError,
          "unsupported bind-parameter type #{param.class} (#{param.inspect}); bind nil/Integer/" \
          "Float/true/false/String/BigDecimal/Date/Time (spec/design/ruby.md §3a)"
      end
    end

    # Decompose a BigDecimal into (sign, unscaled digit string, scale) for jed's
    # `Decimal::from_digits_scale`. `BigDecimal#split` yields `[sign, significant_digits, 10, exp]`
    # where the value is `sign · 0.<digits> · 10^exp`, so `scale = digits.length - exp` (a negative
    # scale means trailing zeros, folded back into the digits at scale 0).
    def append_decimal(buf, dec)
      unless dec.finite?
        raise ArgumentError,
          "cannot bind a non-finite BigDecimal (#{dec}); jed decimal is finite-only (ruby.md §3a)"
      end

      sign, digits, _base, exp = dec.split
      scale = digits.length - exp
      if scale.negative?
        digits += "0" * -scale
        scale = 0
      end
      buf << [TAG_DECIMAL].pack("C")
      buf << [sign.negative? ? 1 : 0].pack("C")
      buf << [digits.bytesize].pack("L<") << digits.b
      buf << [scale].pack("L<")
    end

    def append_timestamp(buf, time)
      utc = time.getutc
      micros = (utc.to_i * 1_000_000) + utc.usec # exact: to_i floors, usec is the 0..999_999 part
      unless micros.between?(I64_FINITE_MIN, I64_FINITE_MAX)
        raise ArgumentError, "Time bind parameter #{time} is out of jed's representable range"
      end
      buf << [TAG_TIMESTAMPTZ].pack("C") << [micros].pack("q<")
    end

    # Days since 1970-01-01 for a Date (exact integer; BC-correct via Date arithmetic).
    def day_count(date)
      days = (date - DATE_EPOCH).to_i
      unless days.between?(I32_FINITE_MIN, I32_FINITE_MAX)
        raise ArgumentError, "Date bind parameter #{date} is out of jed's representable range"
      end
      days
    end

    # Pack an Integer as i64, raising a clear error if it overflows. NOTE: `Array#pack("q<")`
    # silently *wraps* an out-of-range Integer (e.g. 2**70 → 0) rather than raising, so we must
    # range-check explicitly — a silent wrap would bind a wrong value. Binding a larger integer
    # needs a decimal param (a BigDecimal; ruby.md §3a).
    def pack_i64(n)
      unless n >= I64_MIN && n <= I64_MAX
        raise ArgumentError,
          "integer bind parameter #{n} is out of range for a 64-bit integer; bind it as a " \
          "BigDecimal for a decimal column (spec/design/ruby.md §3a)"
      end
      [n].pack("q<")
    end
  end
end
