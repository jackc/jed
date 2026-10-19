# frozen_string_literal: true

module JedNative
  # Half-open byte intervals share the storage key's order. Canonical unions
  # avoid reading or mutating a row twice, including adjacent point sources.
  module IntervalScans
    def primary_scan(stored, predicate)
      return super unless predicate
      candidates = []
      primary = stored[:native_primary_metadata]
      if primary && primary[:columns].length == 1
        key = Expression.new(kind: :column, type: primary[:types][0], value: primary[:columns][0])
        encoder = ->(value) { primary_prefix_key(stored, [value]) }
        intervals = scan_intervals(predicate, key, encoder)
        candidates << [nil, intervals] unless intervals.nil?
      end
      (stored[:native_secondary_metadata] || {}).keys.sort_by(&:b).each do |name|
        details = stored[:native_secondary_metadata][name]
        next if details[:orders][0][0]
        terms = conjunctions(predicate)
        next if details[:predicate] && !conjunctions(details[:predicate]).all? { |part| terms.any? { |term| same_key_expression?(part, term) } }
        key = details[:expressions][0]
        encoder = ->(value) { secondary_prefix_key(stored, name, [value]) }
        intervals = scan_intervals(predicate, key, encoder)
        candidates << [name, intervals] unless intervals.nil?
      end
      return super if candidates.empty?
      name, intervals = candidates.first
      entries = name ? stored[:native_secondary_indexes][name] : stored[:native_primary_entries]
      intervals = intervals.reverse if @native_reverse_scan
      iterator = Enumerator.new do |output|
        intervals.each do |lower, upper|
          charge(name ? secondary_page_count(stored, name, lower, upper) : primary_page_count(stored, lower, upper))
          if stored[:rows].is_a?(PagedRows)
            found = paged_scan_ordinals(stored, name, lower: lower, upper: upper)
          else
            first = lower ? entries.bsearch_index { |key, _| key >= lower } || entries.length : 0
            last = upper ? entries.bsearch_index { |key, _| key >= upper } || entries.length : entries.length
            found = entries[first...last].map(&:last)
          end
          charge_scan_payload(stored, found) unless name
          scan_rows(stored, found, lookup: !!name).each { |row| output << row }
        end
      end
      @native_stream_scan ? iterator : iterator.to_a
    end

    private

    def scan_intervals(predicate, key, encoder)
      terms = conjunctions(predicate)
      source = terms.find do |term|
        interval_set_source?(term) && !reduce_intervals(term, key, encoder).nil?
      end
      return nil unless source
      intervals = reduce_intervals(source, key, encoder)
      terms.each do |term|
        next if term.equal?(source) || interval_set_source?(term)
        clip = reduce_intervals(term, key, encoder)
        intervals = intersect_intervals(intervals, clip) if clip
      end
      merge_intervals(intervals)
    end

    def interval_set_source?(expression)
      expression.kind == :binary && expression.value == 'or' ||
        expression.kind == :quantified && expression.extra[:op] == '=' && expression.extra[:quantifier] != 'all' ||
        expression.kind == :query_membership && !expression.extra[:correlated] && !expression.extra[:negated] && expression.extra[:op] == '=' && expression.extra[:quantifier] != 'all'
    end

    def reduce_intervals(expression, key, encoder)
      if expression.kind == :binary && %w[and or].include?(expression.value)
        return nil if expression.value == 'and' && conjunctions(expression).count { |term| term.kind == :quantified } > 1
        left = reduce_intervals(expression.args[0], key, encoder)
        right = reduce_intervals(expression.args[1], key, encoder)
        return nil if left.nil? || right.nil?
        return expression.value == 'or' ? merge_intervals(left + right) : intersect_intervals(left, right)
      end
      if expression.kind == :quantified
        return nil unless expression.extra[:op] == '=' && expression.extra[:quantifier] != 'all' && same_key_expression?(key, expression.args[0])
        source = expression.args[1]
        exact = source.type == key.type + '[]'
        permitted = exact || Types::INTEGER_BITS.key?(key.type) && (Types::INTEGER_BITS.key?(source.type.delete_suffix('[]')) || source.type == 'decimal[]') || key.type == 'decimal' && Types::INTEGER_BITS.key?(source.type.delete_suffix('[]'))
        return nil unless permitted
        if %i[literal param outer].include?(source.kind)
          values = evaluate(source, []) || []
        elsif source.kind == :array && source.args.all? { |arg| %i[literal param outer].include?(arg.kind) } && (exact || source.args.all? { |arg| %i[param outer].include?(arg.kind) || arg.kind == :literal && (arg.value.nil? || arg.value.is_a?(Integer) && arg.extra&.dig(:untyped)) })
          values = source.args.map { |arg| evaluate(arg, []) }
        else
          return nil
        end
        return merge_intervals(values.flat_map { |value| point_interval(value, key.type, encoder) })
      end
      if expression.kind == :query_membership
        data = expression.extra
        return nil unless interval_set_source?(expression) && !data[:row_value] && same_key_expression?(key, expression.args[0])
        return merge_intervals(data[:rows].flat_map { |row| point_interval(row.first, key.type, encoder) })
      end
      comparison = key_comparisons(key, [expression]).first
      return nil unless comparison
      operator, source = comparison
      return [] if operator == 'is null'
      value = evaluate(source, [])
      return [] if value.nil?
      return point_interval(value, key.type, encoder) if operator == '='
      if (bits = Types::INTEGER_BITS[key.type])
        minimum, maximum = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
        return %w[> >=].include?(operator) ? [[nil, nil]] : [] if value < minimum
        return %w[< <=].include?(operator) ? [[nil, nil]] : [] if value > maximum
      end
      encoded = encoder.call(value)
      case operator
      when '<' then [[nil, encoded]]
      when '<=' then [[nil, prefix_successor(encoded)]]
      when '>=' then [[encoded, nil]]
      when '>'
        successor = prefix_successor(encoded)
        successor ? [[successor, nil]] : []
      end
    end

    def point_interval(value, type, encoder)
      return [] if value.nil?
      if Types::INTEGER_BITS.key?(type)
        if value.is_a?(Decimal)
          return [] unless value == value.trunc
          value = value.to_i
        end
        return [] unless value.is_a?(Integer)
        bits = Types::INTEGER_BITS[type]
        return [] unless value.between?(-(1 << (bits - 1)), (1 << (bits - 1)) - 1)
      elsif type == 'decimal' && value.is_a?(Integer)
        value = Decimal.new(value)
      end
      encoded = encoder.call(value)
      [[encoded, prefix_successor(encoded)]]
    end

    def intersect_intervals(left, right)
      result = []
      left.each do |a, b|
        right.each do |c, d|
          lower = a.nil? ? c : c.nil? ? a : [a, c].max
          upper = b.nil? ? d : d.nil? ? b : [b, d].min
          result << [lower, upper] unless lower && upper && lower >= upper
        end
      end
      merge_intervals(result)
    end

    def merge_intervals(intervals)
      sorted = intervals.sort { |a, b| a[0].nil? ? (b[0].nil? ? 0 : -1) : b[0].nil? ? 1 : a[0] <=> b[0] }
      result = []
      sorted.each do |lower, upper|
        next if lower && upper && lower >= upper
        previous = result.last
        if previous && (previous[1].nil? || lower.nil? || lower <= previous[1])
          previous[1] = previous[1].nil? || upper.nil? ? nil : [previous[1], upper].max
        else
          result << [lower, upper]
        end
      end
      result
    end
  end
  Engine.prepend(IntervalScans)
end
