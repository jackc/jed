# frozen_string_literal: true

module JedNative
  module NWayExplain
    def explain_query(statement)
      @explain_nway_plans = {}
      super
    ensure
      @explain_nway_plans = nil
    end

    def plan_select(statement)
      sources, edges = statement[:from] || [], statement[:joins] || []
      return super unless nway_join_shape?(sources, edges)
      sources, edges, predicate = rewrite_join_inputs(sources, edges, statement[:where]) || [sources, edges, statement[:where]]
      plan = nway_join_plan(sources, edges, predicate, statement)
      nodes = plan[:order].each_with_index.map do |ordinal, position|
        source = plan[:sources][ordinal]
        access = plan[:accesses][position]
        local = source[:native_access_predicate] && plan_filter(source[:native_access_predicate], plan[:scopes][ordinal])
        node = plan_scan(@tables.fetch(source[:name]), local, statement.merge(native_scan_source: source), access)
        node.phase, node.actual = nil, 0
        node.rows, node.cost = access[:output_rows], access[:pipeline_cost]
        if source[:native_pushed_filter]
          local = plan_filter(source[:native_pushed_filter], plan[:scopes][ordinal])
          node.detail += @explain_options[:verbose] ? "; filter=#{plan_expression(plan_filter(source[:native_pushed_filter], plan[:scope]))}" : "; filter:conjuncts=#{conjunctions(local).length}"
        end
        source[:native_scan_profile] = node
        if position.positive? && (step = plan[:steps][position - 1])[:algorithm] == :index
          node.detail = 'Index-nested-loop ' + node.detail.gsub(/outer\([^)]*\)/, 'join')
          calls = step[:calls] || step[:input_rows]
          node.rows *= calls
          node.cost *= calls
          step[:index][:scan_profile] = node
        end
        node
      end
      node = nodes.first
      plan[:steps].each_with_index do |step, index|
        detail = step[:kind].to_s.sub('_outer', '')
        detail += "; keys=#{step[:keys].length}" if step[:algorithm] == :hash
        expressions = step[:ons].map { |ast| plan_compile(ast, plan[:scope]) }
        unless expressions.empty?
          detail += if @explain_options[:verbose]
            '; on=' + expressions.map { |expression| plan_expression(expression) }.join(' and ')
          else
            "; on:conjuncts=#{expressions.sum { |expression| conjunctions(expression).length }}"
          end
        end
        detail += '; ordered: join pk ordered' if plan[:ordered] && index == plan[:steps].length - 1
        node = plan_node(step[:algorithm] == :hash ? 'Hash Join' : 'Nested Loop', detail, step[:rows], step[:cost], [node, nodes[index + 1]])
        step[:node] = node
      end
      @explain_nway_plans ||= {}
      @explain_nway_plans[statement.object_id] = plan
      if predicate
        expression = plan_filter(predicate, plan[:scope])
        detail = @explain_options[:verbose] ? "filter=#{plan_expression(expression)}" : "conjuncts=#{conjunctions(expression).length}"
        rows = [node.rows, estimate_predicate(plan[:logical], expression)].min
        node = plan_node('Filter', detail, rows, node.cost + node.rows * expression_work(expression), [node], :filter)
      end
      join_plan_output(node, statement, plan[:scope], plan[:ordered])
    end

    private

    def join_plan_output(node, statement, scope, ordered)
      projections = expand_stars(statement[:columns], scope)
      outputs = projections.map { |projection| plan_compile(projection[:expr], scope) }
      group_nodes = resolve_group_nodes(statement, scope)
      groups = group_nodes.map { |expression| plan_compile(expression, scope) }
      calls = outputs.flat_map { |expression| expression_nodes(expression) }.select { |expression| expression.kind == :call && Functions.aggregate?(expression.value) }.uniq { |expression| [expression.value, expression.args] }
      having = statement[:having] && plan_filter(statement[:having], scope)
      if !groups.empty? || !calls.empty? || having
        rows = groups.empty? ? 1 : estimate_distinct_rows(node.rows, groups)
        cost = node.cost + node.rows * (calls.length + groups.sum { |expression| expression_work(expression) } + calls.sum { |expression| expression.args.sum { |argument| expression_work(argument) } })
        detail = "groups=#{groups.length} aggs=#{calls.length}"
        if having
          cost += rows * expression_work(having)
          rows = estimate_predicate(rows, having)
          detail += @explain_options[:verbose] ? "; having=#{plan_expression(having)}" : "; having:conjuncts=#{conjunctions(having).length}"
        end
        node = plan_node('Aggregate', detail, rows, cost, [node], :aggregate)
      end
      distinct_on = statement[:distinct_on] && plan_distinct_on_keys(statement, projections, outputs, scope)
      ordered = false if distinct_on
      node = plan_node('Distinct', '-', estimate_distinct_rows(node.rows, outputs), node.cost, [node], :distinct) if statement[:distinct] && !distinct_on
      orders = statement[:order_by] || []
      unless (orders.empty? && !distinct_on) || ordered
        work = orders.sum do |order|
          expression = order[:expr]
          output = expression[:kind] == :literal && expression[:value].is_a?(Integer) || expression[:kind] == :column && !expression[:table] && projections.any? { |projection| (projection[:alias] || expression_name(projection[:expr])) == expression[:name] }
          output ? 0 : expression_work(plan_compile(expression, scope))
        end
        work += distinct_on[:work] if distinct_on
        node = plan_node('Sort', "keys=#{orders.length + (distinct_on ? distinct_on[:appended] : 0)}", node.rows, node.cost + node.rows * work, [node], :sort)
      end
      node = plan_node('Distinct', "on=#{distinct_on[:keys].length}", estimate_distinct_rows(node.rows, distinct_on[:keys]), node.cost, [node], :distinct) if distinct_on
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
  end
  Engine.prepend(NWayExplain)
end
