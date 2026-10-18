# frozen_string_literal: true

module JedNative
  # A plan is metadata: building one never scans row values or runs a SQL body.
  # Runtime checkpoints attach measured work to the same structural nodes.
  module QueryPlans
    MAX_ESTIMATE = (1 << 63) - 1
    Node = Struct.new(:label, :detail, :rows, :cost, :children, :phase, :actual, :lane, :access, keyword_init: true)

    def dispatch(statement)
      return super unless statement[:kind] == :explain
      explain_query(statement)
    end

    def profile_checkpoint(phase)
      frame = @explain_profile_frames&.last
      return unless frame
      frame[:nodes].each do |node|
        node.actual = @cost - frame[:start] if node.phase == phase
      end
    end

    def profile_accumulate(phase, amount)
      frame = @explain_profile_frames&.last
      return unless frame
      frame[:nodes].each { |node| node.actual = (node.actual || 0) + amount if node.phase == phase }
    end

    def profile_inherit(phase, child_phase)
      frame = @explain_profile_frames&.last
      return unless frame
      child = frame[:nodes].find { |node| node.phase == child_phase }
      profile_accumulate(phase, child&.actual || 0)
    end

    def insert(statement)
      profile_mutation(statement) { super }
    end

    def update(statement)
      profile_mutation(statement) { super }
    end

    def delete(statement)
      profile_mutation(statement) { super }
    end

    def select(statement)
      plan = @explain_runtime_plans && @explain_runtime_plans[statement.object_id]
      return super unless plan
      return super if @explain_profile_frames&.last&.dig(:plan).equal?(plan)
      @explain_profile_frames ||= []
      frame = { start: @cost, nodes: plan_nodes(plan), plan: plan }
      @explain_profile_frames << frame
      result = super
      plan.actual = @cost - frame[:start]
      result
    ensure
      @explain_profile_frames.pop if frame
    end

    # Public shared selector. Bound source expressions remain symbolic; callers
    # may inspect literal/parameter values without running arbitrary functions.
    def scan_access_plan(stored, predicate)
      return { kind: :full, terms: [] } unless stored[:collation_skew].to_a.empty?
      return { kind: :full, terms: [] } unless predicate
      terms = conjunctions(predicate)
      primary = stored[:native_primary_metadata]
      if primary && !primary[:columns].empty?
        keys = primary[:columns].each_with_index.map { |column, i| Expression.new(kind: :column, type: primary[:types][i], value: column) }
        parts = access_key_parts(keys, terms)
        return { kind: :primary, parts: parts, terms: parts.flat_map { |part| part[:terms] }, unique: parts.length == keys.length && parts.all? { |part| part[:point] } } unless parts.empty?
      end
      candidates = (stored[:native_secondary_metadata] || {}).sort_by { |name, _| name.b }.filter_map do |name, metadata|
        next if metadata[:predicate] && !conjunctions(metadata[:predicate]).all? { |part| terms.any? { |term| same_key_expression?(part, term) } }
        parts = access_key_parts(metadata[:expressions], terms)
        next if parts.empty?
        selected = parts.flat_map { |part| part[:terms] }
        rows = selected.reduce(stored[:rows].length) { |count, term| estimate_predicate(count, term) }
        [{ kind: :secondary, name: name, parts: parts, terms: selected,
           unique: !!metadata[:definition][:unique] && parts.length == metadata[:expressions].length && parts.all? { |part| part[:point] } }, rows]
      end
      candidates.min_by { |candidate, rows| [rows, candidate[:name].b] }&.first || { kind: :full, terms: [] }
    end

    private

    def profile_mutation(statement)
      plan = @explain_runtime_plans && @explain_runtime_plans[statement.object_id]
      return yield unless plan
      return yield if @explain_profile_frames&.last&.dig(:plan).equal?(plan)
      @explain_profile_frames ||= []
      frame = { start: @cost, nodes: plan_nodes(plan), plan: plan }
      frame[:nodes].each { |node| node.actual = 0 }
      @explain_profile_frames << frame
      result = yield
      plan.actual = @cost - frame[:start]
      result
    ensure
      @explain_profile_frames.pop if frame
    end

    def access_key_parts(keys, terms)
      parts = []
      keys.each do |key|
        comparisons = key_comparisons(key, terms)
        break if comparisons.empty?
        equality = comparisons.find { |operator, _| %w[= is\ null].include?(operator) }
        selected = equality ? [equality] : comparisons
        matched = terms.select { |term| !key_comparisons(key, [term]).empty? }
        parts << { key: key, comparisons: selected, point: !equality.nil?, terms: matched }
        break unless equality
      end
      parts
    end

    def explain_query(statement)
      inner = statement[:statement]
      old_types, old_active = @native_binding_types, @native_binding_active
      binding = ParameterBindings.new(self, inner).resolve
      if statement[:analyze] || !@params.empty?
        raise Error.new('42601', 'bind parameter count mismatch') unless @params.length == binding.count
        @params = binding.bind(@params)
      end
      @native_binding_types, @native_binding_active = binding.types, true
      @explain_options = statement
      @explain_plans = {}
      before_cost, before_lifetime = @cost, @lifetime_cost
      root = build_query_plan(inner)
      # Compilation is unmetered metadata work, including static validation.
      @cost, @lifetime_cost = before_cost, before_lifetime
      analyzed = nil
      if statement[:analyze]
        @explain_runtime_plans = @explain_plans
        start = @cost
        @explain_profile_frames = [{ start: start, nodes: plan_nodes(root), plan: root }]
        result = dispatch(inner)
        @explain_profile_frames.pop
        total = @cost - start
        root.actual = total
        count = result.rows.empty? ? result.rows_affected : result.rows.length
        count ||= 0
        analyzed = plan_node('Analyze', "cost=#{total} rows=#{count}", root.rows, root.cost, [root], :analyze)
        analyzed.actual = total
        analyzed.lane = root.lane
        @cost = before_cost
      end
      rows = []
      render_plan(analyzed || root, 0, rows, statement)
      # Rendering is already complete. Its outcome meter records rows without
      # another execution checkpoint; a ceiling is checked when work starts.
      @cost = [MAX_ESTIMATE, @cost + rows.length].min
      @lifetime_cost = [MAX_ESTIMATE, @lifetime_cost + rows.length].min
      columns, types = %w[depth node detail], %w[i32 text text]
      if statement[:costs]
        columns += %w[est_rows est_cost]
        types += %w[i64 i64]
      end
      if statement[:analyze]
        columns << 'actual_cost'
        types << 'i64'
      end
      if statement[:lane]
        columns << 'lane'
        types << 'text'
      end
      Result.new(columns: columns, column_types: types, rows: rows)
    ensure
      @native_binding_types, @native_binding_active = old_types, old_active
      @explain_options = @explain_plans = @explain_runtime_plans = @explain_profile_frames = nil
    end

    def plan_node(label, detail, rows, cost, children = [], phase = nil)
      Node.new(label: label, detail: detail, rows: [rows, MAX_ESTIMATE].min,
               cost: [cost, MAX_ESTIMATE].min, children: children, phase: phase)
    end

    def plan_nodes(root) = [root] + root.children.flat_map { |child| plan_nodes(child) }

    def render_plan(node, depth, rows, options, lane = node.lane || 'buffered')
      row = [depth, node.label, node.detail.empty? ? '-' : node.detail]
      row.concat([node.rows, node.cost]) if options[:costs]
      if options[:analyze]
        raise Error.new('0A000', "execution has no attribution checkpoint for #{node.label}") if node.actual.nil?
        row << node.actual
      end
      row << lane if options[:lane]
      rows << row
      node.children.each { |child| render_plan(child, depth + 1, rows, options, lane) }
    end

    def build_query_plan(statement)
      raise Error.new('0A000', 'EXPLAIN of WITH requires CTE plan frames') unless statement[:with].to_a.empty?
      node = case statement[:kind]
      when :select then plan_select(statement)
      when :values
        expressions = statement[:rows].flatten.map { |expression| plan_compile(expression, []) }
        count = statement[:rows].length
        plan_node('Values', "rows=#{count}", count, count + expressions.sum { |expression| expression_work(expression) }, [], :root)
      when :set_operation, :setop
        left, right = build_query_plan(statement[:left]), build_query_plan(statement[:right])
        operation = statement[:op]
        rows = operation == 'union' ? left.rows + right.rows : operation == 'intersect' ? [left.rows, right.rows].min : left.rows
        rows = [rows, 200].min unless statement[:all]
        plan_node(operation.capitalize, statement[:all] ? 'all' : 'distinct', rows, left.cost + right.cost, [left, right], :root)
      when :insert, :update, :delete then plan_mutation(statement)
      when :create_table then build_query_plan(statement[:query])
      else raise Error.new('42601', 'statement has no query plan')
      end
      @explain_plans[statement.object_id] = node
      node
    end

    def plan_compile(node, columns)
      unsupported = lambda do |value|
        case value
        when Hash
          %i[subquery exists array_query].include?(value[:kind]) || value[:query] || value.values.any? { |child| unsupported.call(child) }
        when Array then value.any? { |child| unsupported.call(child) }
        else false
        end
      end
      if unsupported.call(node)
        raise Error.new('0A000', 'expression subqueries require metadata-only expression plan frames')
      end
      compile(node, columns)
    end

    def plan_select(statement)
      sources = statement[:from] || []
      if sources.length > 1 || !(statement[:joins] || []).empty?
        raise Error.new('0A000', 'join EXPLAIN requires physical join plan frames')
      end
      source = sources.first
      if source && source[:kind] != :table
        raise Error.new('0A000', 'derived/SRF EXPLAIN requires relation plan frames')
      end
      stored = source && @tables[source[:name]]
      if source && !stored
        raise Error.new('0A000', 'catalog/view EXPLAIN requires relation plan frames') if introspection_relation?(source[:name]) || @views.key?(source[:name]) || @ctes.key?(source[:name])
        table(source[:name])
      end
      columns = stored ? table_columns(stored, source[:alias] || source[:name]) : []
      projections = expand_stars(statement[:columns], columns)
      outputs = projections.map { |projection| plan_compile(projection[:expr], columns) }
      predicate = statement[:where] && plan_compile(statement[:where], columns)
      raise Error.new('42804', 'WHERE requires boolean') if predicate && !%w[boolean unknown].include?(predicate.type)
      node = stored ? plan_scan(stored, predicate, statement) : plan_node('Result', '-', 1, 0, [], :scan)
      streaming_order = node.access ? node.access[:order] : :blocking
      if %i[forward reverse].include?(streaming_order)
        node.detail += if %i[secondary secondary_interval].include?(node.access[:kind])
          "; ordered: index order: #{node.access[:name]}"
        else
          '; ordered: pk ordered'
        end
        node.detail += ' (reverse)' if streaming_order == :reverse
      end
      logical_rows = stored ? stored[:rows].length : 1
      if predicate
        rows = [node.rows, estimate_predicate(logical_rows, predicate)].min
        detail = @explain_options[:verbose] ? "filter=#{plan_expression(predicate)}" : "conjuncts=#{conjunctions(predicate).length}"
        node = plan_node('Filter', detail, rows, node.cost + node.rows * expression_work(predicate), [node], :filter)
      end
      group_nodes = resolve_group_nodes(statement, columns)
      group_expressions = group_nodes.map { |expression| plan_compile(expression, columns) }
      aggregate_calls = outputs.flat_map { |expression| expression_nodes(expression) }.select { |expression| expression.kind == :call && Functions.aggregate?(expression.value) }.uniq { |expression| [expression.value, expression.args] }
      having = statement[:having] && plan_compile(statement[:having], columns)
      if !group_expressions.empty? || !aggregate_calls.empty? || having
        rows = group_expressions.empty? ? 1 : estimate_distinct_rows(node.rows, group_expressions)
        cost = node.cost + node.rows * (aggregate_calls.length + group_expressions.sum { |expression| expression_work(expression) } + aggregate_calls.sum { |expression| expression.args.sum { |argument| expression_work(argument) } })
        detail = "groups=#{group_expressions.length} aggs=#{aggregate_calls.length}"
        if having
          cost += rows * expression_work(having)
          rows = estimate_predicate(rows, having)
          detail += @explain_options[:verbose] ? "; having=#{plan_expression(having)}" : "; having:conjuncts=#{conjunctions(having).length}"
        end
        node = plan_node('Aggregate', detail, rows, cost, [node], :aggregate)
      end
      if outputs.any? { |expression| expression_nodes(expression).any? { |part| part.kind == :window } } || !(statement[:windows] || {}).empty?
        raise Error.new('0A000', 'window EXPLAIN requires resolved window plan frames')
      end
      node = plan_node('Distinct', '-', estimate_distinct_rows(node.rows, outputs), node.cost, [node], :distinct) if statement[:distinct]
      orders = statement[:order_by] || []
      unless orders.empty? || %i[forward reverse].include?(streaming_order)
        order_work = orders.sum do |order|
          expression = order[:expr]
          projection = expression[:kind] == :literal && expression[:value].is_a?(Integer) || expression[:kind] == :column && !expression[:table] && projections.any? { |item| (item[:alias] || expression_name(item[:expr])) == expression[:name] }
          projection ? 0 : expression_work(plan_compile(expression, columns))
        end
        node = plan_node('Sort', "keys=#{orders.length}", node.rows, node.cost + order_work * node.rows, [node], :sort)
      end
      if statement[:limit] || statement[:offset]
        limit = statement[:limit] && plan_constant(plan_compile(statement[:limit], []))
        offset = statement[:offset] ? plan_constant(plan_compile(statement[:offset], [])) : 0
        raise Error.new('0A000', 'nonconstant LIMIT/OFFSET estimate is not implemented') if offset == :unknown || limit == :unknown
        offset ||= 0
        raise Error.new('2201X', 'negative OFFSET') if offset.negative?
        raise Error.new('2201W', 'negative LIMIT') if limit && limit.negative?
        rows = [node.rows - offset, 0].max
        rows = [rows, limit].min if limit
        detail = []
        detail << "limit=#{limit}" if limit
        detail << "offset=#{offset}" if statement[:offset]
        node = plan_node('Limit', detail.join(' '), rows, node.cost, [node], :root)
      end
      node.cost = [MAX_ESTIMATE, node.cost + node.rows * (1 + outputs.sum { |expression| expression_work(expression) })].min
      if @explain_options[:verbose]
        text = "output=[#{outputs.map { |expression| plan_expression(expression) }.join(', ')}]"
        node.detail = node.detail == '-' ? text : "#{node.detail}; #{text}"
      end
      node.lane = streaming_order == :blocking ? 'buffered' : 'streaming'
      node.lane = 'buffered' if outputs.any? { |expression| expression_nodes(expression).any? { |part| part.kind == :call && %w[nextval setval].include?(part.value) } }
      node
    end

    # Keep the plan's order proof on the executor's implementation. Eligibility
    # alone is insufficient: it may still choose the buffered fallback.
    def plan_streaming_order(statement, stored, columns, projections, outputs, predicate)
      return :blocking unless stored && streaming_select_shape?(statement)
      # Validate before stream_order compiles an ORDER BY expression, so a plain
      # plan cannot accidentally fold an expression subquery by executing it.
      (statement[:order_by] || []).each do |order|
        expression = order[:expr]
        alias_reference = expression[:kind] == :column && !expression[:table] && projections.any? { |item| (item[:alias] || expression_name(item[:expr])) == expression[:name] }
        plan_compile(expression, columns) unless alias_reference
      end
      output_columns = projections.zip(outputs).map { |item, expression| { name: item[:alias] || expression_name(item[:expr]), type: expression.type } }
      order = stream_order(statement, stored, columns, outputs, output_columns)
      if order != :none && stored[:native_secondary_metadata]&.any?
        key = stored[:native_primary_metadata]&.dig(:columns)&.first
        primary_bound = key && predicate && conjunctions(predicate).any? { |term| !key_comparisons(Expression.new(kind: :column, type: stored[:columns][key][:type], value: key), [term]).empty? }
        return :blocking unless primary_bound || predicate.nil?
      end
      order
    end

    def plan_scan(stored, predicate, statement, access = nil)
      access ||= scan_access_plan(stored, predicate, statement)
      rows = access[:scan_rows]
      detail = case access[:kind]
      when :full then 'Full scan'
      when :primary
        parts = access[:parts].flat_map do |part|
          name = stored[:columns][part[:key].value][:name]
          part[:comparisons].map { |operator, source| "#{name} #{operator}#{source ? ' ' + plan_expression(source) : ''}" }
        end
        'PK bound: ' + parts.join(' and ')
      when :secondary
        "Index #{access[:parts].empty? ? 'scan' : 'bound'}: using #{access[:name]}"
      when :primary_interval, :secondary_interval
        key = access[:kind] == :primary_interval ? "PK interval set: #{stored[:columns][access[:key].value][:name]}" : "Index interval set: using #{access[:name]}"
        key += "; intervals=#{access[:intervals]}"
        key += "; arrays=#{access[:arrays]}" if access[:arrays].positive?
        key
      when :gin then "GIN bound: using #{access[:name]}"
      when :gist then "GiST bound: using #{access[:name]}"
      else raise Error.new('0A000', 'access path has no EXPLAIN estimator')
      end
      mutation = %i[update delete].include?(statement[:kind])
      touched = scan_column_inventory(statement, mutation: mutation)[stored[:name]] || []
      detail += "; touched=#{touched.length}" unless touched.empty?
      result = plan_node("Scan #{stored[:local_name] || stored[:name]}", detail, rows, access[:scan_cost], [], :scan)
      result.access = access
      result
    end

    def estimate_bound_pages(structure, terms, rows)
      return 0 if rows.zero?
      count = structure[:node_count]
      estimate = terms.reduce(count) { |value, term| estimate_predicate(value, term) }
      [[estimate, structure[:height]].max, count].min
    end

    def estimate_distinct_rows(rows, expressions)
      [rows, 200**expressions.length, MAX_ESTIMATE].min
    end

    def estimate_predicate(rows, expression)
      return rows unless expression
      if expression.kind == :literal
        return expression.value == true ? rows : 0
      end
      if expression.kind == :binary
        operator, left, right = expression.value, *expression.args
        return estimate_predicate(estimate_predicate(rows, left), right) if operator == 'and'
        return [rows, estimate_predicate(rows, left) + estimate_predicate(rows, right)].min if operator == 'or'
        return 0 if %w[= <> < > <= >=].include?(operator) && expression.args.any? { |argument| plan_constant(argument).nil? }
        denominator = %w[= <> != like ilike ~ ~*].include?(operator) ? 200 : %w[< <= > >=].include?(operator) ? 3 : %w[@> <@ &&].include?(operator) ? 100 : 3
        return (rows + denominator - 1) / denominator
      end
      if expression.kind == :unary && expression.value == 'is null'
        return (rows + 199) / 200
      end
      (rows + (expression.type == 'boolean' ? 1 : 2)) / (expression.type == 'boolean' ? 2 : 3)
    end

    def plan_constant(expression)
      return :unknown unless expression
      return expression.value if expression.kind == :literal
      return @params[expression.value] if expression.kind == :param && expression.value < @params.length
      if expression.kind == :cast && expression.args.first.kind == :literal
        return cast_value(expression.args.first.value, expression.type)
      end
      :unknown
    end

    def expression_nodes(expression)
      return expression.flat_map { |part| expression_nodes(part) } if expression.is_a?(Array)
      return [] unless expression.is_a?(Expression)
      [expression] + (expression.args || []).flat_map { |argument| expression_nodes(argument) }
    end

    def expression_work(expression)
      return expression.sum { |part| expression_work(part) } if expression.is_a?(Array)
      return 0 unless expression.is_a?(Expression)
      return 0 if expression.kind == :call && Functions.aggregate?(expression.value)
      own = %i[literal column param outer source star].include?(expression.kind) ? 0 : 1
      own + (expression.args || []).sum { |argument| expression_work(argument) }
    end

    def plan_expression(expression)
      args = expression.args || []
      case expression.kind
      when :literal
        value = expression.value
        return 'null' if value.nil?
        return value ? 'true' : 'false' if value == true || value == false
        return plan_string(Types.format(value, expression.type)) if %w[text uuid date timestamp timestamptz interval bytea json jsonb jsonpath].include?(expression.type) || value.is_a?(Array) || value.is_a?(RangeValue)
        Types.format(value, expression.type)
      when :column then "##{expression.value}"
      when :param then "$#{expression.value + 1}"
      when :outer then "outer(#{Array(expression.value).join(',')})"
      when :binary, :json_operator then "(#{plan_expression(args[0])} #{expression.value} #{plan_expression(args[1])})"
      when :unary
        %w[is\ null is\ not\ null is\ true is\ false is\ unknown].include?(expression.value) ? "(#{plan_expression(args[0])} #{expression.value})" : "(#{expression.value} #{plan_expression(args[0])})"
      when :cast then "cast(#{plan_expression(args[0])} as #{expression.type})"
      when :call, :jsonpath_function
        name = { 'substring' => 'substr', 'position' => 'strpos' }.fetch(expression.value, expression.value)
        "#{name}(#{args.map { |argument| plan_expression(argument) }.join(', ')})"
      when :width_bucket_array then "width_bucket(#{args.map { |argument| plan_expression(argument) }.join(', ')})"
      when :datetime_format, :number_format then "to_char(#{args.map { |argument| plan_expression(argument) }.join(', ')})"
      when :array then "array[#{args.map { |argument| plan_expression(argument) }.join(', ')}]"
      when :row then "row(#{args.map { |argument| plan_expression(argument) }.join(', ')})"
      when :star then '*'
      when :case
        operand, branches, fallback = args
        text = +'(case'
        text << " #{plan_expression(operand)}" if operand
        branches.each { |condition, value| text << " when #{plan_expression(condition)} then #{plan_expression(value)}" }
        text << " else #{plan_expression(fallback)} end)"
      when :quantified
        "(#{plan_expression(args[0])} #{expression.extra[:op]} #{expression.extra[:quantifier]}(#{plan_expression(args[1])}))"
      when :field then "(#{plan_expression(args[0])}).#{expression.value}"
      when :json_sql then plan_json_expression(expression)
      else raise Error.new('0A000', "resolved expression #{expression.kind} has no EXPLAIN renderer")
      end
    end

    def plan_string(value)
      "'" + value.to_s.gsub('\\') { '\\\\' }.gsub("'", "''") + "'"
    end

    def plan_json_expression(expression)
      spec = expression.extra
      context = spec[:context]
      if context.kind == :literal && context.type == 'text' && context.value
        begin
          context = Expression.new(kind: :literal, type: 'jsonb', value: JsonValue.parse(context.value, binary: true))
        rescue Error
          # A typed text operand is parsed only by execution, not by EXPLAIN.
        end
      end
      text = "#{spec[:kind]}(#{plan_expression(context)}, #{plan_expression(spec[:path])}"
      unless spec[:passing].empty?
        bindings = spec[:passing].map do |binding|
          value = binding[:expression]
          value = value.dup.tap { |copy| copy.type = 'jsonb' } if value.kind == :cast && value.type == 'json'
          if value.kind == :literal && value.type == 'json'
            value = Expression.new(kind: :cast, type: 'jsonb', args: [Expression.new(kind: :literal, type: 'text', value: value.value.to_s)])
          end
          name = binding[:name]
          name = SQLRender.identifier(name) unless name.match?(/\A[a-z_][a-z_0-9]*\z/)
          "#{plan_expression(value)} as #{name}"
        end
        text << " passing #{bindings.join(', ')}"
      end
      unless spec[:kind] == :json_exists
        text << " returning #{spec[:type]}"
        if spec[:kind] == :json_query
          text << case spec[:wrapper]
          when :without then ' without array wrapper'
          when :conditional then ' with conditional array wrapper'
          else ' with unconditional array wrapper'
          end
          text << ' keep quotes on scalar string'
        end
        text << " #{plan_json_behavior(spec[:on_empty])} on empty"
      end
      text << " #{plan_json_behavior(spec[:on_error])} on error)"
    end

    def plan_json_behavior(behavior)
      behavior[:kind] == :default ? "default #{plan_expression(behavior[:expression])}" : behavior[:kind].to_s.tr('_', ' ')
    end

    def plan_mutation(statement)
      stored = table(statement[:table])
      name = stored[:local_name] || stored[:name]
      if statement[:kind] == :insert
        child = if statement[:query]
          build_query_plan(statement[:query])
        else
          expressions = (statement[:rows] || [[]]).flatten.map { |node| plan_compile(node, []) }
          count = (statement[:rows] || [[]]).length
          plan_node('Values', "rows=#{count}", count, expressions.sum { |expression| expression_work(expression) }, [], :values)
        end
        conflict = statement[:on_conflict]
        detail = conflict ? "on conflict do #{conflict[:action]}" : '-'
        plan_node("Insert #{name}", detail, child.rows, child.cost, [child], :root)
      else
        columns = table_columns(stored, statement[:alias] || name)
        predicate = statement[:where] && plan_compile(statement[:where], columns)
        child = plan_scan(stored, predicate, statement)
        if predicate
          rows = [child.rows, estimate_predicate(stored[:rows].length, predicate)].min
          detail = @explain_options[:verbose] ? "filter=#{plan_expression(predicate)}" : "conjuncts=#{conjunctions(predicate).length}"
          child = plan_node('Filter', detail, rows, child.cost + child.rows * expression_work(predicate), [child], :filter)
        end
        detail = statement[:kind] == :update ? "sets=#{statement[:assignments].length}" : '-'
        plan_node("#{statement[:kind].to_s.capitalize} #{name}", detail, child.rows, child.cost, [child], :root)
      end
    end
  end

  Engine.prepend(QueryPlans)
end
