# frozen_string_literal: true

module JedNative
  # Preserve authored predicates for access planning, while evaluating their
  # non-trapping, relation-local conjuncts as rows enter the join.
  module JoinPushdown
    def materialize_from(sources, joins, predicate = nil)
      rewritten = @explain_runtime_plans && @explain_rewritten_from&.[](@native_scan_statement&.object_id)
      rewritten ||= rewrite_join_inputs(sources, joins, predicate)
      rewritten ? super(*rewritten) : super
    end

    def rewrite_join_inputs(sources, joins, predicate = nil)
      return if sources.empty? || sources.length > 1 && !joins.empty?
      return if joins.any? { |item| item[:using] || item[:natural] }
      inputs = sources + joins.map { |item| item[:relation] || item[:right] }
      return if inputs.any? { |source| source[:kind] == :join_group }
      labels = inputs.filter_map { |source| source[:alias] || (source[:kind] == :table && source[:name]) }
      raise Error.new('42712', 'table name specified more than once') unless labels.uniq.length == labels.length
      analyzer = ParameterBindings.new(self, nil)
      scopes = []
      inputs.each { |source| scopes << query_source_columns(source, @ctes.transform_values(&:columns), @outer.flat_map(&:first) + scopes.flatten, analyzer) }
      scope = scopes.flatten
      offsets, offset = [], 0
      scopes.each { |columns| offsets << offset; offset += columns.length }
      nullable = Array.new(inputs.length, false)
      pushed, access = Array.new(inputs.length) { [] }, Array.new(inputs.length) { [] }
      rewritten = joins.each_with_index.map do |item, index|
        right = sources.length + index
        residual = []
        (item[:on] ? ast_conjunctions(item[:on]) : []).each do |term|
          target = pushdown_target(term, scope, offsets, scopes)
          movable = target && !inputs[target][:lateral] && !nullable[target] &&
            case item[:kind]
            when :inner then target <= right
            when :left, :left_outer then target == right
            when :right, :right_outer then target < right
            else false
            end
          if movable
            pushed[target] << term
            access[target] << term
          else
            residual << term
            bound_target = access_predicate_target(term, scope, offsets, scopes)
            eligible = bound_target && !inputs[bound_target][:lateral] && !nullable[bound_target] &&
              (item[:kind] == :inner && bound_target <= right || %i[left left_outer].include?(item[:kind]) && bound_target == right || %i[right right_outer].include?(item[:kind]) && bound_target < right)
            access[bound_target] << term if eligible
          end
        end
        case item[:kind]
        when :left, :left_outer then nullable[right] = true
        when :right, :right_outer then right.times { |i| nullable[i] = true }
        when :full, :full_outer then (0..right).each { |i| nullable[i] = true }
        end
        item.merge(on: join_conjunctions(residual))
      end
      residual = []
      (predicate ? ast_conjunctions(predicate) : []).each do |term|
        target = pushdown_target(term, scope, offsets, scopes)
        if target && !inputs[target][:lateral] && !nullable[target]
          pushed[target] << term
          access[target] << term
        else
          residual << term
          # A strict bare-key WHERE can bound a nullable relation without
          # moving its residual: null extensions still face the WHERE.
          access[target] << term if target && pushdown_strict_bound?(term)
          bound_target = access_predicate_target(term, scope, offsets, scopes)
          access[bound_target] << term if bound_target && !inputs[bound_target][:lateral] && !nullable[bound_target]
        end
      end
      inputs = inputs.each_with_index.map do |source, index|
        source, remaining = push_into_body(source, pushed[index], scopes[index])
        if inputs.length == 1
          residual.concat(remaining)
          remaining = []
        end
        source.merge(native_pushed_filter: join_conjunctions(remaining),
                     native_access_predicate: join_conjunctions(access[index]))
      end
      if inputs.length == 1 && predicate
        positions = {}.compare_by_identity
        ast_conjunctions(predicate).each_with_index { |term, index| positions[term] = index }
        residual.sort_by! { |term| positions.fetch(term, positions.length) }
      end
      rewritten = rewritten.each_with_index.map do |item, index|
        source = inputs[sources.length + index].merge(native_original_on: joins[index][:on], native_join_where: predicate,
          native_fixed_hash: inputs.length == 2,
          native_wide_outer: inputs.length > 2 && joins.any? { |edge| !%i[inner cross].include?(edge[:kind]) })
        item.merge(relation: source)
      end
      [inputs.first(sources.length), rewritten, join_conjunctions(residual)]
    end

    def source_relation(source)
      scan_start = @cost if source[:native_scan_profile]
      relation = super
      source[:native_scan_profile].actual += @cost - scan_start if scan_start
      filter = source[:native_pushed_filter]
      return relation unless filter
      filter_start = @cost if source[:native_filter_profile]
      expression = compile_filter(filter, relation.columns)
      result = if @query_memory && relation.instance_variable_get(:@native_memory_owner)
        query_memory_filter(relation) { |row| evaluate(expression, row) == true }
      else
        Relation.new(columns: relation.columns, rows: relation.rows.select { |row| evaluate(expression, row) == true })
      end
      source[:native_filter_profile].actual = source[:native_scan_profile].actual + @cost - filter_start if filter_start
      result
    end

    def local_filter?(node, columns)
      return false unless node.is_a?(Hash)
      return false unless pushdown_syntax?(node)
      references = pushdown_column_nodes(node)
      return false if references.empty? || references.any? do |reference|
        columns.count { |column| column[:name] == reference[:name] && (reference[:table] ? column[:table] == reference[:table] : !column[:qualified_only]) } != 1
      end
      expression = compile(node, columns)
      pushdown_safe?(expression) && expression_nodes(expression).any? { |item| item.kind == :column }
    end

    private

    def pushdown_column_nodes(node)
      case node
      when Hash then node[:kind] == :column ? [node] : node.values.flat_map { |child| pushdown_column_nodes(child) }
      when Array then node.flat_map { |child| pushdown_column_nodes(child) }
      else []
      end
    end

    def pushdown_target(node, scope, offsets, scopes)
      return unless pushdown_syntax?(node)
      expression = compile(node, scope)
      return unless pushdown_safe?(expression)
      indices = expression_nodes(expression).select { |item| item.kind == :column }.map(&:value)
      return if indices.empty?
      targets = indices.map { |ordinal| offsets.each_index.find { |i| ordinal >= offsets[i] && ordinal < offsets[i] + scopes[i].length } }.uniq
      targets.first if targets.length == 1
    end

    def access_predicate_target(node, scope, offsets, scopes)
      return unless metadata_expression?(node)
      references = pushdown_column_nodes(node)
      return if references.empty?
      indices = references.map do |reference|
        matches = scope.each_index.select do |ordinal|
          column = scope[ordinal]
          column[:name] == reference[:name] && (reference[:table] ? column[:table] == reference[:table] : !column[:qualified_only])
        end
        return unless matches.length == 1
        matches.first
      end
      targets = indices.map { |ordinal| offsets.each_index.find { |i| ordinal >= offsets[i] && ordinal < offsets[i] + scopes[i].length } }.uniq
      targets.first if targets.length == 1
    end

    def pushdown_syntax?(node)
      return true unless node.is_a?(Hash)
      case node[:kind]
      when :column, :literal, :param then true
      when :binary then pushdown_syntax?(node[:left]) && pushdown_syntax?(node[:right])
      when :unary, :cast then pushdown_syntax?(node[:expr] || node[:operand])
      when :in
        !node[:query] && pushdown_syntax?(node[:expr] || node[:left]) && (node[:values] || node[:list] || []).all? { |child| pushdown_syntax?(child) }
      when :between
        [node[:expr], node[:low] || node[:lower], node[:high] || node[:upper]].all? { |child| pushdown_syntax?(child) }
      when :call then FunctionCatalog::SAFE_FUNCTIONS.key?(node[:name]) && (node[:args] || []).all? { |child| pushdown_syntax?(child) }
      else false
      end
    end

    def pushdown_safe?(expression)
      args = expression.args || []
      case expression.kind
      when :column, :literal, :param then true
      when :cast
        FunctionCatalog::SAFE_CASTS.include?([args.first.type, expression.type]) && pushdown_safe?(args.first)
      when :call
        signatures = FunctionCatalog::SAFE_FUNCTIONS[expression.value] || []
        signatures.any? { |families| families.length == args.length && families.zip(args).all? { |family, arg| family == arg.type || family == Functions.family(arg.type) || family == 'anyrange' && Types::RANGES.key?(arg.type) } } && args.all? { |arg| pushdown_safe?(arg) }
      when :unary
        %w[not is\ null is\ not\ null is\ true is\ false is\ unknown is\ not\ true is\ not\ false is\ not\ unknown].include?(expression.value) && args.all? { |arg| pushdown_safe?(arg) }
      when :binary
        if %w[like ilike not\ like not\ ilike].include?(expression.value)
          pattern = plan_constant(args.last)
          return false unless pattern.nil? || pattern.is_a?(String)
          return false if pattern && (pattern[/\\+\z/] || '').length.odd?
        elsif !%w[and or = != <> < <= > >= is\ distinct\ from is\ not\ distinct\ from].include?(expression.value)
          return false
        end
        if %w[< <= > >=].include?(expression.value)
          label = expression.instance_variable_get(:@native_collation)
          return false if label && label.name != 'C'
        end
        args.all? { |arg| pushdown_safe?(arg) }
      else false
      end
    end

    def pushdown_strict_bound?(node)
      node[:kind] == :binary && %w[= < <= > >=].include?(node[:op]) &&
        [node[:left], node[:right]].any? { |arg| arg[:kind] == :column } &&
        [node[:left], node[:right]].any? { |arg| %i[literal param].include?(arg[:kind]) }
    end
  end

  Engine.prepend(JoinPushdown)
end
