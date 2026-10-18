# frozen_string_literal: true

module JedNative
  # PostgreSQL's decimal range reductions and display-scale rules, using exact
  # Ruby integer coefficients throughout the numerical computation.
  module DecimalMath
    module_function
    def weight(value) = value.zero? ? 0 : (value.coefficient.abs.to_s.length - 1 - value.scale).div(4)
    def precision(value) = value.zero? ? 0 : value.coefficient.abs.to_s.length
    def trunc_div(a, b) = a.negative? ? -(-a / b) : a / b
    def step(context, scale, sqrt, *values)
      return unless context
      digits = 32 + scale.abs + values.sum { |v| precision(v) + v.scale }
      groups = (digits + 3) / 4
      work = groups * groups
      work *= digits.bit_length if sqrt
      context.charge(work)
      context.consume_scalar_bytes(8 * digits) if context.respond_to?(:consume_scalar_bytes)
    end
    def mul(a, b, scale) = Decimal.new(a.coefficient * b.coefficient, a.scale + b.scale, check: false).round(scale)
    def sqrt_value(value, scale, context = nil)
      return Decimal.new(0, [scale, 0].max) if value.zero?
      raise Error.new('2201F', 'cannot take square root of a negative number') if value.negative?
      step(context, scale, true, value)
      guard = [scale, 0].max + 1
      guard = (value.scale + 1) / 2 + 1 if 2 * guard < value.scale
      number = value.coefficient * 10**(2 * guard - value.scale)
      # Integer Newton iteration, without a floating-point seed.
      root = 1 << ((number.bit_length + 1) / 2)
      loop do
        next_root = (root + number / root) / 2
        break if next_root >= root
        root = next_root
      end
      Decimal.new(root, guard, check: false).round(scale)
    end
    def exp_value(value, scale, context = nil)
      step(context, scale, false, value)
      estimate = value.to_f
      if estimate.abs >= 2000 * 3
        raise Error.new('22003', 'numeric value out of range') if estimate.positive?
        return Decimal.new(0, [scale, 0].max)
      end
      decimal_weight = (estimate * 0.434294481903252).truncate
      divisions = 0
      while estimate.abs > 0.01
        divisions += 1
        estimate /= 2
      end
      reduced = divisions.positive? ? value.divide(Decimal.new(2**divisions), value.scale + divisions) : value
      significant = [1 + decimal_weight + scale + (divisions * 0.301029995663981).truncate, 0].max + 8
      local_scale = significant - 1
      result = Decimal.new(1) + reduced
      term = mul(reduced, reduced, local_scale).divide(Decimal.new(2), local_scale)
      n = 2
      until term.zero?
        step(context, local_scale, false, result, term, reduced)
        result += term
        n += 1
        term = mul(term, reduced, local_scale).divide(Decimal.new(n), local_scale)
      end
      divisions.times do
        step(context, scale, false, result)
        result = mul(result, result, [significant - weight(result) * 8, 0].max)
      end
      result.round(scale)
    end
    def ln_value(value, scale, context = nil)
      raise Error.new('2201E', 'cannot take logarithm of zero or negative number') unless value.positive?
      step(context, scale, false, value)
      reduced, factor, roots = value, 2, 0
      while reduced <= Decimal.new(9, 1) || reduced >= Decimal.new(11, 1)
        local = scale - trunc_div(weight(reduced) * 4, 2) + 8
        reduced = sqrt_value(reduced, local, context)
        factor *= 2
        roots += 1
      end
      local = scale + ((roots + 1) * 0.301029995663981).truncate + 8
      result = (reduced - 1).divide(reduced + 1, local)
      power = result
      square = mul(result, result, local)
      divisor = 1
      loop do
        step(context, local, false, result, power, square)
        divisor += 2
        power = mul(power, square, local)
        term = power.divide(Decimal.new(divisor), local)
        break if term.zero?
        result += term
        break if weight(term) < weight(result) - trunc_div(local * 2, 4)
      end
      mul(result, Decimal.new(factor), scale)
    end
    def ln_weight(value, context)
      if value >= Decimal.new(9, 1) && value <= Decimal.new(11, 1)
        delta = value - 1
        return delta.zero? ? 0 : precision(delta) - 1 - delta.scale
      end
      result = ln_value(value, 20, context)
      return 0 if result.zero?
      weight = precision(result) - 1 - result.scale
      weight.negative? ? weight + 1 : weight
    end
    def scale_for(weight, *values) = [[16 - weight, 0, *values.map(&:scale)].max, 1000].min
    def logarithm(base, number, context)
      step(context, 0, false, base, number)
      raise Error.new('2201E', 'cannot take logarithm of zero or negative number') unless base.positive? && number.positive?
      bw, nw = ln_weight(base, context), ln_weight(number, context)
      result_weight = nw - bw
      scale = scale_for(result_weight, base, number)
      denominator = ln_value(base, [scale + result_weight - bw + 8, 0].max, context)
      numerator = ln_value(number, [scale + result_weight - nw + 8, 0].max, context)
      numerator.divide(denominator, scale)
    end
    def power(base, exponent, context)
      step(context, 0, false, base, exponent)
      integer = exponent.trunc
      if integer == exponent && integer.coefficient.between?(-2_147_483_648, 2_147_483_647)
        n = integer.coefficient
        raise Error.new('2201F', 'zero raised to a negative power is undefined') if base.zero? && n.negative?
        estimate = 0
        unless base.zero?
          estimate = (ln_value(base.abs, 30, context).divide(ln_value(Decimal.new(10), 30, context), 30) * n).to_f
        end
        raise Error.new('22003', 'numeric value out of range') if estimate > Decimal::MAX_DIGITS
        return Decimal.new(0, 1000) if estimate + 1 < -1000
        scale = scale_for(estimate.truncate, base, exponent)
        return Decimal.new(1).round(scale) if n.zero?
        return base.round(scale) if n == 1
        return Decimal.new(1).divide(base, scale) if n == -1
        return mul(base, base, scale) if n == 2
        return Decimal.new(0, scale) if base.zero?
        significant = 1 + scale + estimate.truncate + ln_value(Decimal.new(n.abs), 12, context).to_i + 8
        mask, product = n.abs, base
        result = mask.odd? ? base : Decimal.new(1)
        loop do
          mask >>= 1
          break if mask.zero?
          step(context, scale, false, product, result)
          local = [[significant - 8 * weight(product), 2 * product.scale].min, 0].max
          product = mul(product, product, local)
          if mask.odd?
            local = [[significant - (weight(product) + weight(result)) * 4, product.scale + result.scale].min, 0].max
            result = mul(product, result, local)
          end
        end
        return n.negative? ? Decimal.new(1).divide(result, scale) : result.round(scale)
      end
      return Decimal.new(0, 16) if base.zero?
      raise Error.new('2201F', 'negative number raised to a non-integer power is undefined') if base.negative?
      ln_w = ln_weight(base, context)
      local = [8 - ln_w, 0].max
      logarithm = ln_value(base, local, context)
      exponent_value = mul(logarithm, exponent, local)
      estimate = exponent_value.to_f * 0.434294481903252
      raise Error.new('22003', 'numeric value out of range') if estimate > Decimal::MAX_DIGITS
      return Decimal.new(0, 1000) if estimate < -1000
      scale = scale_for(estimate.truncate, base, exponent)
      local = [[scale + estimate.truncate, 0].max - ln_w + 8, 0].max
      exponent_value = mul(ln_value(base, local, context), exponent, local)
      exp_value(exponent_value, scale, context)
    end
    def call(name, args, context: nil)
      values = args.map { |v| Decimal.parse(v) }
      a, b = values
      case name
      when 'sqrt'
        step(context, 0, false, a)
        sqrt_value(a, scale_for(trunc_div(weight(a) * 4, 2) + 1, a), context)
      when 'exp'
        step(context, 0, false, a)
        estimate = [[a.to_f * 0.434294481903252, -2000].max, 2000].min
        exp_value(a, scale_for(estimate.truncate, a), context)
      when 'ln'
        step(context, 0, false, a)
        raise Error.new('2201E', 'cannot take logarithm of zero or negative number') unless a.positive?
        ln_value(a, scale_for(ln_weight(a, context), a), context)
      when 'log'
        if b then logarithm(a, b, context)
        else step(context, 0, false, a); logarithm(Decimal.new(10), a, context)
        end
      when 'log10'
        step(context, 0, false, a)
        logarithm(Decimal.new(10), a, context)
      when 'pow', 'power' then power(a, b, context)
      end
    end
  end
end
