# frozen_string_literal: true

module JedNative
  module SpillAggregates
    private

    def spill_aggregate_supported?(node) = Functions.aggregate?(node[:name])

    def spill_accumulator(expression)
      name = expression.value
      state = if expression.extra[:within_group] || !(expression.extra[:aggregate_order] || []).empty?
        []
      elsif name.include?('object_agg')
        [false]
      elsif AggregateExecution::MOMENTS.include?(name)
        expression.type == 'decimal' ? [0, Decimal.new(0), Decimal.new(0)] : [0.0, 0.0, 0.0]
      elsif AggregateExecution::PAIRS.include?(name)
        name == 'regr_count' ? [0] : Array.new(6, 0.0)
      elsif %w[sum avg].include?(name) && %w[f32 f64].include?(expression.args.first.type)
        [0.0, 0, false, false, false]
      end
      state ? CompositeValue.new(state) : super
    end

    def spill_fold(expression, packed, values, collections, owner, unique = nil)
      name = expression.value
      value = values.first
      if name.include?('object_agg')
        key, operand = values
        raise Error.new(name.start_with?('jsonb') ? '22023' : '22004', 'JSON object key must not be null') if key.nil?
        raise Error.new('22023', 'JSON object key must be scalar') if key.is_a?(Array) || key.is_a?(JsonValue)
        key = json_key_text(key, expression.args.first.type)
        if name.include?('_unique') && !unique.insert([owner, key])
          raise Error.new('22030', 'duplicate JSON object key')
        end
        unless name.end_with?('_strict') && operand.nil?
          charge
          image = json_image_value(operand, expression.args[1].type, expression.args[1], binary: true)
          collections.push(owner, [key, image])
        end
        return CompositeValue.new([true])
      elsif AggregateExecution::MOMENTS.include?(name)
        return packed if value.nil?
        count, sum, squares = packed
        if expression.type == 'decimal'
          value = Decimal.parse(value)
          decimal_aggregate_work(value, value, :*)
          square = value * value
          decimal_aggregate_work(sum, value, :+)
          decimal_aggregate_work(squares, square, :+)
          return CompositeValue.new([count + 1, sum + value, squares + square])
        end
        value = value.to_f
        next_count, old_sum = count + 1.0, sum
        sum += value
        if count.positive?
          delta = value * next_count - sum
          squares += delta * delta / (next_count * count)
          if sum.infinite? || squares.infinite?
            raise Error.new('22003', 'floating-point aggregate overflow') if old_sum.finite? && value.finite?
            squares = Float::NAN
          end
        elsif !value.finite?
          squares = Float::NAN
        end
        return CompositeValue.new([next_count, sum, squares])
      elsif AggregateExecution::PAIRS.include?(name)
        return packed if values.any?(&:nil?)
        return CompositeValue.new([packed[0] + 1]) if name == 'regr_count'
        count, sx, sxx, sy, syy, sxy = packed
        y, x = values.map(&:to_f)
        n, old_x, old_y = count + 1.0, sx, sy
        sx += x
        sy += y
        if count.positive?
          tx, ty = x * n - sx, y * n - sy
          factor = 1.0 / (n * count)
          sxx += tx * tx * factor
          syy += ty * ty * factor
          sxy += tx * ty * factor
          if sx.infinite? || sxx.infinite?
            raise Error.new('22003', 'floating-point aggregate overflow') if old_x.finite? && x.finite?
            sxx = Float::NAN
          end
          if sy.infinite? || syy.infinite?
            raise Error.new('22003', 'floating-point aggregate overflow') if old_y.finite? && y.finite?
            syy = Float::NAN
          end
          if sxy.infinite?
            raise Error.new('22003', 'floating-point aggregate overflow') if old_x.finite? && x.finite? && old_y.finite? && y.finite?
            sxy = Float::NAN
          end
        else
          sxx = sxy = Float::NAN unless x.finite?
          syy = sxy = Float::NAN unless y.finite?
        end
        return CompositeValue.new([n, sx, sxx, sy, syy, sxy])
      elsif %w[sum avg].include?(name) && %w[f32 f64].include?(expression.args.first.type)
        return packed if value.nil?
        total, count, nan, positive, negative = packed
        value = value.to_f
        if value.nan?
          nan = true
        elsif value.infinite?
          value.positive? ? positive = true : negative = true
        else
          total = Types.binary('+', total, value, type: expression.type)
        end
        return CompositeValue.new([total, count + 1, nan, positive, negative])
      end
      super
    end

    def spill_finalize(expression, state, collections, owner)
      return spill_finalize_ordered(expression, collections, owner) if expression.extra[:within_group]
      return spill_finalize_argument_order(expression, collections, owner) unless (expression.extra[:aggregate_order] || []).empty?
      name = expression.value
      if name.include?('object_agg')
        return nil unless state[0]
        data, pairs = {}, []
        collections.each(owner) do |key, value|
          data[key] = value.value
          pairs << JsonValue.escape(key) + ' : ' + value.to_s unless name.start_with?('jsonb')
        end
        return JsonValue.new(data, binary: name.start_with?('jsonb'), source: name.start_with?('jsonb') ? nil : '{ ' + pairs.join(', ') + ' }')
      elsif AggregateExecution::MOMENTS.include?(name)
        count, sum, squares = state
        sample = %w[var_samp variance stddev_samp stddev].include?(name)
        return nil if count.zero? || sample && count < 2
        if expression.type == 'decimal'
          n = Decimal.new(count)
          numerator = n * squares - sum * sum
          return Decimal.new(0) unless numerator.positive?
          result = numerator / (n * Decimal.new(sample ? count - 1 : count))
          return name.start_with?('stddev') ? aggregate_decimal_sqrt(result, result.scale) : result
        end
        result = squares / (sample ? count - 1 : count)
        return name.start_with?('stddev') ? Math.sqrt(result) : result
      elsif AggregateExecution::PAIRS.include?(name)
        count, sx, sxx, sy, syy, sxy = state
        return count.to_i if name == 'regr_count'
        return nil if count.zero?
        return case name
        when 'covar_pop' then sxy / count
        when 'covar_samp' then count < 2 ? nil : sxy / (count - 1)
        when 'corr' then sxx.zero? || syy.zero? ? nil : sxy / Math.sqrt(sxx * syy)
        when 'regr_avgx' then sx / count
        when 'regr_avgy' then sy / count
        when 'regr_sxx' then sxx
        when 'regr_syy' then syy
        when 'regr_sxy' then sxy
        when 'regr_slope' then sxx.zero? ? nil : sxy / sxx
        when 'regr_intercept' then sxx.zero? ? nil : (sy - sx * sxy / sxx) / count
        when 'regr_r2' then sxx.zero? ? nil : syy.zero? ? 1.0 : sxy * sxy / (sxx * syy)
        end
      elsif %w[sum avg].include?(name) && %w[f32 f64].include?(expression.args.first.type)
        total, count, nan, positive, negative = state
        return nil if count.zero?
        return Float::NAN if nan || positive && negative
        return Float::INFINITY if positive
        return -Float::INFINITY if negative
        return name == 'sum' ? total : total / count
      end
      super
    end

    def spill_finalize_argument_order(expression, collections, owner)
      orderings = expression.extra[:aggregate_order]
      sorter = ExternalSorter.new(self) do |left, right|
        result = 0
        orderings.each_with_index do |(_, order), index|
          result = order_compare(left[index], right[index], order)
          break unless result.zero?
        end
        result
      end
      collections.each(owner) { |row| sorter.push(row) }
      folded = expression.dup
      folded.extra = expression.extra.merge(aggregate_order: [])
      state = spill_accumulator(folded)
      retained, unique = SpillHashRows.new(self), SpillStateMap.new(self)
      sorter.each { |row| state = spill_fold(folded, state, row.drop(orderings.length), retained, 0, unique) }
      spill_finalize(folded, state, retained, 0)
    ensure
      sorter&.close
      retained&.close
      unique&.close
    end

    def spill_finalize_ordered(expression, collections, owner)
      name = expression.value
      orders = expression.extra[:aggregate_order]
      direct = expression.args.map { |argument| evaluate(argument, []) }
      compare = lambda do |left, right|
        result = 0
        orders.each_with_index do |(_, order), index|
          result = order_compare(left[index], right[index], order)
          break unless result.zero?
        end
        result
      end
      if AggregateExecution::HYPOTHETICAL.include?(name)
        count = before = equal = 0
        distinct = SpillStateMap.new(self) if name == 'dense_rank'
        collections.each(owner) do |row|
          count += 1
          comparison = compare.call(row, direct)
          if comparison.negative?
            before += 1 if !distinct || distinct.insert(row)
          elsif comparison.zero?
            equal += 1
          end
        end
        return case name
        when 'rank', 'dense_rank' then before + 1
        when 'percent_rank' then count.zero? ? 0.0 : before.to_f / count
        when 'cume_dist' then (before + equal + 1).to_f / (count + 1)
        end
      end
      sorter = ExternalSorter.new(self, &compare)
      collections.each(owner) { |row| sorter.push(row) }
      if name == 'mode'
        winner, previous, count, best = nil, nil, 0, 0
        sorter.each do |row|
          if previous && Types.compare(previous, row[0]).zero?
            count += 1
          else
            previous, count = row[0], 1
          end
          winner, best = previous, count if count > best
        end
        return winner
      end
      sorted = RowSpool.new(self)
      sorter.each { |row| sorted.push(row) }
      sorter.close
      fraction = direct.first
      return nil if sorted.empty? || fraction.nil?
      fractions = fraction.is_a?(Array) ? fraction : [fraction]
      values = fractions.map do |part|
        next if part.nil?
        value = part.to_f
        raise Error.new('22003', 'percentile fraction must be between zero and one') if value.nan? || value < 0 || value > 1
        position = name == 'percentile_disc' ? [(value * sorted.length).ceil - 1, 0].max : value * (sorted.length - 1)
        lower, upper = nil, nil
        sorted.each_with_index do |row, index|
          lower = row[0] if index == position.floor
          upper = row[0] if index == position.ceil
          break if index >= position.ceil
        end
        name == 'percentile_disc' ? lower : aggregate_percentile(name, position - position.floor, [lower, upper])
      end
      fraction.is_a?(Array) ? ArrayValue.new(values, element_type: expression.type.delete_suffix('[]'), lower_bound: fraction.respond_to?(:lower_bound) ? fraction.lower_bound : 1) : values.first
    ensure
      sorter&.close
      sorted&.close
      distinct&.close
    end
  end
  Engine.prepend(SpillAggregates)
end
