# frozen_string_literal: true

module JedNative
  # Host values do not participate in inference. Resolve every parameter's SQL
  # type from syntax and catalog metadata, then coerce all values before dispatch.
  # This also applies to predicates and assignments that will visit zero rows.
  class ParameterBindings
    UNKNOWN = 'unknown'
    TEXT_OPERATORS = %w[like ilike not\ like not\ ilike ~ ~* !~ !~*].freeze
    COMPARISONS = %w[= <> != < <= > >= is\ distinct\ from is\ not\ distinct\ from @> <@ && << >> &< &> -|-].freeze
    attr_reader :types, :count, :correlated

    def initialize(engine, statement)
      @engine, @statement = engine, statement
      @types, @modifiers, @not_null = [], {}, {}
      @count = highest_parameter(statement)
      @ctes = engine.instance_variable_get(:@ctes).transform_values(&:columns)
      @scope_depth = 0
    end

    def highest_parameter(node)
      case node
      when Hash
        return node[:index] if node[:kind] == :param
        node.values.map { |child| highest_parameter(child) }.max || 0
      when Array then node.map { |child| highest_parameter(child) }.max || 0
      else 0
      end
    end

    def resolve
      return self if @count.zero?
      @types = Array.new(@count, UNKNOWN)
      if @statement[:kind].to_s.start_with?('create_', 'drop_', 'alter_') && !(@statement[:kind] == :create_table && @statement[:query])
        raise Error.new('42601', 'DDL does not accept parameters')
      end
      # A later occurrence can constrain an earlier function or expression.
      (@count + 1).times do
        previous = @types.dup
        statement(@statement)
        break if @types == previous
      end
      @final = true
      statement(@statement)
      raise Error.new('42P18', 'could not determine parameter type') if @types.include?(UNKNOWN)
      self
    end

    def validate_schema_parameters
      return if @count.zero?
      kind = @statement[:kind].to_s
      return unless kind.start_with?('create_', 'drop_', 'alter_')
      return if @statement[:kind] == :create_table && @statement[:query]
      raise Error.new('42P02', 'schema expressions cannot contain parameters')
    end

    # The caller supplies only the enclosing scope's metadata. Marking these
    # columns lets a scalar-subquery planner distinguish genuine outer reads
    # from references to the subquery's own relations and nested subqueries.
    def query_columns(node = @statement, outer = [], parameter_types: nil)
      @types = parameter_types.dup if parameter_types
      @types = Array.new(@count, UNKNOWN) if @types.empty? && @count.positive?
      @correlated = false
      @validate_names = true
      statement(node, outer.map { |column| column.merge(binding_external: true, binding_scope: -1) })
    end

    def relation_columns(node, outer = [])
      relation(node, outer)
    end

    def bind(values)
      values.each_with_index.map do |value, index|
        if value.nil? && @not_null[index]
          raise Error.new('23502', 'NULL parameter assigned to a NOT NULL column')
        end
        result = coerce(value, @types[index])
        (@modifiers[index] || []).each do |mod|
          result = @engine.send(:explicit_modifier, result, @types[index], mod)
        end
        result
      end
    end

    def coerce(value, type)
      return nil if value.nil?
      if type.end_with?('[]')
        return @engine.cast_value(value, type) if value.is_a?(String)
        raise Error.new('42804', 'array parameter requires an array value') unless value.is_a?(Array) && !value.is_a?(CompositeValue)
        array = value.is_a?(ArrayValue) ? value : ArrayValue.new(value, element_type: type[0...-2])
        elements = array.map { |element| coerce(element, type[0...-2]) }
        return ArrayValue.new(elements, element_type: type[0...-2], dimensions: array.dimensions, lower_bounds: array.lower_bounds)
      end
      fields = @engine.instance_variable_get(:@types)[type]
      if fields
        return @engine.cast_value(value, type) if value.is_a?(String)
        unless value.is_a?(Array) && value.length == fields.length
          raise Error.new('42804', 'composite parameter has wrong field count')
        end
        return CompositeValue.new(fields.each_with_index.map { |field, i| coerce(value[i], field[:type]) }, type_name: type, field_names: fields.map { |field| field[:name] })
      end
      valid = case type
      when 'i16', 'i32', 'i64' then value.is_a?(Integer)
      when 'decimal' then value.is_a?(Integer) || value.is_a?(Decimal)
      when 'f32', 'f64' then value.is_a?(Integer) || value.is_a?(Decimal) || value.is_a?(Float)
      when 'boolean' then value == true || value == false
      when 'text' then value.instance_of?(String)
      when 'bytea', 'uuid' then value.is_a?(String)
      when 'date' then value.is_a?(DateValue) || value.is_a?(String)
      when 'timestamp', 'timestamptz' then value.is_a?(TimestampValue) || value.is_a?(String)
      when 'interval' then value.is_a?(IntervalValue) || value.is_a?(String)
      when 'json', 'jsonb' then value.is_a?(JsonValue) || value.is_a?(String)
      else Types::RANGES.key?(type) && (value.is_a?(RangeValue) || value.is_a?(String))
      end
      raise Error.new('42804', "parameter value is incompatible with #{type}") unless valid
      @engine.cast_value(value, type)
    end

    private

    def constrain(node, type, modifier = nil, not_null = false)
      return unless node && node[:kind] == :param
      type = Types.normalize(type || UNKNOWN)
      return if [UNKNOWN, 'record', 'unknown[]'].include?(type)
      index = node[:index] - 1
      old = @types[index]
      raise Error.new('42804', 'conflicting parameter types') unless old == UNKNOWN || old == type
      @types[index] = type
      (@modifiers[index] ||= []) << modifier if modifier && !(@modifiers[index] || []).include?(modifier)
      @not_null[index] = true if not_null
    end

    def statement(node, outer = [])
      return [] unless node
      previous_depth = @scope_depth
      @scope_depth += 1
      previous_ctes = @ctes.dup
      (node[:with] || []).each do |cte|
        additions = []
        if node[:recursive] && %i[set_operation setop].include?(cte[:query][:kind])
          seed = statement(cte[:query][:left], outer)
          seed = seed.each_with_index.map { |column, i| column.merge(name: cte[:columns]&.[](i) || column[:name]) }
          additions = @engine.cte_search_cycle_columns(cte, seed) if @engine.respond_to?(:cte_search_cycle_columns)
          @ctes[cte[:name]] = seed + additions.map { |column| column.merge(star_hidden: true) }
        end
        columns = statement(cte[:query], outer)
        columns = columns.each_with_index.map { |column, i| column.merge(name: cte[:columns]&.[](i) || column[:name]) }
        @ctes[cte[:name]] = columns + additions
      end
      case node[:kind]
      when :select
        scope = []
        (node[:from] || []).each_with_index do |source, index|
          visible = source[:lateral] || %i[function join_group].include?(source[:kind]) ? scope + outer : outer
          scope.concat(relation(source, visible))
          (node[:joins] || []).select { |join| (join[:from_index] || 0) == index }.each do |join|
          joined = join[:relation]
          visible = joined[:lateral] || %i[function join_group].include?(joined[:kind]) ? scope + outer : outer
          right = relation(joined, visible)
          using = join[:using] || (join[:natural] ? scope.map { |column| column[:name] } & right.map { |column| column[:name] } : [])
          right = right.map { |column| using.include?(column[:name]) ? column.merge(qualified_only: true) : column }
          scope.concat(right)
          expression(join[:on], scope + outer, 'boolean') if join[:on]
          end
        end
        scope = scope.map { |column| column.merge(binding_scope: @scope_depth) } + outer
        expression(node[:where], scope, 'boolean') if node[:where]
        output = (node[:columns] || []).flat_map do |projection|
          expr = projection[:expr]
          if expr[:kind] == :star
            scope.select { |column| !column[:star_hidden] && (expr[:table] ? column[:table] == expr[:table] : !column[:qualified_only] && column[:binding_scope] == @scope_depth) }
          elsif expr[:kind] == :field_star
            type = expression(expr[:expr], scope)
            @engine.instance_variable_get(:@types)[type] || raise(Error.new('42809', 'star requires a composite value'))
          else
            [{ name: projection[:alias] || @engine.expression_name(expr), type: expression(expr, scope) }]
          end
        end
        group_output = output.map { |column| column.merge(binding_scope: @scope_depth - 0.5) }
        order_output = output.map { |column| column.merge(binding_scope: @scope_depth + 0.5) }
        (node[:group_by] || []).each { |expr| expression(expr, scope + group_output) }
        expression(node[:having], scope + group_output, 'boolean') if node[:having]
        (node[:order_by] || []).each { |order| expression(order[:expr], order_output + scope) }
        expression(node[:limit], scope, 'i64') if node[:limit].is_a?(Hash)
        expression(node[:offset], scope, 'i64') if node[:offset].is_a?(Hash)
        (node[:windows] || {}).each_value { |window| window_expressions(window, scope) }
        output
      when :values
        rows = node[:rows] || []
        width = rows.first&.length || 0
        Array.new(width) do |i|
          nodes = rows.map { |row| row[i] }
          { name: "column#{i + 1}", type: unify(nodes, outer) }
        end
      when :set_operation, :setop
        left, right = statement(node[:left], outer), statement(node[:right], outer)
        left.each_with_index.map { |column, i| column.merge(type: common(column[:type], right[i]&.dig(:type))) }
      when :insert, :update, :delete
        view = @engine.instance_variable_get(:@views)[node[:table]]
        table = view ? { name: node[:table], columns: @engine.send(:view_schema, view) } : @engine.table(node[:table])
        scope = @engine.table_columns(table, node[:alias] || node[:table]).map { |column| column.merge(binding_scope: @scope_depth) } + outer
        if node[:kind] == :insert
          columns = node[:columns] ? node[:columns].map { |name| table[:columns].find { |column| column[:name] == name } } : table[:columns]
          (node[:rows] || []).each do |row|
            row.each_with_index { |value, i| assignment(value, columns[i], scope) if columns[i] }
          end
          statement(node[:query], outer) if node[:query]
          if (conflict = node[:on_conflict])
            conflict_scope = scope + @engine.table_columns(table, 'excluded')
            (conflict[:assignments] || []).each do |item|
              assignment(item[:expr], table[:columns].find { |column| column[:name] == item[:column] }, conflict_scope)
            end
            expression(conflict[:where], conflict_scope, 'boolean') if conflict[:where]
          end
        elsif node[:kind] == :update
          (node[:assignments] || []).each do |item|
            assignment(item[:expr], table[:columns].find { |column| column[:name] == item[:column] }, scope)
          end
        end
        expression(node[:where], scope, 'boolean') if node[:where]
        aliases = node[:returning_aliases] || {}
        returning_scope = scope + %w[old new].flat_map do |name|
          @engine.table_columns(table, aliases[name.to_sym] || name).map { |column| column.merge(binding_scope: @scope_depth, qualified_only: true) }
        end
        (node[:returning] || []).flat_map do |item|
          expr = item[:expr]
          if expr[:kind] == :star
            returning_scope.select { |column| expr[:table] ? column[:table] == expr[:table] : !column[:qualified_only] }
          else
            [{ name: item[:alias] || @engine.expression_name(expr), type: expression(expr, returning_scope) }]
          end
        end
      when :create_table then statement(node[:query], outer)
      when :explain then statement(node[:statement], outer)
      else
        []
      end
    ensure
      @ctes = previous_ctes if previous_ctes
      @scope_depth = previous_depth if previous_depth
    end

    def assignment(node, column, scope)
      return unless column
      constrain(node, column[:type], column[:type_mod], column[:nullable] == false)
      expression(node, scope, column[:type])
    end

    def relation(node, outer)
      return [] unless node
      name = node[:alias] || node[:name]
      columns = case node[:kind]
      when :table
        if !node[:database] && @ctes.key?(node[:name])
          @ctes[node[:name]]
        elsif (view = @engine.instance_variable_get(:@views)[node[:name]])
          view[:schema] || statement(view[:query], outer).each_with_index.map { |column, i| column.merge(name: view[:columns]&.[](i) || column[:name]) }
        elsif @engine.respond_to?(:introspection_columns) && (catalog_columns = @engine.introspection_columns(node[:name], node[:database], name))
          catalog_columns
        else
          @engine.table_columns(@engine.table(node[:name]), name)
        end
      when :subquery then statement(node[:query], outer)
      when :values then statement(node, outer)
      when :join_group then statement(node.merge(kind: :select, columns: [{ expr: { kind: :star } }]), outer)
      when :function, :call, :srf
        call = node[:expr] || node[:call] || node
        if call[:kind] == :json_table
          expression(call[:context], outer, 'jsonb')
          expression(call[:path], outer, 'jsonpath')
          (call[:passing] || []).each { |binding| expression(binding[:expr], outer) }
          @engine.json_table_columns(call[:columns])
        elsif defined?(JsonRelations) && JsonRelations::JSON_RELATIONS.include?(call[:name])
          arguments = (call[:args] || []).map { |arg| Expression.new(kind: :metadata, type: expression(arg, outer)) }
          @engine.json_relation_columns(node, arguments)
        else
          type = expression(call, outer)
          [{ name: node[:alias] || call[:name] || 'value', type: type.sub(/\[\]\z/, '') }]
        end
      else []
      end
      columns.each_with_index.map { |column, i| column.merge(table: name || column[:table], name: node[:columns]&.[](i) || column[:name]) }
    end

    def expression(node, scope, wanted = nil)
      return UNKNOWN unless node.is_a?(Hash)
      case node[:kind]
      when :param
        constrain(node, wanted)
        @types[node[:index] - 1]
      when :literal then Types.normalize(node[:type] || Types.infer(node[:value]))
      when :column
        matches = scope.select { |column| column[:name] == node[:name] && (node[:table] ? column[:table] == node[:table] : !column[:qualified_only]) }
        if matches.length > 1
          depth = matches.map { |column| column[:binding_scope] || @scope_depth }.max
          matches = matches.select { |column| (column[:binding_scope] || @scope_depth) == depth }
          if @validate_names && matches.length > 1 && matches.map { |column| [column[:name], column[:table]] }.uniq.length > 1
            raise Error.new('42702', "ambiguous column #{node[:name]}")
          end
        end
        if matches.empty? && @validate_names
          code = node[:table] && scope.none? { |column| column[:table] == node[:table] } ? '42P01' : '42703'
          raise Error.new(code, "unknown column #{node[:name]}")
        end
        @correlated = true if matches.first&.dig(:binding_external)
        matches.first&.dig(:type) || UNKNOWN
      when :cast
        type = Types.normalize(node[:type])
        constrain(node[:expr], type, node[:type_mod])
        expression(node[:expr], scope, type)
        type
      when :binary then binary(node, scope, wanted)
      when :unary
        op = node[:op].to_s
        arg = expression(node[:expr] || node[:operand], scope, op == 'not' ? 'boolean' : op.start_with?('is ') ? nil : wanted)
        op == 'not' || op.start_with?('is ') ? 'boolean' : arg
      when :call, :function, :srf then function(node, scope, wanted)
      when :json_value, :json_query, :json_exists
        expression(node[:context], scope, 'jsonb')
        expression(node[:path], scope, 'jsonpath')
        (node[:passing] || []).each { |binding| expression(binding[:expr], scope) }
        Types.normalize(node[:returning] || (node[:kind] == :json_exists ? 'boolean' : node[:kind] == :json_query ? 'jsonb' : 'text'))
      when :json_constructor
        expression(node[:expr], scope, 'text')
        'json'
      when :case
        operand = expression(node[:operand], scope) if node[:operand]
        values = node[:branches].map do |condition, value|
          expression(condition, scope, operand || 'boolean')
          value
        end
        values << node[:else] if node[:else]
        unify(values, scope, wanted)
      when :in
        nodes = [node[:expr] || node[:left]] + (node[:values] || node[:list] || [])
        unify(nodes, scope)
        statement(node[:query], scope) if node[:query]
        'boolean'
      when :between
        unify([node[:expr], node[:low] || node[:lower], node[:high] || node[:upper]], scope)
        'boolean'
      when :quantified
        left = expression(node[:left], scope)
        right = expression(node[:right], scope, known?(left) && left != 'record' ? left + '[]' : nil)
        expression(node[:left], scope, right[0...-2]) if right.end_with?('[]')
        'boolean'
      when :array
        args = node[:args] || node[:elements] || node[:values] || []
        element = wanted&.end_with?('[]') ? wanted[0...-2] : nil
        unify(args, scope, element).sub(/\[\]\z/, '') + '[]'
      when :row
        (node[:args] || node[:elements] || node[:values] || []).each { |arg| expression(arg, scope) }
        'record'
      when :subquery, :exists, :array_query
        columns = statement(node[:query], scope)
        return 'boolean' if node[:kind] == :exists
        type = columns.first&.dig(:type) || UNKNOWN
        node[:kind] == :array_query ? type + '[]' : type
      when :collate then expression(node[:expr], scope, wanted)
      when :field
        type = expression(node[:expr], scope)
        @engine.instance_variable_get(:@types)[type]&.find { |field| field[:name] == node[:name] }&.dig(:type) || UNKNOWN
      when :subscript
        type = expression(node[:expr], scope)
        (node[:specs] || []).each { |spec| spec.each_value { |v| expression(v, scope, 'i64') if v.is_a?(Hash) } }
        (node[:specs] || []).any? { |spec| spec[:slice] } ? type : type.sub(/\[\]\z/, '')
      else
        node.each_value { |value| expression(value, scope) if value.is_a?(Hash) && value[:kind] }
        UNKNOWN
      end
    end

    def binary(node, scope, wanted)
      op = node[:op].to_s.downcase
      left, right = node.values_at(:left, :right)
      if TEXT_OPERATORS.include?(op) || %w[and or].include?(op)
        target = TEXT_OPERATORS.include?(op) ? 'text' : 'boolean'
        types = [expression(left, scope, target), expression(right, scope, target)]
        types << expression(node[:escape], scope, 'text') if node[:escape]
        raise Error.new('42804', "#{op} requires #{target}") if types.any? { |type| known?(type) && type != target }
        return 'boolean'
      end
      lt, rt = expression(left, scope), expression(right, scope)
      numeric = %w[i16 i32 i64 decimal f32 f64]
      if left[:kind] == :literal && left[:untyped] && !%i[literal param].include?(right[:kind]) && numeric.include?(lt) && numeric.include?(rt) && !(lt == 'decimal' && Types::INTEGER_BITS.key?(rt))
        lt = rt
      elsif right[:kind] == :literal && right[:untyped] && !%i[literal param].include?(left[:kind]) && numeric.include?(lt) && numeric.include?(rt) && !(rt == 'decimal' && Types::INTEGER_BITS.key?(lt))
        rt = lt
      end
      if op == '||'
        anchor = [lt, rt].find { |type| type.end_with?('[]') }
        anchor ||= 'jsonb' if lt == 'jsonb' && rt == 'jsonb'
        anchor ||= 'bytea' if [lt, rt].include?('bytea') && [[left, lt], [right, rt]].all? { |arg, type| %w[bytea unknown].include?(type) || arg[:kind] == :literal && arg[:untyped] }
        anchor ||= 'text'
        expression(left, scope, anchor) if left[:kind] == :param
        expression(right, scope, anchor) if right[:kind] == :param
        return anchor
      end
      lt = expression(left, scope, rt) if known?(rt) && rt != 'record' && %i[param array].include?(left[:kind])
      rt = expression(right, scope, lt) if known?(lt) && lt != 'record' && %i[param array].include?(right[:kind])
      unless known?(lt) || known?(rt)
        lt = expression(left, scope, wanted || 'i64')
        rt = expression(right, scope, wanted || 'i64')
      end
      return 'boolean' if COMPARISONS.include?(op)
      return 'text' if %w[->> #>>].include?(op)
      return lt if %w[-> #> #-].include?(op)
      @engine.respond_to?(:binary_type) ? @engine.binary_type(op, lt, rt) : common(lt, rt)
    end

    def unify(nodes, scope, wanted = nil)
      types = nodes.map { |node| expression(node, scope) }
      type = types.reduce(wanted || UNKNOWN) { |a, b| common(a, b) }
      nodes.each { |node| expression(node, scope, type) } if known?(type)
      type
    end

    def common(left, right)
      left, right = left || UNKNOWN, right || UNKNOWN
      return right unless known?(left)
      return left unless known?(right)
      return left if left == right
      Types.promote(left, right)
    end

    def known?(type) = type && ![UNKNOWN, 'unknown[]'].include?(type)

    def function(node, scope, wanted)
      name, args = node[:name].downcase, node[:args] || []
      if node[:within_group] && %w[mode percentile_disc percentile_cont].include?(name)
        args.each { |arg| expression(arg, scope) }
        type = expression(node[:within_group].first[:expr], scope)
        type = 'f64' if name == 'percentile_cont' && type != 'interval'
        type += '[]' if args.first && expression(args.first, scope).end_with?('[]')
        return type
      end
      if %w[coalesce greatest least nullif].include?(name)
        return unify(args, scope, wanted)
      end
      types = args.map { |arg| expression(arg, scope) }
      signatures = FunctionCatalog::SIGNATURES[name]
      if signatures
        candidates = signatures.select do |families, _result, promote, variadic, defaults|
          next false unless variadic ? args.length >= families.length - 1 : args.length.between?(families.length - defaults, families.length)
          types.each_with_index.all? do |type, i|
            family = families[[i, families.length - 1].min]
            !known?(type) || family_match?(family, type, promote, types)
          end
        end
        if candidates.empty?
          conflict = args.each_with_index.any? do |arg, index|
            arg[:kind] == :param && known?(types[index]) && signatures.none? do |signature|
              family = signature[0][[index, signature[0].length - 1].min]
              family_match?(family, types[index], signature[2], types)
            end
          end
          raise Error.new(conflict ? '42804' : '42883', "function #{name} has no matching overload")
        end
        args.each_with_index do |arg, index|
          next unless arg[:kind] == :param && !known?(types[index])
          choices = candidates.map { |signature| signature[0][[index, signature[0].length - 1].min] }.uniq
          if choices.include?('text')
            candidates.select! { |signature| signature[0][[index, signature[0].length - 1].min] == 'text' }
            choices = ['text']
          end
          if choices.length > 1
            raise Error.new('42725', "function #{name} is ambiguous") if @final
            next
          end
          family = choices.first
          next if family.start_with?('any')
          target = { 'integer' => 'i64', 'float' => 'f64' }.fetch(family, family)
          types[index] = expression(arg, scope, target)
        end
      end
      expression(node[:filter], scope, 'boolean') if node[:filter]
      window_expressions(node[:over], scope) if node[:over].is_a?(Hash)
      (node[:order_by] || []).each { |order| expression(order[:expr], scope) }
      Functions.return_type(name, types)
    end

    def family_match?(family, type, promote, types)
      return true if %w[any anycompatible].include?(family)
      return type.end_with?('[]') if %w[anyarray anycompatiblearray].include?(family)
      return Types::RANGES.key?(type) if family == 'anyrange'
      actual = Functions.family(type)
      actual == family || promote && family == 'decimal' && actual == 'integer' && types.include?('decimal')
    end

    def window_expressions(window, scope)
      (window[:partition_by] || []).each { |expr| expression(expr, scope) }
      (window[:order_by] || []).each { |order| expression(order[:expr], scope) }
    end
  end

  module Bindings
    def query_metadata(statement, outer = [])
      binding = ParameterBindings.new(self, statement)
      columns = binding.query_columns(statement, outer, parameter_types: @native_binding_types)
      { columns: columns, correlated: binding.correlated }
    end

    def dispatch(statement)
      return super if @native_binding_active
      owns_binding = true
      @native_binding_plans ||= {}.compare_by_identity
      schema = [@tables.map { |name, table| [name, table[:columns].object_id] },
                @types.map { |name, fields| [name, fields.object_id] },
                @views.map { |name, view| [name, view.object_id] }]
      entry = @native_binding_plans[statement]
      if entry && entry[0] == schema
        binding = entry[1]
      else
        binding = ParameterBindings.new(self, statement)
        binding.validate_schema_parameters
        raise Error.new('42601', 'bind parameter count mismatch') unless @params.length == binding.count
        binding.resolve
        @native_binding_plans.shift if @native_binding_plans.length >= 64
        @native_binding_plans[statement] = [schema, binding]
      end
      raise Error.new('42601', 'bind parameter count mismatch') unless @params.length == binding.count
      @params = binding.bind(@params) unless binding.count.zero?
      @native_binding_types = binding.types
      @native_binding_active = true
      super
    ensure
      if owns_binding
        @native_binding_active = false
        @native_binding_types = nil
      end
    end

    def compile(node, columns)
      if node[:kind] == :param && @native_binding_types
        index = node[:index] - 1
        return Expression.new(kind: :param, type: @native_binding_types[index], value: index)
      end
      super
    end
  end
  Engine.prepend(Bindings)
end
