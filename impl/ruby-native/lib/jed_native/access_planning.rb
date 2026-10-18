# frozen_string_literal: true

module JedNative
  # Inventory and execution share one selected access descriptor. Estimates use
  # resident catalog/skeleton facts and resolved expressions, never leaf values.
  module AccessPlanning
    ACCESS_RANK = { primary: 0, secondary: 1, gist: 2, gin: 3,
                    primary_interval: 4, secondary_interval: 5, full: 6 }.freeze

    def select(statement)
      previous = [@native_scan_statement, @native_stream_scan, @native_reverse_scan]
      @native_scan_statement = statement
      @native_stream_scan, @native_reverse_scan = false, false
      super
    ensure
      @native_scan_statement, @native_stream_scan, @native_reverse_scan = previous
    end

    def update(statement)
      previous = @native_scan_statement
      @native_scan_statement = statement
      super
    ensure
      @native_scan_statement = previous
    end

    def delete(statement)
      previous = @native_scan_statement
      @native_scan_statement = statement
      super
    ensure
      @native_scan_statement = previous
    end

    def primary_scan(stored, predicate)
      access = scan_access_plan(stored, predicate, @native_scan_statement)
      execute_scan_access(stored, predicate, access)
    end

    def execute_scan_access(stored, predicate, access)
      case access[:kind]
      when :full then nil
      when :primary then primary_key_scan(stored, predicate)
      when :secondary then secondary_scan(stored, predicate, selected_name: access[:name], allow_full: access[:parts].empty?)
      when :primary_interval, :secondary_interval then execute_interval_access(stored, predicate, access)
      when :gin, :gist then execute_auxiliary_scan(stored, access)
      end
    end

    def scan_access_plan(stored, predicate, statement = nil)
      candidates = scan_access_candidates(stored, predicate)
      candidates.each { |candidate| estimate_scan_candidate(stored, predicate, candidate, statement) }
      candidates.min_by { |candidate| [candidate[:pipeline_cost], ACCESS_RANK.fetch(candidate[:kind]), candidate[:name].to_s.b] }
    end

    def scan_access_candidates(stored, predicate)
      candidates = [{ kind: :full, terms: [] }]
      return candidates unless predicate && stored[:collation_skew].to_a.empty?
      terms = conjunctions(predicate)
      primary = stored[:native_primary_metadata]
      if primary && !primary[:columns].empty?
        keys = primary[:columns].each_with_index.map { |column, i| Expression.new(kind: :column, type: primary[:types][i], value: column) }
        parts = access_key_parts(keys, terms)
        unless parts.empty?
          candidates << { kind: :primary, parts: parts, terms: parts.flat_map { |part| part[:terms] },
                          unique: parts.length == keys.length && parts.all? { |part| part[:point] } }
        end
        if keys.length == 1 && (interval = symbolic_interval_source(predicate, keys.first))
          candidates << interval.merge(kind: :primary_interval, key: keys.first, unique: true)
        end
      end
      (stored[:native_secondary_metadata] || {}).sort_by { |name, _| name.b }.each do |name, metadata|
        next unless partial_predicate_implied?(metadata[:predicate], terms)
        parts = access_key_parts(metadata[:expressions], terms)
        if !parts.empty? || metadata[:predicate]
          candidates << { kind: :secondary, name: name, parts: parts,
                          terms: parts.flat_map { |part| part[:terms] }, partial: metadata[:predicate],
                          unique: !!metadata[:definition][:unique] && parts.length == metadata[:expressions].length && parts.all? { |part| part[:point] } }
        end
        if !metadata[:orders][0][0] && (interval = symbolic_interval_source(predicate, metadata[:expressions].first))
          candidates << interval.merge(kind: :secondary_interval, name: name, key: metadata[:expressions].first,
                                       partial: metadata[:predicate], unique: !!metadata[:definition][:unique] && metadata[:expressions].length == 1)
        end
      end
      auxiliary_scan_bounds(stored, predicate).each do |bound|
        candidates << bound.merge(kind: bound[:kind].to_sym, terms: []) if auxiliary_plan_usable?(stored, bound)
      end
      candidates
    end

    # A candidate's order is a structural fact. No expression is evaluated here.
    def scan_candidate_order(statement, stored, candidate)
      return :blocking unless statement && streaming_select_shape?(statement)
      orders = statement[:order_by] || []
      return :none if orders.empty?
      secondary = %i[secondary secondary_interval].include?(candidate[:kind])
      metadata = secondary ? stored[:native_secondary_metadata][candidate[:name]] : stored[:native_primary_metadata]
      keys = secondary ? metadata[:expressions] : metadata[:columns].each_with_index.map { |column, i| Expression.new(kind: :column, type: metadata[:types][i], value: column) }
      return :blocking if keys.empty? || secondary && orders.length > keys.length
      directions = orders.take(keys.length).map { |order| order[:direction] == :desc }
      return :blocking unless directions.uniq.length == 1
      return :blocking if directions.first && (secondary || orders.length < keys.length)
      return :blocking if secondary && metadata[:orders].any? { |descending, nulls_first| descending || nulls_first }
      source = statement[:from].first
      scope = table_columns(stored, source[:alias] || source[:name])
      projections = expand_stars(statement[:columns], scope)
      valid = orders.take(keys.length).each_with_index.all? do |order, i|
        node = order[:expr]
        if node[:kind] == :literal && node[:value].is_a?(Integer)
          node = projections[node[:value] - 1]&.dig(:expr)
        elsif node[:kind] == :column && !node[:table]
          projection = projections.find { |entry| (entry[:alias] || expression_name(entry[:expr])) == node[:name] }
          node = projection[:expr] if projection
        end
        next false unless node && metadata_expression?(node)
        same_key_expression?(compile(node, scope), keys[i])
      end
      valid ? (directions.first ? :reverse : :forward) : :blocking
    end

    private

    def estimate_array_elements(expression)
      return expression.args if expression.kind == :array
      value = plan_constant(expression)
      return [] if value.nil?
      return value.map { |item| Expression.new(kind: :literal, type: expression.type.delete_suffix('[]'), value: item) } if value.is_a?(Array)
      Array.new(10) { Expression.new(kind: :outer, type: expression.type.delete_suffix('[]')) }
    end

    def plan_constant(expression)
      return :unknown unless expression
      if expression.kind == :array
        values = expression.args.map { |argument| plan_constant(argument) }
        return values.include?(:unknown) ? :unknown : values
      end
      super
    end

    def expression_work(expression)
      work = super
      if expression.is_a?(Expression) && expression.kind == :quantified
        source = expression.args[1]
        known = source.kind == :array || plan_constant(source).is_a?(Array)
        work += estimate_array_elements(source).length if known
      end
      work
    end

    def estimate_predicate(rows, expression)
      return rows unless expression
      return 0 if rows.zero?
      if expression.kind == :literal || expression.kind == :param
        value = plan_constant(expression)
        return value == true ? rows : 0 unless value == :unknown
      end
      if expression.kind == :binary
        operator, left, right = expression.value, *expression.args
        if operator == 'and'
          terms = conjunctions(expression)
          paired = {}
          terms.each_with_index do |term, position|
            next if paired.key?(position) || term.kind != :binary || !%w[< <= > >=].include?(term.value)
            operand, source, direction = estimate_comparison(term)
            next unless source && plan_constant(source) != :unknown
            partner = ((position + 1)...terms.length).find do |index|
              next false if paired.key?(index)
              other, bound, other_direction = estimate_comparison(terms[index])
              bound && plan_constant(bound) != :unknown && direction != other_direction && same_key_expression?(operand, other)
            end
            if partner
              paired[position], paired[partner] = :range, :skip
            end
          end
          return terms.each_with_index.reduce(rows) do |count, (term, position)|
            if paired[position] == :skip
              count
            elsif paired[position] == :range
              operand, bound, direction = estimate_comparison(term)
              counterpart = terms.each_with_index.find { |other, index| paired[index] == :skip && same_key_expression?(operand, estimate_comparison(other)[0]) }
              estimate_paired_range(count, operand, [[term.value, bound], [counterpart[0].value, estimate_comparison(counterpart[0])[1]]])
            else
              estimate_predicate(count, term)
            end
          end
        end
        return [rows, estimate_predicate(rows, left) + estimate_predicate(rows, right)].min if operator == 'or'
        return 0 if %w[= <> != < > <= >=].include?(operator) && expression.args.any? { |argument| plan_constant(argument).nil? }
        return rows - (rows + 199) / 200 if %w[<> !=].include?(operator)
        denominator = %w[= like ilike ~ ~* !~ !~*].include?(operator) ? 200 : %w[< <= > >=].include?(operator) ? 3 : nil
        return (rows + denominator - 1) / denominator if denominator
      elsif expression.kind == :unary
        return rows - estimate_predicate(rows, expression.args.first) if expression.value == 'not'
        return (rows + 199) / 200 if expression.value == 'is null'
        return rows - (rows + 199) / 200 if expression.value == 'is not null'
      elsif expression.kind == :quantified
        op, quantifier = expression.extra.values_at(:op, :quantifier)
        if op == '=' && quantifier != 'all' || op == '<>' && quantifier == 'all'
          elements = estimate_array_elements(expression.args[1])
          return 0 if op == '<>' && (plan_constant(expression.args[1]).nil? || elements.any? { |element| plan_constant(element).nil? })
          estimate = [rows, elements.sum { |element| plan_constant(element).nil? ? 0 : (rows + 199) / 200 }].min
          return op == '<>' ? rows - estimate : estimate
        end
        return (rows + 2) / 3
      end
      # A structural boolean expression has the boolean fallback. Specialized
      # predicates without a selectivity class retain the opaque fallback.
      bare = %i[column param call].include?(expression.kind) && expression.type == 'boolean'
      denominator = bare ? 2 : 3
      (rows + denominator - 1) / denominator
    end

    def estimate_comparison(expression)
      return [nil, nil, nil] unless expression.kind == :binary && %w[< <= > >=].include?(expression.value)
      left, right = expression.args
      direction = %w[< <=].include?(expression.value) ? :upper : :lower
      if %i[literal param].include?(left.kind)
        left, right = right, left
        direction = direction == :upper ? :lower : :upper
      end
      [left, right, direction]
    end

    def estimate_paired_range(rows, operand, comparisons)
      (rows + 199) / 200
    end

    def metadata_expression?(node)
      case node
      when Array then node.all? { |child| metadata_expression?(child) }
      when Hash
        !%i[subquery exists array_query].include?(node[:kind]) && !node[:query] && node.each_value.all? { |child| metadata_expression?(child) }
      else true
      end
    end

    def partial_predicate_implied?(partial, terms)
      !partial || conjunctions(partial).all? { |part| terms.any? { |term| same_key_expression?(part, term) } }
    end

    def symbolic_interval_source(predicate, key)
      terms = conjunctions(predicate)
      source = terms.find { |term| interval_set_source?(term) && symbolic_interval_shape(term, key) }
      return nil unless source
      count, arrays = symbolic_interval_shape(source, key)
      clips = terms.reject { |term| term.equal?(source) || interval_set_source?(term) }.select { |term| symbolic_interval_shape(term, key) }
      { source: source, intervals: count, arrays: arrays, terms: [source] + clips }
    end

    def symbolic_interval_shape(expression, key)
      if expression.kind == :binary && %w[and or].include?(expression.value)
        return nil if expression.value == 'and' && conjunctions(expression).count { |term| term.kind == :quantified } > 1
        left, right = expression.args.map { |arg| symbolic_interval_shape(arg, key) }
        return nil unless left && right
        return expression.value == 'or' ? [left[0] + right[0], left[1] + right[1]] : [[left[0], right[0]].max, left[1] + right[1]]
      end
      if expression.kind == :quantified
        return nil unless expression.extra[:op] == '=' && expression.extra[:quantifier] != 'all' && same_key_expression?(key, expression.args[0])
        source = expression.args[1]
        exact = source.type == key.type + '[]'
        permitted = exact || Types::INTEGER_BITS.key?(key.type) && (Types::INTEGER_BITS.key?(source.type.delete_suffix('[]')) || source.type == 'decimal[]') || key.type == 'decimal' && Types::INTEGER_BITS.key?(source.type.delete_suffix('[]'))
        return nil unless permitted
        scalar_source = %i[literal param outer].include?(source.kind)
        array_source = source.kind == :array && source.args.all? { |arg| %i[literal param outer].include?(arg.kind) } && (exact || source.args.all? { |arg| %i[param outer].include?(arg.kind) || arg.value.nil? || arg.value.is_a?(Integer) && arg.extra&.dig(:untyped) })
        return scalar_source || array_source ? [1, 1] : nil
      end
      return [1, 1] if expression.kind == :query_membership && interval_set_source?(expression) && !expression.extra[:row_value] && same_key_expression?(key, expression.args[0])
      key_comparisons(key, [expression]).empty? ? nil : [1, 0]
    end

    def auxiliary_plan_usable?(stored, bound)
      metadata = stored[:native_auxiliary_indexes][bound[:name]]
      bound[:arms].all? do |position, operator, source|
        value = plan_constant(source)
        if bound[:kind] == 'gist'
          !(operator == '@>' && value.is_a?(RangeValue) && value.empty)
        elsif %w[@> = ?&].include?(operator) && value != :unknown && !value.nil?
          # Empty containment cannot safely narrow the table: empty containers
          # have no GIN posting from which to recover every qualifying row.
          if metadata[:types][position] == 'jsonb' && operator == '@>'
            !gin_terms(stored, bound[:name], position, value).empty?
          else
            !value.respond_to?(:empty?) || !value.empty?
          end
        else
          true
        end
      end
    end

    def estimate_scan_candidate(stored, predicate, access, statement)
      structure = stored[:native_primary_metadata] || { node_count: stored[:rows].empty? ? 0 : 1, height: stored[:rows].empty? ? 0 : 1 }
      count = stored[:rows].length
      rows = candidate_selectivity(count, access, stored)
      index = case access[:kind]
      when :secondary, :secondary_interval then stored[:native_secondary_metadata][access[:name]]
      when :gin then stored[:native_auxiliary_indexes][access[:name]]
      else structure
      end
      pages = if access[:kind] == :full
        structure[:node_count]
      elsif rows.zero?
        0
      else
        [[candidate_selectivity(index[:node_count], access, stored), index[:height]].max, index[:node_count]].min
      end
      work = access[:kind] == :gin ? rows : access[:kind] == :gist ? pages : 0
      original_rows = rows
      order = scan_candidate_order(statement, stored, access)
      limit = statement && statement[:limit] && statement[:limit][:kind] == :literal && statement[:limit][:value]
      offset = statement && statement[:offset] && statement[:offset][:kind] == :literal ? statement[:offset][:value] || 0 : 0
      if order != :blocking && limit.is_a?(Integer)
        target = [limit + offset, 0].max
        cap = required_scan_input(predicate, target, rows)
        rows = [rows, cap].min
        if %i[full primary primary_interval].include?(access[:kind]) && rows < original_rows
          pages = rows.zero? ? 0 : [pages, [rows, structure[:height]].max].min
        elsif access[:native_order_only] && rows < original_rows
          pages = rows.zero? ? 0 : [[rows, index[:height]].max, index[:node_count]].min
        end
      end
      fetches = %i[secondary secondary_interval gin gist].include?(access[:kind]) ? rows * structure[:height] : 0
      access[:scan_rows] = rows
      access[:scan_cost] = [pages + fetches + rows + work, QueryPlans::MAX_ESTIMATE].min
      access[:output_rows] = [rows, estimate_predicate(count, predicate)].min
      access[:pipeline_cost] = [access[:scan_cost] + rows * expression_work(predicate), QueryPlans::MAX_ESTIMATE].min
      access[:order] = order
      access
    end

    def required_scan_input(predicate, target, maximum)
      return 0 if target.zero? || maximum.zero?
      return maximum if estimate_predicate(maximum, predicate) < target
      low, high = 0, maximum
      while low < high
        middle = low + (high - low) / 2
        estimate_predicate(middle, predicate) >= target ? high = middle : low = middle + 1
      end
      low
    end

    def candidate_selectivity(count, access, stored)
      case access[:kind]
      when :full then count
      when :gin, :gist
        parts = access[:arms].map do |position, operator, source|
          null = plan_constant(source).nil?
          metadata = stored[:native_auxiliary_indexes][access[:name]]
          denominator = access[:kind] == :gist && !Types::RANGES.key?(metadata[:types][position]) ? 200 : 100
          [null, denominator]
        end
        if access[:kind] == :gin
          [count, parts.sum { |null, denominator| null ? 0 : (count + denominator - 1) / denominator }].min
        else
          parts.reduce(count) { |n, (null, denominator)| null ? 0 : (n + denominator - 1) / denominator }
        end
      else
        combined = access[:terms].uniq.reduce { |left, right| Expression.new(kind: :binary, type: 'boolean', value: 'and', args: [left, right]) }
        rows = estimate_predicate(count, combined)
        if access[:unique] && access[:kind] == :primary || access[:unique] && access[:kind] == :secondary
          rows = [rows, 1].min
        end
        if (partial = access[:partial]) && !access[:unique]
          constrained = (access[:parts] || []).flat_map { |part| expression_nodes(part[:key]).select { |node| node.kind == :column }.map(&:value) }
          constrained += expression_nodes(access[:key]).select { |node| node.kind == :column }.map(&:value) if access[:key]
          conjunctions(partial).each do |term|
            columns = expression_nodes(term).select { |node| node.kind == :column }.map(&:value)
            rows = estimate_predicate(rows, term) if (columns & constrained).empty?
          end
        end
        rows
      end
    end

    def interval_page_estimate(source, index, access, stored)
      return 0 if index[:node_count].zero?
      if source.kind == :binary && source.value == 'or'
        return source.args.sum { |part| interval_page_estimate(part, index, access, stored) }
      end
      if source.kind == :quantified
        elements = estimate_array_elements(source.args[1])
        return elements.sum do |element|
          next 0 if plan_constant(element).nil?
          [[(index[:node_count] + 199) / 200, index[:height]].max, index[:node_count]].min
        end
      end
      rows = estimate_predicate(index[:node_count], source)
      rows.zero? ? 0 : [[rows, index[:height]].max, index[:node_count]].min
    end

    def execute_interval_access(stored, predicate, access)
      name = access[:name]
      key = access[:key]
      encoder = name ? ->(value) { secondary_prefix_key(stored, name, [value]) } : ->(value) { primary_prefix_key(stored, [value]) }
      intervals = scan_intervals(predicate, key, encoder)
      return nil unless intervals
      entries = name ? stored[:native_secondary_indexes][name] : stored[:native_primary_entries]
      intervals = intervals.reverse if @native_reverse_scan
      iterator = Enumerator.new do |output|
        intervals.each do |lower, upper|
          first = lower ? entries.bsearch_index { |encoded, _| encoded >= lower } || entries.length : 0
          last = upper ? entries.bsearch_index { |encoded, _| encoded >= upper } || entries.length : entries.length
          charge(name ? secondary_page_count(stored, name, lower, upper) : primary_page_count(stored, lower, upper))
          ordinals = entries[first...last].map(&:last)
          charge_scan_payload(stored, ordinals) unless name
          scan_rows(stored, ordinals, lookup: !!name).each { |row| output << row }
        end
      end
      @native_stream_scan ? iterator : iterator.to_a
    end
  end
  Engine.prepend(AccessPlanning)
end
