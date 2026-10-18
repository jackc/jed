# frozen_string_literal: true

module JedNative
  # Statistics are already resident catalog facts. All interpolation uses exact
  # integer fractions; no sampled row or index leaf is read while planning.
  module StatisticsEstimates
    def estimate_scan_candidate(stored, predicate, access, statement)
      previous = @native_estimate_table
      @native_estimate_table = stored
      super
    ensure
      @native_estimate_table = previous
    end

    def scan_access_plan(stored, predicate, statement = nil)
      previous = @native_estimate_table
      @native_estimate_table = stored
      super
    ensure
      @native_estimate_table = previous
    end

    def statistics_distribution(stored, ordinal)
      fact = stored[:storage_statistics]&.find { |entry| entry[:column] == ordinal }
      return nil unless fact && fact[:analyzed_rows].positive?
      rows = stored[:rows].length
      nulls = [estimate_scale(rows, fact[:null_count], fact[:analyzed_rows]), rows].min
      nonnull = rows - nulls
      analyzed_nonnull = fact[:analyzed_rows] - fact[:null_count]
      distinct = fact[:distinct_count]
      if distinct
        distinct = estimate_scale(nonnull, distinct, analyzed_nonnull) if distinct * 10 > analyzed_nonnull && analyzed_nonnull.positive?
        distinct = [distinct, nonnull].min
      end
      consumed = 0
      mcv = fact[:mcv].map do |value, frequency|
        count = [estimate_scale(rows, frequency, fact[:sample_rows]), nonnull - consumed].min
        consumed += count
        [value, count]
      end
      { fact: fact, rows: rows, nulls: nulls, nonnull: nonnull, ndv: distinct,
        mcv: mcv, residual: nonnull - consumed }
    end

    private

    def estimate_distinct_rows(rows, expressions)
      return super unless @native_estimate_table && expressions.all? { |expression| expression.kind == :column }
      facts = expressions.map { |expression| statistics_distribution(@native_estimate_table, expression.value) }
      return super unless facts.all? { |fact| fact && fact[:ndv] }
      [rows, facts.reduce(1) { |count, fact| [QueryPlans::MAX_ESTIMATE, count * (fact[:ndv] + (fact[:nulls].positive? ? 1 : 0))].min }].min
    end

    def plan_select(statement)
      previous = @native_estimate_table
      source = statement[:from]&.first
      @native_estimate_table = source && @tables[source[:name]] if statement[:from].to_a.length == 1 && statement[:joins].to_a.empty?
      super
    ensure
      @native_estimate_table = previous
    end

    def plan_mutation(statement)
      previous = @native_estimate_table
      @native_estimate_table = @tables[statement[:table]]
      super
    ensure
      @native_estimate_table = previous
    end

    def estimate_scale(count, numerator, denominator)
      return 0 if count.zero? || numerator.zero? || denominator.zero?
      [QueryPlans::MAX_ESTIMATE, (count * numerator + denominator - 1) / denominator].min
    end

    def estimate_predicate(rows, expression)
      return super unless @native_estimate_table && expression && rows.positive?
      estimate = statistics_predicate(expression, @native_estimate_table)
      return super if estimate.nil?
      estimate_scale(rows, estimate, @native_estimate_table[:rows].length)
    end

    def statistics_predicate(expression, stored)
      if expression.kind == :binary && %w[= <> != < <= > >=].include?(expression.value)
        left, right = expression.args
        operator = expression.value
        if right.kind == :column
          left, right = right, left
          operator = { '=' => '=', '<>' => '<>', '!=' => '!=', '<' => '>', '<=' => '>=', '>' => '<', '>=' => '<=' }.fetch(operator)
        end
        return nil unless left.kind == :column
        distribution = statistics_distribution(stored, left.value)
        return nil unless distribution && distribution[:ndv]
        value = statistics_operand(right, stored[:columns][left.value][:type])
        return nil if value == :unsupported
        return statistics_comparison(distribution, stored, left.value, operator, value)
      end
      if expression.kind == :unary && %w[is\ null is\ not\ null].include?(expression.value) && expression.args[0].kind == :column
        distribution = statistics_distribution(stored, expression.args[0].value)
        return nil unless distribution
        return expression.value == 'is null' ? distribution[:nulls] : distribution[:nonnull]
      end
      if expression.kind == :column && expression.type == 'boolean'
        distribution = statistics_distribution(stored, expression.value)
        return nil unless distribution && distribution[:ndv]
        return statistics_comparison(distribution, stored, expression.value, '=', true)
      end
      if expression.kind == :quantified && expression.args[0].kind == :column
        op, quantifier = expression.extra.values_at(:op, :quantifier)
        if op == '=' && quantifier != 'all' || op == '<>' && quantifier == 'all'
          column = expression.args[0]
          distribution = statistics_distribution(stored, column.value)
          return nil unless distribution && distribution[:ndv]
          elements = estimate_array_elements(expression.args[1])
          return 0 if op == '<>' && (plan_constant(expression.args[1]).nil? || elements.any? { |element| plan_constant(element).nil? })
          seen = {}
          result = elements.sum do |element|
            value = statistics_operand(element, column.type)
            next 0 if value.nil?
            next 0 if value == :unsupported
            unless value == :unknown
              key = statistics_operand_key(stored, column.value, value)
              next 0 if seen[key]
              seen[key] = true
            end
            statistics_comparison(distribution, stored, column.value, '=', value)
          end
          result = [result, distribution[:nonnull]].min
          return op == '<>' ? distribution[:nonnull] - result : result
        end
      end
      gin = statistics_gin_predicate(expression, stored)
      gin && gin[:rows]
    end

    def statistics_operand(expression, type)
      value = plan_constant(expression)
      return value if value.nil? || value == :unknown
      return value if expression.type == type
      if Types::INTEGER_BITS.key?(type)
        value = value.to_i if value.is_a?(Decimal) && value == value.trunc
        return nil unless value.is_a?(Integer)
        bits = Types::INTEGER_BITS[type]
        return value.between?(-(1 << (bits - 1)), (1 << (bits - 1)) - 1) ? value : nil
      end
      return Decimal.new(value) if type == 'decimal' && value.is_a?(Integer)
      :unsupported
    end

    def statistics_operand_key(stored, ordinal, value)
      column = stored[:columns][ordinal]
      statistics_codec.key_body(column[:type], statistics_value(column[:type], value), column[:collation] == 'C' ? nil : column[:collation])
    end

    def statistics_fact_key(stored, ordinal, value)
      column = stored[:columns][ordinal]
      statistics_codec.key_body(column[:type], value, column[:collation] == 'C' ? nil : column[:collation])
    end

    def statistics_comparison(distribution, stored, ordinal, operator, value)
      return 0 if value.nil?
      nonnull, distinct = distribution.values_at(:nonnull, :ndv)
      if value == :unknown
        return nil unless %w[= <> !=].include?(operator)
        equality = estimate_scale(nonnull, 1, [distinct, 1].max)
        return operator == '=' ? equality : nonnull - equality
      end
      key = statistics_operand_key(stored, ordinal, value)
      facts = distribution[:fact]
      if %w[= <> !=].include?(operator)
        match = distribution[:mcv].find { |item, _| statistics_fact_key(stored, ordinal, item) == key }
        if match
          equality = match[1]
        elsif !facts[:stale] && facts[:sample_rows] == facts[:analyzed_rows] && facts[:mcv].length == facts[:distinct_count]
          equality = 0
        else
          equality = estimate_scale(distribution[:residual], 1, [distinct - facts[:mcv].length, 1].max)
          equality = 1 if equality.zero? && nonnull.positive?
        end
        return operator == '=' ? equality : nonnull - equality
      end
      include_equal = %w[<= >].include?(operator)
      mcv_rows = distribution[:mcv].sum do |item, count|
        comparison = statistics_fact_key(stored, ordinal, item) <=> key
        comparison.negative? || include_equal && comparison.zero? ? count : 0
      end
      histogram = facts[:histogram]
      mass = if histogram.length >= 2
        position = histogram.bsearch_index do |item|
          comparison = statistics_fact_key(stored, ordinal, item) <=> key
          include_equal ? comparison.positive? : comparison >= 0
        end || histogram.length
        [distribution[:residual], estimate_scale(distribution[:residual], position, histogram.length - 1)].min
      else
        0
      end
      lower = [nonnull, mcv_rows + mass].min
      %w[> >=].include?(operator) ? nonnull - lower : lower
    end

    def estimate_paired_range(rows, operand, comparisons)
      stored = @native_estimate_table
      return super unless stored && operand.kind == :column
      distribution = statistics_distribution(stored, operand.value)
      return super unless distribution && distribution[:ndv]
      lower = comparisons.find { |operator, _| %w[> >=].include?(operator) }
      upper = comparisons.find { |operator, _| %w[< <=].include?(operator) }
      return super unless lower && upper
      low_value, high_value = [lower, upper].map { |_, source| statistics_operand(source, operand.type) }
      return super if [low_value, high_value].any? { |value| %i[unknown unsupported].include?(value) }
      return 0 if low_value.nil? || high_value.nil?
      low_key = statistics_operand_key(stored, operand.value, low_value)
      high_key = statistics_operand_key(stored, operand.value, high_value)
      return 0 if low_key > high_key
      low_inclusive, high_inclusive = lower[0] == '>=', upper[0] == '<='
      selected = distribution[:mcv].sum do |value, count|
        key = statistics_fact_key(stored, operand.value, value)
        (key > low_key || low_inclusive && key == low_key) && (key < high_key || high_inclusive && key == high_key) ? count : 0
      end
      histogram = distribution[:fact][:histogram]
      if histogram.length >= 2
        first = histogram.bsearch_index do |value|
          key = statistics_fact_key(stored, operand.value, value)
          low_inclusive ? key >= low_key : key > low_key
        end || histogram.length
        last = histogram.bsearch_index do |value|
          key = statistics_fact_key(stored, operand.value, value)
          high_inclusive ? key > high_key : key >= high_key
        end || histogram.length
        selected += [distribution[:residual], estimate_scale(distribution[:residual], [last - first, 0].max, histogram.length - 1)].min
      else
        selected += estimate_scale(distribution[:residual], 1, 200)
      end
      estimate_scale(rows, selected, distribution[:rows])
    end

    def statistics_gin_predicate(expression, stored)
      # Logical array equality keeps its scalar equality selectivity. Its GIN
      # access candidate may independently use element term facts.
      return nil if expression.kind == :binary && expression.value == '='
      bound = auxiliary_scan_bounds(stored, expression).find { |candidate| candidate[:kind] == 'gin' }
      bound && statistics_gin_bound(stored, bound)
    end

    def statistics_gin_bound(stored, bound)
      metadata = stored[:native_auxiliary_indexes][bound[:name]]
      arms = bound[:arms].map do |position, operator, source|
        ordinal = metadata[:columns][position]
        distribution = statistics_distribution(stored, ordinal)
        return nil unless distribution && distribution[:fact][:terms]
        value = plan_constant(source)
        return nil if value == :unknown
        replacement = Expression.new(kind: :literal, type: source.type, value: value)
        prepared = gin_query_terms(stored, bound[:name], [position, operator, replacement])
        return nil unless prepared
        terms, mode = prepared
        fact = distribution[:fact]
        counts = terms.map do |term|
          # Multi-column index terms carry a key-position byte; statistics are
          # per column and retain the unprefixed term.
          term = term.byteslice(1..) if metadata[:columns].length > 1
          frequency = fact[:terms][:entries].find { |key, _| key == term }&.last
          count = if frequency
            [estimate_scale(distribution[:rows], frequency, fact[:analyzed_rows]), distribution[:nonnull]].min
          elsif fact[:terms][:untracked_rows].zero? && !fact[:stale]
            0
          else
            [1, [estimate_scale(distribution[:rows], (fact[:terms][:untracked_rows] + 1) / 2, fact[:analyzed_rows]), distribution[:nonnull]].min].max
          end
          [count, term]
        end.sort
        if mode == :any
          entries = counts.sum(&:first)
          rows = [distribution[:nonnull], entries].min
        else
          rows = counts.first&.first || 0
          entries = rows
          counts.drop(1).each do |count, _|
            entries += rows
            rows = estimate_scale(rows, count, distribution[:rows])
          end
        end
        { rows: rows, entries: entries }
      end
      total = metadata[:columns].sum do |ordinal|
        fact = stored[:storage_statistics].find { |entry| entry[:column] == ordinal }
        return nil unless fact && fact[:terms] && fact[:analyzed_rows].positive?
        estimate_scale(stored[:rows].length, fact[:terms][:occurrences], fact[:analyzed_rows])
      end
      { rows: [stored[:rows].length, arms.sum { |arm| arm[:rows] }].min,
        entries: arms.sum { |arm| arm[:entries] }, total_entries: total }
    end

    def estimate_scan_candidate(stored, predicate, access, statement)
      super
      return access unless access[:kind] == :gin
      estimate = statistics_gin_bound(stored, access)
      return access unless estimate
      index = stored[:native_auxiliary_indexes][access[:name]]
      pages = estimate[:entries].zero? ? 0 : [[estimate_scale(index[:node_count], estimate[:entries], estimate[:total_entries]), index[:height]].max, index[:node_count]].min
      rows = estimate[:rows]
      if access[:order] != :blocking && statement&.dig(:limit, :kind) == :literal
        limit = statement[:limit][:value]
        offset = statement[:offset]&.dig(:value) || 0
        rows = [rows, required_scan_input(predicate, limit + offset, rows)].min if limit.is_a?(Integer)
      end
      access[:scan_rows] = rows
      access[:scan_cost] = [QueryPlans::MAX_ESTIMATE, pages + estimate[:entries] + rows * (stored[:native_primary_metadata][:height] + 1)].min
      access[:output_rows] = [rows, estimate_predicate(stored[:rows].length, predicate)].min
      access[:pipeline_cost] = [QueryPlans::MAX_ESTIMATE, access[:scan_cost] + rows * expression_work(predicate)].min
      access
    end
  end
  Engine.prepend(StatisticsEstimates)
end
