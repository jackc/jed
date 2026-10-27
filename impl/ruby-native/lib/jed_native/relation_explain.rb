# frozen_string_literal: true

module JedNative
  # Non-base relations are search fences. Their plans own recursively planned
  # bodies and resident catalog facts; building a plan never runs those bodies.
  module RelationExplain
    def explain_query(statement)
      @explain_rewritten_from = {}
      @explain_group_components = {}
      super
    ensure
      @explain_rewritten_from = nil
      @explain_group_components = nil
    end

    def plan_select(statement)
      sources, joins = statement[:from] || [], statement[:joins] || []
      windows = !window_nodes([statement[:columns], statement[:order_by]]).empty?
      return super if nway_join_shape?(sources, joins)
      base = lambda { |source| source[:kind] == :table && @tables.key?(source[:name]) && !@ctes.key?(source[:name]) }
      return super if sources.empty? || !windows && sources.length == 1 && base.call(sources.first) &&
        (joins.empty? || joins.length == 1 && !joins.first[:using] && !joins.first[:natural] && base.call(joins.first[:relation]))
      original = sources + joins.map { |edge| edge[:relation] }
      analyzer = ParameterBindings.new(self, nil)
      outer = @outer.flat_map(&:first)
      scope = query_input_columns(statement, @ctes.transform_values(&:columns), outer, analyzer)
      scope = scope.take(scope.length - outer.length)
      rewritten = rewrite_join_inputs(sources, joins, statement[:where])
      sources, joins, predicate = rewritten || [sources, joins, statement[:where]]
      @explain_rewritten_from ||= {}
      @explain_rewritten_from[statement.object_id] = [sources, joins, predicate]
      relation_nodes = []
      prefix = []
      node = nil
      sources.each_with_index do |source, index|
        grouped = joins.select { |edge| (edge[:from_index] || 0) == index }
        if sources.length > 1 && !grouped.empty?
          original_id = source.object_id
          source = { kind: :join_group, from: [source], joins: grouped.map { |edge| edge.merge(from_index: 0) },
                     native_inline_group: index.zero? || grouped.all? { |edge| %i[inner cross].include?(edge[:kind]) } }
          @explain_group_components[original_id] = source if @explain_group_components
          grouped = []
        end
        current = query_source_columns(source, @ctes.transform_values(&:columns), outer + prefix, analyzer)
        previous = @outer
        @outer = @outer + [[prefix, Array.new(prefix.length)]] if lateral_source_dependency?(source, prefix)
        begin
          right = plan_relation(source, statement, scope)
        ensure
          @outer = previous
        end
        if node
          node = plan_relation_join(node, right, source, { kind: :cross }, prefix, current, statement)
        else
          node = right
        end
        prefix += current
        grouped.each do |edge|
          source = edge[:relation]
          current = query_source_columns(source, @ctes.transform_values(&:columns), outer + prefix, analyzer)
          previous = @outer
          @outer = @outer + [[prefix, Array.new(prefix.length)]] if lateral_source_dependency?(source, prefix)
          begin
            right = plan_relation(source, statement, scope)
          ensure
            @outer = previous
          end
          node = plan_relation_join(node, right, source, edge, prefix, current, statement)
          prefix += current
        end
      end
      if predicate
        expression = plan_filter(predicate, scope)
        detail = @explain_options[:verbose] ? "filter=#{plan_expression(expression)}" : "conjuncts=#{conjunctions(expression).length}"
        node = plan_node('Filter', detail, estimate_predicate(node.rows, expression), node.cost + node.rows * expression_work(expression), [node], :filter)
      end
      join_plan_output(node, statement, scope, false)
    end

    def plan_relation(source, statement, scope)
      analyzer = ParameterBindings.new(self, nil)
      columns = query_source_columns(source, @ctes.transform_values(&:columns), @outer.flat_map(&:first), analyzer)
      node = case source[:kind]
      when :subquery, :derived
        query = source[:query]
        child = build_query_plan(query)
        label = 'Subquery' + (source[:alias] ? " #{source[:alias]}" : '')
        plan_node(label, '-', child.rows, child.cost, [child])
      when :join_group
        query = { kind: :select, from: source[:from], joins: source[:joins], columns: [{ expr: { kind: :star } }] }
        source[:native_group_statement] = query
        child = plan_select(query)
        child.cost -= child.rows if source[:native_inline_group]
        label = 'Subquery' + (source[:alias] ? " #{source[:alias]}" : '')
        plan_node(label, '-', child.rows, child.cost, [child])
      when :function, :call, :srf
        call = source[:expr] || source[:call] || source
        args = (call[:args] || []).map { |arg| plan_compile(arg, []) }
        count = 1000
        if call[:name] == 'generate_series'
          first, last, step = args.map { |arg| plan_constant(arg) }
          step ||= 1
          if [first, last, step].all? { |value| value.is_a?(Integer) }
            raise Error.new('22023', 'series step cannot be zero') if step.zero?
            count = (last - first) * step < 0 ? 0 : (last - first) / step + 1
          elsif first.nil? || last.nil? || step.nil?
            count = 0
          end
        end
        work = args.sum { |arg| expression_work(arg) } + (call[:name] == 'generate_series' ? 0 : 1)
        plan_node("SRF #{call[:name]}", '-', count, count + work)
      when :table
        name = source[:name]
        if introspection_relation?(name)
          database = introspection_scope(source[:database])
          count = introspection_rows(name, database).length
          plan_node("Catalog Scan #{name}", "db=#{database}", count, count)
        elsif (view = @views[name])
          child = build_query_plan(source[:native_body_query] || view[:query])
          plan_node("Subquery #{source[:alias] || name}", '-', child.rows, child.cost, [child])
        elsif (binding = @ctes[name])
          if binding.respond_to?(:query) && binding.query
            child = build_query_plan(source[:native_body_query] || binding.query)
            plan_node("CTE Scan #{name}", '-', child.rows, child.cost, [child])
          else
            plan_node("CTE Scan #{name}", '-', binding.rows.length, binding.rows.length)
          end
        else
          predicate = source[:native_access_predicate] && plan_filter(source[:native_access_predicate], columns)
          plan_scan(@tables.fetch(name), predicate, statement.merge(native_scan_source: source))
        end
      else raise Error.new('0A000', "relation #{source[:kind]} has no plan")
      end
      if source[:native_pushed_filter]
        filter = plan_filter(source[:native_pushed_filter], columns)
        detail = @explain_options[:verbose] ? "filter=#{plan_expression(plan_filter(source[:native_pushed_filter], scope))}" : "filter:conjuncts=#{conjunctions(filter).length}"
        node.detail = node.detail == '-' ? detail : node.detail + '; ' + detail
        node.cost += node.rows * expression_work(filter)
        node.rows = estimate_predicate(node.rows, filter)
        source[:native_filter_profile] = node
      end
      node.phase, node.actual = nil, 0
      source[:native_scan_profile] = node
      node
    end

    private

    def plan_relation_join(left, right, source, edge, prefix, columns, statement)
      scope = prefix + columns
      on = edge[:on] && plan_filter(edge[:on], scope)
      original = source[:native_original_on] || edge[:on]
      gate = original && plan_compile(original, scope)
      kind = edge[:kind]
      lateral = lateral_source_dependency?(source, prefix)
      index = !lateral && join_index_plan(Relation.new(columns: prefix, rows: []), source, kind, edge[:on], edge[:using], choose: false)
      keys = !index && !lateral && %i[inner left left_outer].include?(kind) && hash_join_keys(gate, prefix.length)
      left_rows, right_rows = left.rows, right.rows
      if index
        right = plan_scan(index[:stored], index[:predicate], statement, index[:access])
        right.detail = 'Index-nested-loop ' + right.detail.gsub(/outer\([^)]*\)/, 'join')
      end
      if index || lateral
        scale_relation_plan(right, left_rows)
        pairs = right.rows
      else
        pairs = left_rows * right_rows
      end
      rows = index ? pairs : estimate_predicate(pairs, on)
      rows = [rows, left_rows].max if %i[left left_outer full full_outer].include?(kind)
      rows = [rows, right_rows].max if %i[right right_outer full full_outer].include?(kind)
      cost = left.cost + right.cost
      if keys
        bytes = keys.sum { |_, _, type| FileFormat::STORAGE_WIDTH.fetch(type, 1) }
        cost += (left_rows + right_rows) * bytes + rows * (bytes + keys.length * 4 + expression_work(on))
      else
        cost += pairs * expression_work(on)
      end
      detail = kind.to_s.sub('_outer', '')
      detail += "; keys=#{keys.length}" if keys
      detail += @explain_options[:verbose] ? "; on=#{plan_expression(on)}" : "; on:conjuncts=#{conjunctions(on).length}" if on
      node = plan_node(keys ? 'Hash Join' : 'Nested Loop', detail, rows, cost, [left, right])
      source[:native_join_profile], source[:native_join_left_node] = node, left
      source[:native_index_plan] = index
      source[:native_join_algorithm] = index ? :index : keys ? :hash : :nested
      if index
        index[:scan_profile] = right
        right.phase, right.actual = nil, 0
      end
      node
    end

    def scale_relation_plan(node, count)
      node.rows *= count
      node.cost *= count
      node.children.each { |child| scale_relation_plan(child, count) }
    end
  end
  Engine.prepend(RelationExplain)
end
