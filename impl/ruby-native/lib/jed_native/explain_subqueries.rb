# frozen_string_literal: true

module JedNative
  # A subquery remains symbolic during EXPLAIN. Its hidden plan supplies types
  # and scheduled work; only the ordinary executor may obtain its row values.
  module ExplainSubqueries
    def build_query_plan(statement)
      previous = @native_subquery_plan_frame
      frame = { statement: statement, hidden: {} }
      @native_subquery_plan_frame = frame
      root = super
      frame[:hidden].each_value do |entry|
        next if entry[:correlated]
        target = case entry[:clause]
                 when :where then owned_plan_nodes(root).find { |node| node.phase == :filter }
                 when :having then owned_plan_nodes(root).find { |node| node.phase == :aggregate }
                 end || root
        plan_add_hidden_cost(root, target, entry[:plan].cost)
      end
      root
    ensure
      @native_subquery_plan_frame = previous
    end

    def profile_checkpoint(phase)
      super
      frame = @explain_profile_frames&.last
      return unless phase == :scan && frame && frame[:hidden_subquery_work]
      owned_plan_nodes(frame[:plan]).each do |node|
        node.actual = [node.actual - frame[:hidden_subquery_work], 0].max if node.phase == :scan
      end
    end

    private

    def plan_compile(node, columns)
      previous = @native_planning_expression
      @native_planning_expression = true
      compile(node, columns)
    ensure
      @native_planning_expression = previous
    end

    def compile_query_expression(node, columns, row_fields: nil)
      return super unless @native_subquery_plan_frame
      query = node[:query]
      metadata = query_metadata(query, @outer.flat_map(&:first) + columns)
      kind = node[:kind]
      expected = row_fields ? row_fields.length : 1
      raise Error.new('42601', 'subquery column count does not match its operand') if kind != :exists && metadata[:columns].length != expected
      row_fields&.zip(metadata[:columns])&.each do |left, right|
        validate_comparison(left, Expression.new(kind: :source, type: right[:type]))
      end
      type = row_fields ? 'record' : kind == :exists ? 'boolean' : metadata[:columns].first[:type]
      type = 'text' if type == 'unknown'
      type += '[]' if kind == :array_query
      body = plan_subquery_body(node, query, columns, metadata[:correlated])
      Expression.new(kind: :query_value, type: type, value: query,
                     extra: { scope: columns, query_kind: kind, row_value: !!row_fields,
                              negated: node[:negated], correlated: metadata[:correlated],
                              native_subplan: body, subquery_origin: true })
    end

    def compile_query_membership(node, columns, quantified)
      return super unless @native_subquery_plan_frame
      left_node = quantified ? node[:left] : node[:expr] || node[:left]
      left = compile(left_node, columns)
      query = quantified ? node[:right][:query] : node[:query]
      metadata = query_metadata(query, @outer.flat_map(&:first) + columns)
      row_value = left_node[:kind] == :row
      fields = row_value ? left.args : [left]
      raise Error.new('42601', 'subquery column count does not match its operand') unless fields.length == metadata[:columns].length
      begin
        fields.zip(metadata[:columns]).each { |field, column| validate_comparison(field, Expression.new(kind: :source, type: column[:type])) }
      rescue Error => error
        raise Error.new('42883', 'quantified comparison has incompatible operands') if quantified && error.sqlstate == '42804'
        raise
      end
      body = plan_subquery_body(node, query, columns, metadata[:correlated])
      Expression.new(kind: :query_membership, type: 'boolean', value: query, args: [left],
                     extra: { scope: columns, correlated: metadata[:correlated], rows: nil, row_value: row_value,
                              quantified: quantified, quantifier: quantified ? node[:quantifier] : 'any',
                              op: quantified ? node[:op] : '=', negated: node[:negated],
                              native_subplan: body, subquery_origin: true })
    end

    def plan_subquery_body(site, query, columns, correlated)
      frame = @native_subquery_plan_frame
      return frame[:hidden][query.object_id][:plan] if frame[:hidden].key?(query.object_id)
      previous = @outer
      @outer = @outer + [[columns, Array.new(columns.length)]]
      body = build_query_plan(query)
      clause = %i[where having].find { |key| plan_contains_site?(frame[:statement][key], site) }
      frame[:hidden][query.object_id] = { plan: body, clause: clause, correlated: correlated }
      body
    ensure
      @outer = previous if previous
    end

    def plan_contains_site?(tree, site)
      return true if tree.equal?(site)
      return tree.any? { |child| plan_contains_site?(child, site) } if tree.is_a?(Array)
      return tree.any? { |key, child| key != :query && plan_contains_site?(child, site) } if tree.is_a?(Hash)
      false
    end

    def plan_add_hidden_cost(node, target, cost)
      found = node.equal?(target)
      node.children.each { |child| found = plan_add_hidden_cost(child, target, cost) || found }
      node.cost = [QueryPlans::MAX_ESTIMATE, node.cost + cost].min if found
      found
    end

    def expression_work(expression)
      result = super
      if expression.is_a?(Expression) && expression.extra.is_a?(Hash) && expression.extra[:native_subplan] && expression.extra[:correlated]
        result += expression.extra[:native_subplan].cost
      end
      result
    end

    def plan_expression(expression)
      case expression.kind
      when :numeric_context then plan_expression(expression.args.first)
      when :query_value
        case expression.extra[:query_kind]
        when :exists then "#{expression.extra[:negated] ? 'not ' : ''}exists(<subquery>)"
        when :array_query then 'array(<subquery>)'
        else 'scalar(<subquery>)'
        end
      when :query_membership
        left = plan_expression(expression.args.first)
        data = expression.extra
        data[:quantified] ? "#{left} #{data[:op]} #{data[:quantifier]}(<subquery>)" : "#{left} #{data[:negated] ? 'not ' : ''}in (<subquery>)"
      when :row_query_compare then "(#{plan_expression(expression.args[0])} #{expression.value} #{plan_expression(expression.args[1])})"
      else super
      end
    end

    def estimate_predicate(rows, expression)
      if expression&.kind == :query_membership && expression.extra[:native_subplan] && interval_set_source?(expression)
        return (rows + 2) / 3
      end
      super
    end

    def hoisted_query(query)
      frame = @explain_profile_frames&.last
      before = @cost
      result = super
      frame[:hidden_subquery_work] = (frame[:hidden_subquery_work] || 0) + @cost - before if frame
      result
    end
  end
  Engine.prepend(ExplainSubqueries)
end
