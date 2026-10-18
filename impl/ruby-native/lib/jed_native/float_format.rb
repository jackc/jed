# frozen_string_literal: true

module JedNative
  module FloatFormat
    module_function
    # Search exact decimal candidates inside the float's open rounding interval.
    # Open endpoints reproduce PostgreSQL's shortest output, including midpoint
    # cases where the host's usual Float#to_s chooses a different representation.
    def render(value, width = 64)
      return 'NaN' if value.nan?
      return value.positive? ? 'Infinity' : '-Infinity' if value.infinite?
      negative = (1.0 / value).negative?
      return negative ? '-0' : '0' if value.zero?
      value = value.abs
      if width == 32
        bits = [value].pack('g').unpack1('N')
        mantissa = bits & 0x7fffff
        exponent = (bits >> 23) & 255
        lower_tight = mantissa.zero? && exponent > 1
        mantissa |= 1 << 23 if exponent.positive?
        power = exponent.zero? ? -149 : exponent - 150
      else
        bits = [value].pack('G').unpack1('Q>')
        mantissa = bits & ((1 << 52) - 1)
        exponent = (bits >> 52) & 2047
        lower_tight = mantissa.zero? && exponent > 1
        mantissa |= 1 << 52 if exponent.positive?
        power = exponent.zero? ? -1074 : exponent - 1075
      end
      unit = power.negative? ? Rational(1, 1 << -power) : Rational(1 << power)
      exact = mantissa * unit
      lower = exact - unit / (lower_tight ? 4 : 2)
      upper = exact + unit / 2
      # Digit count of the exact rational locates its leading decimal exponent.
      top = exact.numerator.to_s.length - exact.denominator.to_s.length
      top -= 1 if exact < power10(top)
      digits = nil
      n = 1
      loop do
        q = top - n + 1
        factor = power10(q)
        scaled = exact / factor
        lo = scaled.floor
        hi = lo + 1
        lo_inside = lo * factor > lower
        hi_inside = hi * factor < upper
        if lo_inside || hi_inside
          pick = if lo_inside && hi_inside
                   remainder = scaled - lo
                   remainder < Rational(1, 2) || (remainder == Rational(1, 2) && lo.even?) ? lo : hi
                 elsif lo_inside then lo else hi
                 end
          digits = pick.to_s
          top = q + digits.length - 1
          digits = digits.sub(/0+\z/, '')
          break
        end
        n += 1
      end
      sign = negative ? '-' : ''
      threshold = width == 32 ? 6 : 15
      if top < -4 || top >= threshold
        fraction = digits.length > 1 ? '.' + digits[1..] : ''
        sign + digits[0] + fraction + 'e' + (top.negative? ? '-' : '+') + top.abs.to_s.rjust(2, '0')
      elsif top.negative?
        sign + '0.' + '0' * (-top - 1) + digits
      elsif top + 1 >= digits.length
        sign + digits + '0' * (top + 1 - digits.length)
      else
        sign + digits[0, top + 1] + '.' + digits[top + 1..]
      end
    end
    def power10(power) = power.negative? ? Rational(1, 10**-power) : Rational(10**power)
  end
end
