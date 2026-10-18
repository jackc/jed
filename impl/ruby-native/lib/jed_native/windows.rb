# frozen_string_literal: true

module JedNative
  # The window stage consumes the filtered/grouped row contexts, then stores its
  # results beside those contexts. It leaves the ordinary evaluator responsible
  # for the expressions around a window call.
  module WindowExecution
    WINDOW_ARITIES = {
      'row_number' => 0..0, 'rank' => 0..0, 'dense_rank' => 0..0,
      'percent_rank' => 0..0, 'cume_dist' => 0..0, 'ntile' => 1..1,
      'lag' => 1..3, 'lead' => 1..3, 'first_value' => 1..1,
      'last_value' => 1..1, 'nth_value' => 2..2
    }.freeze

    def compile(node, columns)
      if node && node[:kind] == :collate
        arg = compile(node[:expr], columns)
        raise Error.new('42804', 'COLLATE requires text') unless %w[text varchar char].include?(Types.base(arg.type))
      end
      return super unless node && node[:kind] == :call
      name = node[:name]
      if Functions.aggregate?(name) && !node[:over] && !(node[:args] || []).all? { |arg| window_nodes(arg).empty? }
        raise Error.new('42803', 'aggregate arguments cannot contain window functions')
      end
      if WINDOW_ARITIES.key?(name) && !node[:over]
        raise Error.new('42809', "window function #{name} requires OVER")
      end
      return super unless node[:over]
      unless WINDOW_ARITIES.key?(name) || Functions.aggregate?(name)
        raise Error.new('42809', "#{name} is not a window function or aggregate")
      end
      if WINDOW_ARITIES[name] && !WINDOW_ARITIES[name].cover?((node[:args] || []).length)
        raise Error.new('42883', "incorrect argument count for #{name}")
      end
      raise Error.new('0A000', 'DISTINCT window aggregates are not supported') if node[:distinct]
      raise Error.new('0A000', 'ordered window aggregate arguments are not supported') if node[:order_by] && !node[:order_by].empty?
      if node[:filter] && !Functions.aggregate?(name)
        raise Error.new('0A000', 'FILTER is only valid for aggregate window functions')
      end
      node[:args].each do |arg|
        raise Error.new('42P20', 'window function calls cannot be nested') unless window_nodes(arg).empty?
      end
      result = super
      result.extra = result.extra.merge(window_id: node.object_id)
      result
    end

    def aggregate_expr?(expression)
      return true if expression.extra.is_a?(Hash) && expression.extra[:over] && contains_aggregate?(expression.extra[:over])
      super
    end

    def select(statement)
      old_prefix = @window_scan_prefix
      @window_scan_prefix = window_scan_prefix(statement)
      super
    ensure
      @window_scan_prefix = old_prefix
    end

    def source_relation(source)
      prefix = @window_scan_prefix
      return super unless prefix && prefix[:source].equal?(source)
      stored = table(source[:name])
      rows = stored[:rows].sort do |left, right|
        comparison = 0
        prefix[:indices].each do |index|
          comparison = Types.compare(left[index], right[index])
          break unless comparison.zero?
        end
        prefix[:descending] ? -comparison : comparison
      end.take(prefix[:count])
      charge(1 + rows.length) unless rows.empty?
      columns = table_columns(stored, source[:alias] || source[:name])
      Relation.new(columns: columns, rows: rows)
    end

    def evaluate(expr, row, group = nil)
      if expr.kind == :call && expr.extra && expr.extra[:over]
        key = [expr.extra[:window_id], row.object_id, group&.object_id]
        unless @window_values && @window_values.key?(key)
          raise Error.new('42P20', 'window functions are not allowed in this context')
        end
        return @window_values[key]
      end
      super
    end

    def prepare_window_values(statement, columns, contexts)
      @window_values = {}
      if @query_memory
        previous_partition_owner = @query_memory_window_partition_owner
        @query_memory_window_partition_owner = query_memory_owner
        result_owner = query_memory_owner
      end
      definitions = {}
      (statement[:windows] || {}).each do |name, spec|
        definitions[name] = resolve_window_spec(spec, definitions)
      end
      definitions.each_value do |spec|
        (spec[:partition_by] + spec[:order_by].map { |item| item[:expr] }).each do |key|
          raise Error.new('42P20', 'window functions are not allowed in window definitions') unless window_nodes(key).empty?
          validate_window_key(compile(key, columns))
        end
      end
      forbidden = [statement[:where], statement[:having], statement[:group_by]]
      forbidden.concat((statement[:joins] || []).map { |join| join[:on] })
      unless forbidden.all? { |value| window_nodes(value).empty? }
        raise Error.new('42P20', 'window functions are not allowed in this clause')
      end
      nodes = window_nodes(statement[:columns]) + window_nodes(statement[:order_by])
      grouped = !(statement[:group_by] || []).empty? || contains_aggregate?(statement[:columns]) || contains_aggregate?(statement[:windows]) || contains_aggregate?(statement[:order_by])
      if grouped && (!nodes.empty? || !definitions.empty?) && !respond_to?(:validate_aggregate_scope)
        grouping = statement[:group_by] || []
        [statement[:columns], statement[:windows], statement[:order_by]].each do |value|
          validate_window_grouping(value, grouping, columns)
        end
      end
      shared = {}
      nodes.uniq(&:object_id).each do |node|
        spec = resolve_window_spec(node[:over], definitions)
        (spec[:partition_by] + spec[:order_by].map { |item| item[:expr] }).each do |key|
          raise Error.new('42P20', 'window functions are not allowed in window definitions') unless window_nodes(key).empty?
        end
        expression = compile(node, columns)
        partition_exprs = spec[:partition_by].map { |key| compile(key, columns) }
        order_exprs = spec[:order_by].map { |key| compile(key[:expr], columns) }
        (partition_exprs + order_exprs).each { |key| validate_window_key(key) }
        frame = prepare_window_frame(spec, columns, order_exprs)
        cache_key = [spec[:partition_by], spec[:order_by]]
        # Compound keys have independent materialization slots for each window
        # expression under the shared cost contract.
        cache_key << node.object_id unless (partition_exprs + order_exprs).all? { |key| key.kind == :column }
        partitions = shared[cache_key] ||= window_partitions(contexts, partition_exprs, order_exprs, spec[:order_by])
        partitions.each do |partition|
          evaluate_window_partition(node, expression, spec, frame, partition)
        end
        if @query_memory
          contexts.each do |row, group|
            value = @window_values[[node.object_id, row.object_id, group&.object_id]]
            query_memory_add(result_owner, LogicalMemory.value_bytes(value, expression.type))
          end
        end
      end
    ensure
      if @query_memory && result_owner
        query_memory_release(@query_memory_window_partition_owner)
        @query_memory_window_partition_owner = previous_partition_owner
      end
    end

    private

    def window_scan_prefix(statement)
      return nil unless statement[:kind] == :select && statement[:limit] && !statement[:distinct] && !statement[:where] && !statement[:with]
      return nil unless (statement[:group_by] || []).empty? && (statement[:joins] || []).empty? && (statement[:from] || []).length == 1
      source = statement[:from].first
      return nil unless source[:kind] == :table && @tables.key?(source[:name]) && !source[:columns]
      return nil if contains_aggregate?(statement[:columns]) || contains_aggregate?(statement[:windows])
      key = @tables[source[:name]][:constraints].find { |constraint| constraint[:kind] == :primary_key }
      return nil unless key
      keys = key[:columns]
      orders = statement[:order_by] || []
      return nil unless orders.length == keys.length && orders.each_with_index.all? { |order, index| order[:expr][:kind] == :column && order[:expr][:name] == keys[index] }
      descending = orders.first[:direction] == :desc
      return nil unless orders.all? { |order| (order[:direction] == :desc) == descending }
      nodes = window_nodes(statement[:columns]) + window_nodes(statement[:order_by])
      return nil if nodes.empty?
      definitions = {}
      (statement[:windows] || {}).each { |name, spec| definitions[name] = resolve_window_spec(spec, definitions) }
      nodes.each do |node|
        return nil if %w[lead percent_rank cume_dist ntile].include?(node[:name])
        spec = resolve_window_spec(node[:over], definitions)
        return nil unless spec[:partition_by].empty?
        window_order = spec[:order_by]
        return nil unless window_order.length == keys.length && window_order.each_with_index.all? do |order, index|
          order[:expr][:kind] == :column && order[:expr][:name] == keys[index] && (order[:direction] == :desc) == descending
        end
        bound = spec[:frame]&.dig(:end)
        return nil if bound && bound[:direction] == :following
      end
      limit = evaluate(compile(statement[:limit], []), [])
      offset = statement[:offset] ? evaluate(compile(statement[:offset], []), []) : 0
      return nil unless limit.is_a?(Integer) && offset.is_a?(Integer) && limit >= 0 && offset >= 0
      indices = keys.map { |name| @tables[source[:name]][:columns].index { |column| column[:name] == name } }
      { source: source, count: limit + offset, indices: indices, descending: descending }
    end

    def validate_window_key(expression)
      raise Error.new('0A000', 'correlated window keys are not supported') if expression.kind == :outer
      if expression.kind == :collate && !%w[text varchar char].include?(expression.args.first.type)
        raise Error.new('42804', 'COLLATE requires text')
      end
      (expression.args || []).each { |arg| validate_window_key(arg) if arg.is_a?(Expression) }
    end

    def validate_window_grouping(value, grouping, columns)
      if value.is_a?(Array)
        value.each { |child| validate_window_grouping(child, grouping, columns) }
      elsif value.is_a?(Hash)
        return if %i[subquery exists array_query].include?(value[:kind])
        return if value[:kind] == :call && Functions.aggregate?(value[:name]) && !value[:over]
        return if grouping.any? { |key| key == value }
        if value[:kind] == :column
          expression = compile(value, columns)
          allowed = grouping.any? do |key|
            key[:kind] == :column && compile(key, columns).value == expression.value
          end
          raise Error.new('42803', "column #{value[:name]} must appear in GROUP BY") unless allowed
        end
        value.each do |key, child|
          validate_window_grouping(child, grouping, columns) unless %i[query with].include?(key)
        end
      end
    end

    def window_nodes(value, result = [])
      if value.is_a?(Array)
        value.each { |child| window_nodes(child, result) }
      elsif value.is_a?(Hash)
        return result if %i[subquery exists array_query].include?(value[:kind])
        result << value if value[:kind] == :call && value[:over]
        value.each do |key, child|
          next if %i[query with].include?(key)
          window_nodes(child, result)
        end
      end
      result
    end

    def resolve_window_spec(spec, definitions)
      if spec.is_a?(String)
        return definitions.fetch(spec) { raise Error.new('42704', "window #{spec} does not exist") }
      end
      result = { partition_by: [], order_by: [] }.merge(spec)
      if spec[:base]
        base = definitions.fetch(spec[:base]) { raise Error.new('42704', "window #{spec[:base]} does not exist") }
        raise Error.new('42P20', 'cannot override PARTITION BY of a window') unless result[:partition_by].empty?
        if !base[:order_by].empty? && !result[:order_by].empty?
          raise Error.new('42P20', 'cannot override ORDER BY of a window')
        end
        raise Error.new('42P20', 'cannot copy a window with a frame clause') if base[:frame]
        result[:partition_by] = base[:partition_by]
        result[:order_by] = base[:order_by] if result[:order_by].empty?
      end
      result
    end

    def window_partitions(contexts, partition_exprs, order_exprs, orders)
      partitions = {}
      contexts.each_with_index do |(row, group), ordinal|
        key = partition_exprs.map { |expression| evaluate(expression, row, group) }
        order = order_exprs.map { |expression| evaluate(expression, row, group) }
        if @query_memory
          query_memory_add(@query_memory_window_partition_owner, MemorySchedule::ENTRY + query_memory_key_bytes(key)) unless partitions.key?(key)
          query_memory_add(@query_memory_window_partition_owner, query_memory_collation_bytes(order, @query_memory_window_partition_owner))
        end
        (partitions[key] ||= []) << { row: row, group: group, order: order, ordinal: ordinal }
      end
      partitions.each_value do |partition|
        unless orders.empty?
          partition.sort! do |a, b|
            comparison = 0
            orders.each_with_index do |order, index|
              comparison = order_compare(a[:order][index], b[:order][index], order)
              break unless comparison.zero?
            end
            comparison.zero? ? a[:ordinal] <=> b[:ordinal] : comparison
          end
        end
        peer = -1
        starts = []
        previous = nil
        partition.each_with_index do |entry, index|
          equal = previous && entry[:order].zip(previous).all? { |a, b| a.nil? || b.nil? ? a.nil? && b.nil? : Types.compare(a, b).zero? }
          unless equal
            peer += 1
            starts << index
          end
          entry[:peer] = peer
          previous = entry[:order]
        end
        starts << partition.length
        partition.each do |entry|
          entry[:peer_start] = starts[entry[:peer]]
          entry[:peer_end] = starts[entry[:peer] + 1]
          entry[:group_starts] = starts
        end
      end
      partitions.values
    end

    def prepare_window_frame(spec, columns, order_exprs)
      frame = spec[:frame] || { mode: :range, start: { kind: :unbounded, direction: :preceding }, end: { kind: :current_row } }
      frame = frame.transform_values { |value| value.is_a?(Hash) ? value.dup : value }
      if frame[:start][:kind] == :unbounded && frame[:start][:direction] == :following
        raise Error.new('42P20', 'frame cannot start with UNBOUNDED FOLLOWING')
      end
      if frame[:end][:kind] == :unbounded && frame[:end][:direction] == :preceding
        raise Error.new('42P20', 'frame cannot end with UNBOUNDED PRECEDING')
      end
      if frame[:mode] == :groups && order_exprs.empty?
        raise Error.new('42P20', 'GROUPS requires ORDER BY')
      end
      [:start, :end].each do |side|
        bound = frame[side]
        next unless bound[:kind] == :offset
        expression = compile(bound[:value], [])
        offset = evaluate(expression, [])
        raise Error.new('22004', 'frame offset cannot be NULL') if offset.nil?
        raise Error.new('22013', 'frame offset must not be negative') if offset.negative?
        if frame[:mode] == :range
          raise Error.new('42P20', 'RANGE offset requires one ORDER BY key') unless order_exprs.length == 1
          type = order_exprs.first.type
          unless Types::INTEGER_BITS.key?(type) || %w[decimal f32 f64].include?(type)
            raise Error.new('0A000', 'RANGE offsets require a numeric ordering key')
          end
          if Types::INTEGER_BITS.key?(type) && !offset.is_a?(Integer)
            raise Error.new('0A000', 'integer RANGE keys require integer offsets')
          end
        elsif !offset.is_a?(Integer)
          raise Error.new('42804', 'frame offset must be an integer')
        end
        bound[:offset] = offset
      end
      frame
    end

    def evaluate_window_partition(node, expression, spec, frame, partition)
      count = partition.length
      return if count.zero?
      name = expression.value
      args = expression.args
      memory_owner = query_memory_owner if @query_memory
      cached_values = if %w[lag lead first_value last_value nth_value].include?(name)
        partition.map do |entry|
          value = evaluate(args[0], entry[:row], entry[:group])
          query_memory_add(memory_owner, LogicalMemory.value_bytes(value, args[0].type)) if @query_memory
          value
        end
      end
      ntile = nil
      if name == 'ntile'
        first = partition.first
        ntile = evaluate(args.first, first[:row], first[:group])
        raise Error.new('22014', 'argument of ntile must be greater than zero') if ntile && ntile <= 0
      end
      aggregate_rows = nil
      aggregate_expr = nil
      if Functions.aggregate?(name)
        aggregate_rows = partition.map do |entry|
          values = args.map { |arg| arg.kind == :star ? 1 : evaluate(arg, entry[:row], entry[:group]) }
          moving = frame[:start][:kind] != :unbounded || frame[:exclude] && frame[:exclude] != :no_others || expression.extra[:filter_expr]
          retaining = name.match?(/\A(?:array_agg|string_agg|jsonb?_agg|jsonb?_object_agg)/)
          if @query_memory && (moving || retaining)
            args.each_with_index do |arg, index|
              query_memory_add(memory_owner, LogicalMemory.value_bytes(values[index], arg.type)) unless arg.kind == :star
            end
          end
          values + [expression.extra[:filter_expr] ? evaluate(expression.extra[:filter_expr], entry[:row], entry[:group]) : true]
        end
        synthetic_args = args.each_with_index.map { |arg, index| Expression.new(kind: :column, type: arg.type, value: index) }
        aggregate_expr = Expression.new(kind: :call, type: expression.type, value: name, args: synthetic_args,
                                       extra: expression.extra.merge(over: nil, filter_expr: Expression.new(kind: :column, type: 'boolean', value: args.length)))
      end
      incremental = aggregate_expr && %w[count sum avg min max bool_and bool_or every].include?(name) &&
                    !expression.extra[:filter_expr] && (!frame[:exclude] || frame[:exclude] == :no_others)
      accumulator = { count: 0, value: nil }
      previous_low = 0
      previous_high = 0
      partition.each_with_index do |entry, index|
        charge
        value = case name
                when 'row_number' then index + 1
                when 'rank' then entry[:peer_start] + 1
                when 'dense_rank' then entry[:peer] + 1
                when 'percent_rank' then count == 1 ? 0.0 : entry[:peer_start].to_f / (count - 1)
                when 'cume_dist' then entry[:peer_end].to_f / count
                when 'ntile'
                  if ntile
                    base, remainder = count.divmod(ntile)
                    threshold = (base + 1) * remainder
                    index < threshold ? index / (base + 1) + 1 : (index - threshold) / [base, 1].max + remainder + 1
                  end
                when 'lag', 'lead'
                  offset = args[1] ? evaluate(args[1], entry[:row], entry[:group]) : 1
                  if offset
                    target = index + (name == 'lead' ? offset : -offset)
                    if target.between?(0, count - 1)
                      cached_values[target]
                    elsif args[2]
                      evaluate(args[2], entry[:row], entry[:group])
                    end
                  end
                else
                  indices = window_frame_indices(frame, partition, index, spec[:order_by])
                  if name == 'first_value' || name == 'last_value' || name == 'nth_value'
                    nth = name == 'nth_value' ? evaluate(args[1], entry[:row], entry[:group]) : 1
                    raise Error.new('22016', 'nth_value argument must be greater than zero') if nth && nth <= 0
                    target = name == 'last_value' ? indices.last : nth && indices[nth - 1]
                    if target
                      cached_values[target]
                    end
                  else
                    if incremental
                      low = indices.first || 0
                      high = indices.empty? ? low : indices.last + 1
                      reset = low < previous_low || high < previous_high || (low != previous_low && name != 'count')
                      if reset
                        accumulator = { count: 0, value: nil }
                        previous_low = low
                        previous_high = low
                      end
                      if name == 'count'
                        previous_low.upto([low, previous_high].min - 1) do |position|
                          charge
                          accumulator[:count] -= 1 unless aggregate_rows[position][0].nil?
                        end
                      end
                      [previous_high, low].max.upto(high - 1) do |position|
                        window_accumulate(accumulator, aggregate_rows[position][0], name, expression.type)
                      end
                      previous_low, previous_high = low, high
                      window_accumulator_value(accumulator, name, expression.type)
                    else
                      aggregate(aggregate_expr, indices.map { |position| aggregate_rows[position] })
                    end
                  end
                end
        @window_values[[node.object_id, entry[:row].object_id, entry[:group]&.object_id]] = value
      end
    ensure
      query_memory_release(memory_owner) if @query_memory && memory_owner
    end

    def window_accumulate(state, value, name, type)
      charge
      return if value.nil?
      state[:count] += 1
      case name
      when 'count' then nil
      when 'sum', 'avg'
        number = Types.cast(value, type)
        state[:value] = state[:value].nil? ? number : Types.binary('+', state[:value], number, type: type)
      when 'min'
        state[:value] = value if state[:value].nil? || Types.compare(value, state[:value]).negative?
      when 'max'
        state[:value] = value if state[:value].nil? || Types.compare(value, state[:value]).positive?
      when 'bool_and', 'every'
        state[:value] = state[:value].nil? ? value : state[:value] && value
      when 'bool_or'
        state[:value] = state[:value].nil? ? value : state[:value] || value
      end
    end

    def window_accumulator_value(state, name, type)
      return state[:count] if name == 'count'
      return nil if state[:count].zero?
      if name == 'avg'
        Types.binary('/', state[:value], Types.cast(state[:count], type), type: type)
      else
        state[:value]
      end
    end

    def window_frame_indices(frame, partition, index, orders)
      low = window_frame_edge(frame[:start], false, frame[:mode], partition, index, orders)
      high = window_frame_edge(frame[:end], true, frame[:mode], partition, index, orders)
      low = [[low, 0].max, partition.length].min
      high = [[high, 0].max, partition.length].min
      return [] if low >= high
      indices = (low...high).to_a
      case frame[:exclude]
      when :current_row then indices.reject { |position| position == index }
      when :group then indices.reject { |position| partition[position][:peer] == partition[index][:peer] }
      when :ties then indices.reject { |position| position != index && partition[position][:peer] == partition[index][:peer] }
      else indices
      end
    end

    def window_frame_edge(bound, ending, mode, partition, index, orders)
      entry = partition[index]
      if bound[:kind] == :unbounded
        return bound[:direction] == :preceding ? 0 : partition.length
      end
      if mode == :rows
        shift = bound[:kind] == :current_row ? 0 : bound[:offset] * (bound[:direction] == :preceding ? -1 : 1)
        return index + shift + (ending ? 1 : 0)
      end
      if bound[:kind] == :current_row
        return ending ? entry[:peer_end] : entry[:peer_start]
      end
      if mode == :groups
        peer = entry[:peer] + bound[:offset] * (bound[:direction] == :preceding ? -1 : 1)
        starts = entry[:group_starts]
        return 0 if peer.negative?
        return partition.length if peer >= starts.length - 1
        return starts[peer + (ending ? 1 : 0)]
      end
      key = entry[:order].first
      return ending ? entry[:peer_end] : entry[:peer_start] if key.nil?
      order = orders.first
      descending = order[:direction] == :desc || order[:desc]
      sign = bound[:direction] == :preceding ? -1 : 1
      sign = -sign if descending
      offset = bound[:offset]
      threshold = sign.positive? ? key + offset : key - offset
      if ending
        position = partition.length
        partition.each_with_index do |candidate, candidate_index|
          comparison = order_compare(candidate[:order].first, threshold, order)
          if comparison.positive?
            position = candidate_index
            break
          end
        end
        position
      else
        position = partition.length
        partition.each_with_index do |candidate, candidate_index|
          comparison = order_compare(candidate[:order].first, threshold, order)
          if comparison >= 0
            position = candidate_index
            break
          end
        end
        position
      end
    end
  end

  Engine.prepend(WindowExecution)
end
