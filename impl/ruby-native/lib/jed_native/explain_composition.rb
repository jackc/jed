# frozen_string_literal: true

module JedNative
  module ExplainComposition
    def explain_query(statement)
      @explain_cte_definitions, @explain_cte_references, @explain_cte_main = {}, [], {}
      super
    ensure
      @explain_cte_definitions = @explain_cte_references = @explain_cte_main = nil
    end

    def build_query_plan(statement)
      node = if !(statement[:with] || []).empty?
        plan_with(statement)
      elsif %i[setop set_operation].include?(statement[:kind])
        left, right = build_query_plan(statement[:left]), build_query_plan(statement[:right])
        columns = query_metadata(statement)[:columns]
        count = statement[:op] == 'union' ? left.rows + right.rows : statement[:op] == 'intersect' ? [left.rows, right.rows].min : (left.rows + 1) / 2
        count = [count, 200**columns.length].min unless statement[:all]
        result = plan_node(statement[:op].capitalize, statement[:all] ? 'all' : 'distinct', count, left.cost + right.cost, [left, right], :combine)
        plan_composition_tail(result, statement).tap { |root| root.lane = 'deferred' }
      elsif statement[:kind] == :values
        expressions = statement[:rows].flatten.map { |expression| plan_compile(expression, []) }
        count = statement[:rows].length
        result = plan_node('Values', "rows=#{count}", count, count + expressions.sum { |expression| expression_work(expression) }, [], :values)
        if !(statement[:order_by] || []).empty? || statement[:limit] || statement[:offset]
          result = plan_node('Subquery *VALUES*', '-', count, result.cost, [result], :scan)
          result = plan_composition_tail(result, statement)
          result.cost += result.rows
        end
        result
      else
        super
      end
      plan_nodes(node).each do |child|
        child.instance_variable_set(:@native_plan_owner, statement.object_id) unless child.instance_variable_defined?(:@native_plan_owner)
        child.actual ||= 0
      end
      if statement[:kind] == :select && statement[:limit] && (statement[:group_by] || []).empty? && !statement[:distinct] && !contains_aggregate?([statement[:columns], statement[:having]]) && window_nodes([statement[:columns], statement[:order_by]]).empty?
        limit = plan_constant(plan_compile(statement[:limit], []))
        offset = statement[:offset] ? plan_constant(plan_compile(statement[:offset], [])) : 0
        if limit.is_a?(Integer) && offset.is_a?(Integer) && limit + offset <= QueryPlans::MAX_ESTIMATE
          owned_plan_nodes(node).each do |child|
            child.detail = child.detail.sub(/\Akeys=\d+/) { |detail| "#{detail}, top-k=#{limit.zero? ? 0 : limit + offset}" } if child.label == 'Sort' && !child.detail.include?('top-k=')
          end
        end
      end
      @explain_plans[statement.object_id] = node
      node
    end

    def plan_select(statement)
      sources, joins = statement[:from] || [], statement[:joins] || []
      grouped = !(statement[:group_by] || []).empty? || contains_aggregate?([statement[:columns], statement[:having]])
      windows = !window_nodes([statement[:columns], statement[:order_by]]).empty?
      return super unless (grouped || windows) && joins.empty? && (sources.empty? || sources.length == 1 && sources[0][:kind] == :table && @tables.key?(sources[0][:name]) && !@ctes.key?(sources[0][:name]))
      source = sources.first
      stored = source && @tables.fetch(source[:name])
      scope = stored ? table_columns(stored, source[:alias] || source[:name]) : []
      predicate = statement[:where] && plan_compile(statement[:where], scope)
      node = stored ? plan_scan(stored, predicate, statement) : plan_node('Result', '-', 1, 0, [], :scan)
      if windows && stored && statement[:limit]
        limit = plan_constant(plan_compile(statement[:limit], []))
        offset = statement[:offset] ? plan_constant(plan_compile(statement[:offset], [])) : 0
        if limit.is_a?(Integer) && offset.is_a?(Integer)
          prefix = window_scan_prefix(statement.merge(limit: { kind: :literal, type: 'i64', value: limit }, offset: { kind: :literal, type: 'i64', value: offset }))
        end
      end
      if prefix
        node.rows = [node.rows, prefix[:count]].min
        node.cost = node.rows.zero? ? 0 : node.rows + [stored[:native_primary_metadata][:node_count], [node.rows, stored[:native_primary_metadata][:height]].max].min
        node.detail += '; ordered: pk ordered'
        node.detail += ' (reverse)' if prefix[:descending]
      end
      if predicate
        detail = @explain_options[:verbose] ? "filter=#{plan_expression(predicate)}" : "conjuncts=#{conjunctions(predicate).length}"
        node = plan_node('Filter', detail, estimate_predicate(node.rows, predicate), node.cost + node.rows * expression_work(predicate), [node], :filter)
      end
      join_plan_output(node, statement, scope, !!prefix)
    end

    def plan_relation(source, statement, scope)
      binding = source[:kind] == :table && @ctes[source[:name]]
      return super unless binding && binding.instance_variable_defined?(:@native_plan_rows)
      count = binding.instance_variable_get(:@native_plan_rows)
      if binding.instance_variable_get(:@native_plan_inline)
        query = plan_copy_query(source[:native_body_query] || binding.query)
        previous = @ctes
        begin
          @ctes = binding.ctes.dup
          child = build_query_plan(query)
        ensure
          @ctes = previous
        end
        node = plan_node("CTE Scan #{source[:name]}", '-', child.rows, child.cost, [child])
      else
        node = plan_node("CTE Scan #{source[:name]}", '-', count, count)
      end
      if source[:native_pushed_filter]
        columns = binding.columns.map { |column| column.merge(table: source[:alias] || source[:name]) }
        filter = plan_compile(source[:native_pushed_filter], columns)
        node.detail = @explain_options[:verbose] ? "filter=#{plan_expression(plan_compile(source[:native_pushed_filter], scope))}" : "filter:conjuncts=#{conjunctions(filter).length}"
        node.cost += node.rows * expression_work(filter)
        node.rows = estimate_predicate(node.rows, filter)
      end
      node.phase = nil
      @explain_cte_references << { name: source[:name], alias: source[:alias], query: query, original: source[:native_body_query] || binding.query, node: node }
      node
    end

    def materialize_ctes(statement)
      previous = @explain_materializing_statement
      @explain_materializing_statement = statement if @explain_runtime_plans
      super
      if @explain_runtime_plans && (main = @explain_cte_main&.[](statement.object_id))
        main[:start] = @cost
      end
    ensure
      @explain_materializing_statement = previous
    end

    def explain_cte_definition(name, before)
      return unless @explain_runtime_plans && @explain_materializing_statement
      definition = @explain_cte_definitions.dig(@explain_materializing_statement.object_id, name)
      definition.actual += @cost - before if definition
    end

    def explain_cte_source_query(source, binding)
      return source[:native_body_query] || binding.query unless @explain_runtime_plans
      original = source[:native_body_query] || binding.query
      reference = @explain_cte_references.find { |entry| entry[:name] == source[:name] && entry[:alias] == source[:alias] && entry[:original] == original }
      source[:native_cte_profile] = reference[:node] if reference
      reference&.dig(:query) || original
    end

    def source_relation(source)
      start = @cost if @explain_runtime_plans && source[:kind] == :table && @ctes[source[:name]]
      result = super
      if start && (node = source[:native_cte_profile])
        node.actual += @cost - start
      end
      result
    end

    def profile_checkpoint(phase)
      frame = @explain_profile_frames&.last
      return unless frame
      owner = frame[:plan].instance_variable_get(:@native_plan_owner)
      main = @explain_cte_main&.[](owner)
      root, start = main && main[:start] ? [main[:plan], main[:start]] : [frame[:plan], frame[:start]]
      nodes = owned_plan_nodes(root)
      nodes.each { |node| node.actual = @cost - start if node.phase == phase }
      root.actual = @cost - start if phase == :root
    end

    def profile_accumulate(phase, amount)
      frame = @explain_profile_frames&.last
      return unless frame
      main = @explain_cte_main&.[](frame[:plan].instance_variable_get(:@native_plan_owner))
      root = main && main[:start] ? main[:plan] : frame[:plan]
      owned_plan_nodes(root).each { |node| node.actual = (node.actual || 0) + amount if node.phase == phase }
    end

    def profile_inherit(phase, child_phase)
      frame = @explain_profile_frames&.last
      return unless frame
      main = @explain_cte_main&.[](frame[:plan].instance_variable_get(:@native_plan_owner))
      root = main && main[:start] ? main[:plan] : frame[:plan]
      child = owned_plan_nodes(root).find { |node| node.phase == child_phase }
      profile_accumulate(phase, child&.actual || 0)
    end

    private

    def owned_plan_nodes(root)
      owner = root.instance_variable_get(:@native_plan_owner)
      plan_nodes(root).select { |node| node.instance_variable_get(:@native_plan_owner) == owner }
    end

    def plan_copy_query(node)
      case node
      when Hash then node.transform_values { |child| plan_copy_query(child) }
      when Array then node.map { |child| plan_copy_query(child) }
      else node
      end
    end

    def plan_with(statement)
      original_ctes = @ctes
      @ctes = @ctes.dup
      items = statement[:with]
      main_statement = statement.reject { |key, _| %i[with recursive].include?(key) }
      definitions, contribution = [], 0
      @explain_cte_definitions[statement.object_id] = {}
      items.each_with_index do |cte, index|
        body, name = cte[:query], cte[:name]
        recursive = statement[:recursive] && cte_reference_count(body, name).positive?
        if recursive
          seed_columns = query_metadata(body[:left])[:columns]
          seed_columns = seed_columns.each_with_index.map { |column, ordinal| column.merge(name: cte[:columns]&.[](ordinal) || column[:name]) }
          seed = CTEs::CTEBinding.new(columns: seed_columns, rows: [])
          seed.instance_variable_set(:@native_plan_rows, 0)
          @ctes[name] = seed
        end
        columns = query_metadata(body, @outer.flat_map(&:first))[:columns]
        columns = columns.each_with_index.map { |column, ordinal| column.merge(name: cte[:columns]&.[](ordinal) || column[:name]) }
        count = cte_reference_count(main_statement, name)
        mutation_reader = %i[insert update delete merge].include?(main_statement[:kind]) && count.positive?
        items.drop(index + 1).each do |later|
          uses = cte_reference_count(later[:query], name)
          count += uses
          mutation_reader ||= %i[insert update delete merge].include?(later[:query][:kind]) && uses.positive?
        end
        modifying = %i[insert update delete merge].include?(body[:kind])
        materialized = recursive || modifying || cte[:materialized] == true || cte[:materialized].nil? && count > 1
        declaration = @ctes.dup
        child = build_query_plan(body)
        inline = !materialized && count.positive? && !mutation_reader
        binding = CTEs::CTEBinding.new(columns: columns, rows: [], query: materialized ? nil : body, ctes: declaration, outer: @outer.dup,
                                       native_body_pushable: !materialized && count == 1 && !mutation_reader)
        binding.instance_variable_set(:@native_plan_rows, child.rows)
        binding.instance_variable_set(:@native_plan_inline, inline)
        @ctes[name] = binding
        cost = materialized && (count.positive? || modifying) ? child.cost : 0
        definition = plan_node("CTE #{name}", materialized ? 'materialized' : 'inlined', child.rows, cost, inline ? [] : [child])
        definition.actual = 0
        @explain_cte_definitions[statement.object_id][name] = definition
        definitions << definition
        contribution += cost
        contribution += child.cost * count if mutation_reader && !materialized
      end
      main = build_query_plan(main_statement)
      @explain_rewritten_from[statement.object_id] = @explain_rewritten_from[main_statement.object_id] if @explain_rewritten_from&.key?(main_statement.object_id)
      @explain_cte_main[statement.object_id] = { plan: main, statement: main_statement }
      root = plan_node('WITH', "ctes=#{items.length}", main.rows, contribution + main.cost, definitions + [main])
      root.lane = writable_with?(statement) ? 'buffered' : 'deferred'
      root
    ensure
      @ctes = original_ctes
    end

    def plan_composition_tail(node, statement)
      orders = statement[:order_by] || []
      node = plan_node('Sort', "keys=#{orders.length}", node.rows, node.cost, [node], :sort) unless orders.empty?
      if statement[:limit] || statement[:offset]
        limit = statement[:limit] && plan_constant(plan_compile(statement[:limit], []))
        offset = statement[:offset] ? plan_constant(plan_compile(statement[:offset], [])) || 0 : 0
        count = [node.rows - offset, 0].max
        count = [count, limit].min if limit
        detail = []
        detail << "limit=#{limit}" if limit
        detail << "offset=#{offset}" if statement[:offset]
        node = plan_node('Limit', detail.join(' '), count, node.cost, [node], :root)
      end
      node
    end

    def join_plan_output(node, statement, scope, ordered)
      projections = expand_stars(statement[:columns], scope)
      outputs = projections.map { |projection| plan_compile(projection[:expr], scope) }
      group_nodes = resolve_group_nodes(statement, scope)
      sets = expand_grouping_sets(group_nodes)
      groups = sets.flatten(1).uniq.map { |expression| plan_compile(expression, scope) }
      calls = projection_aggregates([statement[:columns], statement[:having], statement[:order_by], statement[:windows]]).map { |expression| plan_compile(expression, scope) }
      having = statement[:having] && plan_compile(statement[:having], scope)
      grouped = !groups.empty? || !calls.empty? || having
      validate_aggregate_scope(statement, scope, grouped)
      if grouped
        rows = groups.empty? ? 1 : estimate_distinct_rows(node.rows, groups)
        rows *= sets.length
        cost = node.cost + node.rows * (calls.length + groups.sum { |expression| expression_work(expression) } + calls.sum { |expression| expression.args.sum { |argument| expression_work(argument) } })
        detail = "groups=#{groups.length} aggs=#{calls.length}"
        detail += "; sets=#{sets.length}" if sets.length > 1
        outputs = outputs.map { |expression| spill_rewrite(expression, groups, calls) }
        if having
          having = spill_rewrite(having, groups, calls)
          cost += rows * expression_work(having)
          rows = estimate_predicate(rows, having)
          detail += @explain_options[:verbose] ? "; having=#{plan_expression(having)}" : "; having:conjuncts=#{conjunctions(having).length}"
        end
        node = plan_node('Aggregate', detail, rows, cost, [node], :aggregate)
      end
      windows = window_nodes([statement[:columns], statement[:order_by]]).map { |expression| plan_compile(expression, scope) }
      unless windows.empty?
        base = grouped ? groups.length + calls.length : scope.length
        replace = lambda do |expression|
          if expression.extra.is_a?(Hash) && expression.extra[:window_id]
            index = windows.index { |window| window.extra[:window_id] == expression.extra[:window_id] }
            next Expression.new(kind: :column, type: expression.type, value: base + index) if index
          end
          copy = expression.dup
          copy.args = expression.args.map { |argument| argument.is_a?(Expression) ? replace.call(argument) : argument } if expression.args
          copy
        end
        outputs = outputs.map { |expression| replace.call(expression) }
        node = plan_node('Window', "funcs=#{windows.length}", node.rows, node.cost + node.rows * windows.length, [node], :window)
      end
      projection_work = outputs.sum { |expression| expression_work(expression) }
      distinct_on = statement[:distinct_on] && plan_distinct_on_keys(statement, projections, outputs, scope)
      ordered = false if distinct_on
      if statement[:distinct] && !distinct_on
        node = plan_node('Distinct', '-', estimate_distinct_rows(node.rows, outputs), node.cost + node.rows * projection_work, [node], :distinct)
        projection_work = 0
      end
      orders = statement[:order_by] || []
      unless (orders.empty? && !distinct_on) || ordered
        work = orders.sum do |order|
          expression = order[:expr]
          output = expression[:kind] == :literal && expression[:value].is_a?(Integer) || expression[:kind] == :column && !expression[:table] && projections.any? { |projection| (projection[:alias] || expression_name(projection[:expr])) == expression[:name] }
          output ? 0 : expression_work(plan_compile(expression, scope))
        end
        work += distinct_on[:work] if distinct_on
        detail = "keys=#{orders.length + (distinct_on ? distinct_on[:appended] : 0)}"
        if statement[:limit] && !grouped && windows.empty? && !statement[:distinct]
          limit = plan_constant(plan_compile(statement[:limit], []))
          offset = statement[:offset] ? plan_constant(plan_compile(statement[:offset], [])) || 0 : 0
          detail += ", top-k=#{limit.zero? ? 0 : limit + offset}" if limit.is_a?(Integer) && offset.is_a?(Integer) && limit + offset <= QueryPlans::MAX_ESTIMATE
        end
        node = plan_node('Sort', detail, node.rows, node.cost + node.rows * work, [node], :sort)
      end
      node = plan_node('Distinct', "on=#{distinct_on[:keys].length}", estimate_distinct_rows(node.rows, distinct_on[:keys]), node.cost, [node], :distinct) if distinct_on
      node = plan_composition_tail(node, statement.merge(order_by: []))
      node.cost += node.rows * (1 + projection_work)
      if @explain_options[:verbose]
        output = "output=[#{outputs.map { |expression| plan_expression(expression) }.join(', ')}]"
        node.detail = node.detail == '-' ? output : node.detail + '; ' + output
      end
      node.lane = 'buffered'
      node
    end
  end
  Engine.prepend(ExplainComposition)
end
