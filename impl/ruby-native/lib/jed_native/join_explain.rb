# frozen_string_literal: true

module JedNative
  module JoinExplain
    def explain_query(statement)
      @explain_join_plans = {}
      super
    ensure
      @explain_join_plans = nil
    end

    def plan_select(statement)
      sources, edges = statement[:from] || [], statement[:joins] || []
      return super unless sources.length == 1 && edges.length == 1 && !edges.first[:using] && !edges.first[:natural]
      return super unless [sources.first, edges.first[:relation]].all? { |source| source[:kind] == :table && !source[:lateral] && @tables.key?(source[:name]) && !@ctes.key?(source[:name]) }
      analyzer = ParameterBindings.new(self, nil)
      original_scope = analyzer.relation_columns(sources.first) + analyzer.relation_columns(edges.first[:relation])
      # This validation cannot execute subquery bodies during plain EXPLAIN.
      plan_compile(statement[:where], original_scope) if statement[:where]
      plan_compile(edges.first[:on], original_scope) if edges.first[:on]
      sources, edges, filter = rewrite_join_inputs(sources, edges, statement[:where]) || [sources, edges, statement[:where]]
      plan = if two_base_join?(sources, edges)
        two_base_join_plan(sources.first, edges.first, filter, statement)
      else
        fixed_join_plan(sources.first, edges.first, filter)
      end
      inputs = [plan[:outer], plan[:inner]]
      scans = inputs.each_with_index.map do |source, index|
        stored = @tables.fetch(source[:name])
        scope = analyzer.relation_columns(source)
        predicate = source[:native_access_predicate] && plan_compile(source[:native_access_predicate], scope)
        access = index == 1 && plan[:index] ? plan[:index][:access] : plan[:scans][index]
        node = plan_scan(stored, predicate, statement.merge(native_scan_source: source), access)
        node.phase = nil
        node.actual = 0
        if index == 1 && plan[:index]
          node.detail = 'Index-nested-loop ' + node.detail
          node.detail = node.detail.gsub(/outer\([^)]*\)/, 'join')
          node.rows *= plan[:scans][0][:output_rows]
          node.cost *= plan[:scans][0][:output_rows]
        end
        source[:native_scan_profile] = node
        if source[:native_pushed_filter]
          expression = plan_compile(source[:native_pushed_filter], scope)
          detail = @explain_options[:verbose] ? "filter=#{plan_expression(plan_compile(source[:native_pushed_filter], original_scope))}" : "filter:conjuncts=#{conjunctions(expression).length}"
          node.detail += '; ' + detail
          node.cost += node.rows * expression_work(expression)
          node.rows = access[:output_rows] * (index == 1 && plan[:index] ? plan[:scans][0][:output_rows] : 1)
          source[:native_filter_profile] = node
        end
        node
      end
      if plan[:index]
        plan[:index][:scan_profile] = inputs[1][:native_scan_profile]
        plan[:index][:filter_profile] = inputs[1][:native_filter_profile]
      end
      detail = edges.first[:kind].to_s.sub('_outer', '')
      detail += "; keys=#{plan[:keys].length}" if plan[:algorithm] == :hash
      if plan[:on]
        detail += @explain_options[:verbose] ? "; on=#{plan_expression(plan_compile(edges.first[:on], original_scope))}" : "; on:conjuncts=#{conjunctions(plan[:on]).length}"
      end
      detail += '; ordered: join pk ordered' if plan[:ordered]
      base_cost = plan[:cost] - plan[:output_count]
      base_cost -= plan[:join_rows] * expression_work(plan[:filter]) if plan[:filter]
      node = plan_node(plan[:algorithm] == :hash ? 'Hash Join' : 'Nested Loop', detail, plan[:join_rows], base_cost, scans)
      plan[:node], plan[:outer_node] = node, scans.first
      @explain_join_plans ||= {}
      @explain_join_plans[statement.object_id] = plan
      if plan[:filter]
        expression = plan[:filter]
        detail = @explain_options[:verbose] ? "filter=#{plan_expression(plan_compile(filter, original_scope))}" : "conjuncts=#{conjunctions(expression).length}"
        node = plan_node('Filter', detail, estimate_predicate(node.rows, expression), node.cost + node.rows * expression_work(expression), [node], :filter)
      end
      projections = expand_stars(statement[:columns], original_scope)
      outputs = projections.map { |projection| plan_compile(projection[:expr], original_scope) }
      raise Error.new('0A000', 'grouped join EXPLAIN requires aggregate plan frames') if outputs.any? { |expression| aggregate_expr?(expression) } || !(statement[:group_by] || []).empty?
      orders = statement[:order_by] || []
      unless orders.empty? || plan[:ordered]
        work = orders.sum do |order|
          expression = order[:expr]
          output = expression[:kind] == :literal && expression[:value].is_a?(Integer) || expression[:kind] == :column && !expression[:table] && projections.any? { |projection| (projection[:alias] || expression_name(projection[:expr])) == expression[:name] }
          output ? 0 : expression_work(plan_compile(expression, original_scope))
        end
        node = plan_node('Sort', "keys=#{orders.length}", node.rows, node.cost + node.rows * work, [node], :sort)
      end
      if statement[:limit] || statement[:offset]
        limit = statement[:limit] && plan_constant(plan_compile(statement[:limit], []))
        offset = statement[:offset] ? plan_constant(plan_compile(statement[:offset], [])) || 0 : 0
        rows = [node.rows - offset, 0].max
        rows = [rows, limit].min if limit
        detail = []
        detail << "limit=#{limit}" if limit
        detail << "offset=#{offset}" if statement[:offset]
        node = plan_node('Limit', detail.join(' '), rows, node.cost, [node], :root)
      end
      node.cost += node.rows * (1 + outputs.sum { |expression| expression_work(expression) })
      node.detail += "; output=[#{outputs.map { |expression| plan_expression(expression) }.join(', ')}]" if @explain_options[:verbose]
      node.lane = 'buffered'
      node
    end

    def fixed_join_plan(first, edge, where)
      second = edge[:relation]
      analyzer = ParameterBindings.new(self, nil)
      scopes = [first, second].map { |source| analyzer.relation_columns(source) }
      scope = scopes.flatten
      scans = [first, second].each_with_index.map do |source, index|
        join_scan_candidates(source, scopes[index]).min_by { |access| [access[:pipeline_cost], AccessPlanning::ACCESS_RANK.fetch(access[:kind]), access[:name].to_s.b] }
      end
      on = edge[:on] && plan_compile(edge[:on], scope)
      gate = second[:native_original_on] ? plan_compile(second[:native_original_on], scope) : on
      filter = where && plan_compile(where, scope)
      left, right = scans.map { |scan| scan[:output_rows] }
      pairs = left * right
      rows = estimate_predicate(pairs, on)
      kind = edge[:kind]
      rows = [rows, left].max if %i[left left_outer full full_outer].include?(kind)
      rows = [rows, right].max if %i[right right_outer full full_outer].include?(kind)
      algorithm, keys, index = :nested, nil, nil
      work = pairs * expression_work(on)
      if %i[left left_outer].include?(kind)
        index = join_index_plan(Relation.new(columns: scopes[0], rows: []), second, kind, edge[:on], nil, choose: false)
        if index
          algorithm = :index
          rows = [left, left * index[:access][:output_rows]].max
          work = left * (index[:access][:scan_cost] + index[:access][:scan_rows] * expression_work(index[:filter]) + index[:access][:output_rows] * expression_work(on)) - scans[1][:pipeline_cost]
        elsif (keys = hash_join_keys(gate, scopes[0].length))
          algorithm = :hash
          bytes = keys.sum { |_, _, type| FileFormat::STORAGE_WIDTH.fetch(type, 1) }
          matches = keys.reduce(pairs) { |count, _| (count + 199) / 200 }
          matches = [matches, left].max
          work = (left + right) * bytes + matches * (bytes + keys.length * 4 + expression_work(on))
        end
      end
      output = estimate_predicate(rows, filter)
      { outer: first, inner: second, algorithm: algorithm, keys: keys, index: index, scans: scans,
        on: on, filter: filter, rows: output, join_rows: rows, output_count: output,
        cost: scans.sum { |scan| scan[:pipeline_cost] } + work + rows * expression_work(filter) + output }
    end
  end
  Engine.prepend(JoinExplain)
end
