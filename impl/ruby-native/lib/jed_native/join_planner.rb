# frozen_string_literal: true

module JedNative
  # Both two-base INNER orientations compete using resident catalog facts.
  # Physical order changes never change the logical column slots returned to
  # expression resolution.
  module JoinPlanner
    JOIN_RANK = { index: 0, hash: 1, nested: 2 }.freeze

    def materialize_from(sources, joins, predicate = nil)
      plan = @explain_runtime_plans && @explain_join_plans&.[](@native_scan_statement&.object_id)
      return super unless plan || two_base_join?(sources, joins)
      plan ||= two_base_join_plan(sources.first, joins.first, predicate, @native_scan_statement)
      left = source_relation(plan[:outer])
      inner = plan[:inner].merge(native_join_algorithm: plan[:algorithm], native_index_plan: plan[:index])
      if plan[:ordered]
        scope = left.columns + ParameterBindings.new(self, nil).relation_columns(inner)
        inner = inner.merge(native_join_window: { target: plan[:target], filter: predicate && compile_filter(predicate, scope) })
      end
      join_start = @cost if plan[:node]
      result = join(left, inner, joins.first[:kind], joins.first[:on], nil)
      plan[:node].actual = (plan[:outer_node].actual || 0) + @cost - join_start if plan[:node]
      if plan[:reverse]
        width = left.columns.length
        order = (width...result.columns.length).to_a + (0...width).to_a
        reordered = Relation.new(columns: order.map { |index| result.columns[index] }, rows: result.rows.map { |row| order.map { |index| row[index] } })
        query_memory_transfer(result, reordered) if @query_memory
        result = reordered
      end
      [result, plan[:ordered] ? nil : predicate]
    end

    def two_base_join?(sources, joins)
      return false unless sources.length == 1 && joins.length == 1 && %i[inner cross].include?(joins.first[:kind])
      return false if joins.first[:using] || joins.first[:natural]
      [sources.first, joins.first[:relation]].all? do |source|
        source && source[:kind] == :table && !source[:lateral] && @tables.key?(source[:name]) && !@ctes.key?(source[:name])
      end
    end

    def two_base_join_plan(first, edge, where, statement)
      second = edge[:relation]
      sources = [first, second]
      analyzer = ParameterBindings.new(self, nil)
      scopes = sources.map { |source| analyzer.relation_columns(source) }
      labels = sources.map { |source| source[:alias] || source[:name] }
      raise Error.new('42712', 'table name specified more than once') unless labels.uniq.length == labels.length
      scans = sources.each_with_index.map do |source, index|
        predicate = source[:native_access_predicate] && compile_filter(source[:native_access_predicate], scopes[index])
        scan_access_plan(@tables.fetch(source[:name]), predicate)
      end
      candidates = []
      [[0, 1], [1, 0]].each do |outer_index, inner_index|
        outer, inner = sources[outer_index], sources[inner_index]
        scope = scopes[outer_index] + scopes[inner_index]
        on = edge[:on] && compile_filter(edge[:on], scope)
        original = second[:native_original_on] || edge[:on]
        gate = original && compile_filter(original, scope)
        filter = where && compile_filter(where, scope)
        outer_rows, inner_rows = scans[outer_index][:output_rows], scans[inner_index][:output_rows]
        pairs = [outer_rows * inner_rows, QueryPlans::MAX_ESTIMATE].min
        joined_rows = estimate_predicate(pairs, on)
        output_rows = estimate_predicate(joined_rows, filter)
        ordered = join_driver_ordered?(statement, outer, scans[outer_index])
        target = ordered ? join_window_target(statement) : nil
        starts = if ordered && target.zero?
          0
        elsif ordered && output_rows > target
          [outer_rows, (target * outer_rows + output_rows - 1) / output_rows].min
        else outer_rows
        end
        visited = starts * inner_rows
        residual_work = expression_work(filter)
        output_work = ordered ? [output_rows, target].min : output_rows
        common = scans[outer_index][:pipeline_cost] + scans[inner_index][:pipeline_cost] +
          joined_rows * residual_work + output_work
        nested = common + visited * expression_work(on)
        base = { reverse: outer_index == 1, outer: outer.merge(native_access_plan: scans[outer_index]),
                 inner: inner.merge(native_access_plan: scans[inner_index], native_original_on: original, native_join_where: second[:native_join_where]),
                 ordered: ordered, target: target, rows: output_rows, join_rows: estimate_predicate(visited, on), output_count: output_work, scans: [scans[outer_index], scans[inner_index]],
                 on: on, filter: filter, sources: [outer, inner] }
        candidates << base.merge(algorithm: :nested, cost: nested)
        if (keys = hash_join_keys(gate, scopes[outer_index].length))
          bytes = keys.sum { |_, _, type| FileFormat::STORAGE_WIDTH.fetch(type, 1) }
          matches = keys.reduce(visited) { |count, _| (count + 199) / 200 }
          cost = common + (inner_rows + starts) * bytes + matches * (bytes + 4 * keys.length + expression_work(on))
          candidates << base.merge(algorithm: :hash, cost: cost, keys: keys)
        end
        metadata = Relation.new(columns: scopes[outer_index], rows: [])
        if (index = join_index_plan(metadata, base[:inner], edge[:kind], edge[:on], nil, choose: false))
          scan = index[:access]
          local_work = expression_work(index[:filter])
          inl_on = join_unbound_terms(on, index, scopes[outer_index].length)
          inl_where = join_unbound_terms(filter, index, scopes[outer_index].length)
          inl_pairs = outer_rows * scan[:output_rows]
          inl_joined = estimate_predicate(inl_pairs, inl_on)
          inl_output = estimate_predicate(inl_joined, inl_where)
          inl_starts = if ordered && target.zero?
            0
          elsif ordered && inl_output > target
            [outer_rows, (target * outer_rows + inl_output - 1) / inl_output].min
          else outer_rows
          end
          filtered_pairs = estimate_predicate(inl_starts * scan[:scan_rows], inl_on)
          cost = scans[outer_index][:pipeline_cost] + inl_starts * (scan[:scan_cost] + scan[:scan_rows] * (local_work + expression_work(on))) +
            filtered_pairs * residual_work + (ordered ? [inl_output, target].min : inl_output)
          candidates << base.merge(algorithm: :index, cost: cost, index: index, rows: inl_output, join_rows: filtered_pairs, output_count: ordered ? [inl_output, target].min : inl_output)
        end
      end
      candidates.min_by { |candidate| [[candidate[:cost], QueryPlans::MAX_ESTIMATE].min, candidate[:reverse] ? 1 : 0, JOIN_RANK.fetch(candidate[:algorithm])] }
    end

    private

    def join_unbound_terms(expression, index, width)
      return unless expression
      bound = (index[:access][:parts] || []).flat_map { |part| part[:terms] }
      remaining = conjunctions(expression).reject do |term|
        local = join_bound_expression(term, width)
        bound.any? { |used| same_key_expression?(local, used) }
      end
      remaining.reduce { |left, right| Expression.new(kind: :binary, type: 'boolean', value: 'and', args: [left, right]) }
    end

    def join_window_target(statement)
      limit = plan_constant(compile(statement[:limit], []))
      offset = statement[:offset] ? plan_constant(compile(statement[:offset], [])) : 0
      limit.zero? ? 0 : limit + (offset || 0)
    end

    def join_driver_ordered?(statement, source, access)
      return false unless statement && statement[:limit] && !statement[:distinct] && (statement[:group_by] || []).empty? && !contains_aggregate?(statement[:columns]) && !contains_window?(statement[:columns])
      return false unless %i[full primary primary_interval].include?(access[:kind])
      orders = statement[:order_by] || []
      return false if orders.empty?
      limit = plan_constant(compile(statement[:limit], []))
      offset = statement[:offset] ? plan_constant(compile(statement[:offset], [])) : 0
      return false unless limit.is_a?(Integer) && limit >= 0 && (offset.nil? || offset.is_a?(Integer) && offset >= 0)
      table = @tables.fetch(source[:name])
      keys = table[:native_primary_metadata][:columns]
      return false if keys.empty? || orders.length > keys.length
      orders.each_with_index.all? do |order, index|
        node = order[:expr]
        if node[:kind] == :literal && node[:value].is_a?(Integer)
          node = statement[:columns][node[:value] - 1]&.dig(:expr)
        elsif node[:kind] == :column && !node[:table]
          projection = statement[:columns].find { |entry| entry[:alias] == node[:name] }
          node = projection[:expr] if projection
        end
        node && node[:kind] == :column && node[:name] == table[:columns][keys[index]][:name] &&
          (!node[:table] || node[:table] == (source[:alias] || source[:name])) && order[:direction] != :desc
      end
    end
  end

  Engine.prepend(JoinPlanner)
end
