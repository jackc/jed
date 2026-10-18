# frozen_string_literal: true

module JedNative
  # The SQL/JSON constructs reuse the path interpreter. Their error boundaries
  # are deliberately outside context/PASSING conversion and DEFAULT evaluation.
  module JsonSql
    def compile(node, columns)
      return super unless %i[json_value json_query json_exists].include?(node[:kind])
      spec = compile_json_spec(node, columns)
      spec[:context] = compile(node[:context], columns)
      json_format_validate(spec[:context].type, node[:format_json])
      unless %w[text json jsonb bytea null unknown].include?(spec[:context].type)
        raise Error.new('42804', 'JSON context requires JSON or text')
      end
      spec[:path] = compile(node[:path], columns)
      unless %w[text jsonpath null unknown].include?(spec[:path].type)
        raise Error.new('42804', 'JSON path requires a jsonpath value')
      end
      if spec[:path].kind == :literal && !spec[:path].value.nil?
        spec[:path] = Expression.new(kind: :literal, type: 'jsonpath', value: JsonPathValue.parse(spec[:path].value))
      end
      spec[:passing] = compile_json_passing(node[:passing], columns)
      Expression.new(kind: :json_sql, type: spec[:type], args: [spec[:context], spec[:path]] + spec[:passing].map { |binding| binding[:expression] }, extra: spec)
    end

    def compile_json_spec(node, columns, table_column: false)
      kind = node[:kind]
      type = Types.normalize(node[:returning] || (kind == :json_exists ? 'boolean' : kind == :json_query ? 'jsonb' : 'text'))
      wrapper = node[:wrapper] || :without
      if kind == :json_query && (!%w[json jsonb].include?(type) || node[:keep_quotes] == false)
        raise Error.new('0A000', 'JSON_QUERY scalar RETURNING and OMIT QUOTES are not supported')
      end
      if table_column && (type.end_with?('[]') || @types.key?(type))
        raise Error.new('0A000', 'JSON_TABLE container column types are not supported')
      end
      spec = node.merge(type: type, wrapper: wrapper)
      %i[on_empty on_error].each do |clause|
        behavior = node[clause] || { kind: kind == :json_exists ? :false : :null }
        accepted = if kind == :json_exists
          %i[error true false unknown]
        elsif kind == :json_query
          %i[error null empty_array empty_object default]
        else
          %i[error null default]
        end
        if kind == :json_exists && clause == :on_empty && node[clause] || !accepted.include?(behavior[:kind])
          raise Error.new('42601', 'invalid SQL/JSON behavior')
        end
        spec[clause] = behavior[:kind] == :default ? compile_json_default(behavior, type, columns) : behavior
      end
      spec
    end

    def compile_json_default(behavior, type, columns)
      node = behavior[:default]
      shape = node
      shape = shape[:expr] while %i[cast collate].include?(shape[:kind])
      unless %i[literal call binary unary].include?(shape[:kind]) && !(%i[binary unary].include?(shape[:kind]) && %w[and or not].include?(shape[:op]))
        raise Error.new('42804', 'DEFAULT requires a constant, function, or operator expression')
      end
      json_validate_default_nodes(node)
      value = compile(node, columns)
      if value.kind == :literal && value.type == 'text' && node[:kind] == :literal
        value = Expression.new(kind: :literal, type: type, value: cast_value(value.value, type))
      elsif value.type != type && !%w[null unknown jsonb boolean].include?(value.type)
        if %w[json jsonb].include?(type) && !%w[json jsonb text].include?(value.type) || value.type == 'json' && !%w[json jsonb text].include?(type)
          raise Error.new('42846', 'cannot cast DEFAULT to the RETURNING type')
        end
        value = Expression.new(kind: :cast, type: type, args: [value])
      end
      behavior.merge(expression: value)
    end

    def json_validate_default_nodes(node)
      case node
      when Hash
        case node[:kind]
        when :column then raise Error.new('42804', 'DEFAULT cannot contain column references')
        when :subquery, :exists, :array_query then raise Error.new('0A000', 'DEFAULT cannot contain subqueries')
        when :param then raise Error.new('42P02', 'DEFAULT cannot contain parameters')
        when :call
          raise Error.new('42803', 'DEFAULT cannot contain aggregate or window calls') if Functions.aggregate?(node[:name]) || node[:over]
        end
        node.each_value { |child| json_validate_default_nodes(child) }
      when Array then node.each { |child| json_validate_default_nodes(child) }
      end
    end

    def json_format_validate(type, format)
      return unless format.is_a?(String)
      encoding = format.upcase
      raise Error.new('22023', 'unrecognized JSON encoding') unless %w[UTF8 UTF16 UTF32].include?(encoding)
      raise Error.new('42804', 'JSON ENCODING requires bytea input') unless type == 'bytea'
      raise Error.new('22023', 'invalid source encoding name') unless encoding == 'UTF8'
    end

    def compile_json_passing(passing, columns)
      (passing || []).map do |binding|
        expr = compile(binding[:expr], columns)
        json_format_validate(expr.type, binding[:format_json])
        if !binding[:format_json] && %w[date timestamp timestamptz].include?(expr.type)
          raise Error.new('0A000', 'JSON path datetime items are not supported')
        end
        binding.merge(expression: expr)
      end
    end

    def json_document(value)
      return nil if value.nil?
      if value.is_a?(Bytea)
        text = value.dup.force_encoding(Encoding::UTF_8)
        raise Error.new('22021', 'invalid byte sequence for encoding UTF8') unless text.valid_encoding? && !text.include?("\0")
        value = text
      end
      return JsonValue.parse(value, binary: true).value if value.is_a?(JsonValue) || value.is_a?(String)
      raise Error.new('42804', 'JSON context requires JSON or text')
    end

    def evaluate_json_passing(bindings, row, group = nil)
      result = {}
      bindings.each do |binding|
        value = evaluate(binding[:expression], row, group)
        format = binding[:format_json]
        type = binding[:expression].type
        item = if format && (value.is_a?(String) || value.is_a?(JsonValue))
          charge if value.is_a?(String) && !value.is_a?(UUID)
          json_document(value)
        elsif !format && value.is_a?(Float)
          raise Error.new('0A000', 'nonfinite JSON path numbers are not supported') unless value.finite?
          Decimal.parse(Kernel.format(type == 'f32' ? '%.6g' : '%.15g', value))
        else
          json_image_value(value, type, binding[:expression]).value
        end
        result[binding[:name]] = item unless result.key?(binding[:name])
      end
      result
    end

    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :json_sql
      spec = expression.extra
      charge
      document = evaluate(spec[:context], row, group)
      path = evaluate(spec[:path], row, group)
      return nil if document.nil? || path.nil?
      document = json_document(document)
      path = JsonPathValue.parse(path)
      vars = evaluate_json_passing(spec[:passing], row, group)
      json_path_result(spec, document, path, vars, row, group, constant: respond_to?(:constant_expression?) && constant_expression?(expression))
    end

    def json_path_result(spec, document, path, vars, row, group = nil, source_text: nil, constant: false)
      empty = false
      begin
        evaluator = JsonPathEvaluator.new(path, document, vars: vars, context: self, passing: true,
                                          variable_bases: vars.length, source_text: source_text)
        items = evaluator.run(exists: spec[:kind] == :json_exists)[:items]
        charge(items.length) unless source_text
        if spec[:kind] == :json_exists
          value = !items.empty?
          return spec[:type] == 'boolean' ? value : cast_value(value, spec[:type])
        end
        if spec[:kind] == :json_query && (spec[:wrapper] == :unconditional || spec[:wrapper] == :conditional && items.length != 1)
          charge(JsonValue.payload(items)) unless constant
          return json_result_value(items, spec[:type])
        end
        empty = items.empty?
        unless empty
          raise Error.new('22034', 'JSON path returned more than one item') if items.length > 1
          if spec[:kind] == :json_query
            return json_result_value(items[0], spec[:type])
          end
          return json_scalar_value(items[0], spec[:type], spec[:type_mod])
        end
      rescue Error => error
        raise unless error.sqlstate.start_with?('22')
        return json_behavior(spec[:on_error], spec, row, group, error)
      end
      json_behavior(spec[:on_empty], spec, row, group, Error.new('22035', 'JSON path returned no items')) if empty
    end

    def json_result_value(value, type)
      result = JsonValue.new(value)
      type == 'json' ? JsonValue.new(value, binary: false, source: result.to_s) : result
    end

    def json_scalar_value(value, type, modifier = nil)
      return nil if value.nil?
      raise Error.new('2203F', 'JSON_VALUE requires a scalar') if value.is_a?(Array) || value.is_a?(Hash)
      return json_result_value(value, type) if %w[json jsonb].include?(type)
      if value.is_a?(String)
        result = cast_value(value, type)
      elsif type == 'decimal' && (value.is_a?(Decimal) || value.is_a?(Integer))
        result = Decimal.parse(value)
      else
        result = cast_value(Types.format(value), type)
      end
      if modifier && type == 'decimal'
        result = Types.cast(result, "decimal(#{modifier[:precision]},#{modifier[:scale] || 0})")
      end
      result
    rescue Error => error
      raise if error.sqlstate == '2203F' || %w[date timestamp timestamptz interval].include?(type)
      raise Error.new('2203A', error.message)
    end

    def json_behavior(behavior, spec, row, group, error)
      case behavior[:kind]
      when :error then raise error
      when :null, :unknown then nil
      when :true, :false
        value = behavior[:kind] == :true
        spec[:type] == 'boolean' ? value : cast_value(value, spec[:type])
      when :empty_array then json_result_value([], spec[:type])
      when :empty_object then json_result_value({}, spec[:type])
      when :default
        expr = behavior[:expression]
        value = evaluate(expr, row, group)
        return value if value.nil? || expr.type == spec[:type]
        if expr.type == 'jsonb' || expr.type == 'boolean' && spec[:type] != 'i32'
          begin
            item = value.is_a?(JsonValue) ? value.value : value
            %w[json jsonb].include?(spec[:type]) ? json_result_value(item, spec[:type]) : json_scalar_value(item, spec[:type], spec[:type_mod])
          rescue Error => failure
            raise Error.new('42804', failure.message)
          end
        else
          cast_value(value, spec[:type])
        end
      end
    end

    def json_table_columns(nodes)
      nodes.flat_map do |node|
        node[:kind] == :nested ? json_table_columns(node[:columns]) : [{ name: node[:name], type: node[:kind] == :ordinality ? 'i64' : Types.normalize(node[:type]) }]
      end
    end

    def compile_json_table_columns(nodes)
      nodes.map do |node|
        case node[:kind]
        when :nested
          node.merge(program: JsonPathValue.parse(node[:path]), columns: compile_json_table_columns(node[:columns]))
        when :ordinality then node
        else
          type = Types.normalize(node[:type])
          query = %w[json jsonb].include?(type) || node[:format_json]
          if !query && (node[:wrapper] != :without || node[:keep_quotes] == false)
            raise Error.new('0A000', 'JSON_TABLE scalar columns cannot use wrappers')
          end
          kind = node[:kind] == :exists ? :json_exists : query ? :json_query : :json_value
          spec = compile_json_spec(node.merge(kind: kind, returning: type), [], table_column: true)
          path = node[:path] || '$.' + JsonValue.escape(node[:name])
          node.merge(spec: spec, program: JsonPathValue.parse(path), path: path)
        end
      end
    end

    def json_table_expand(nodes, items, vars)
      rows = []
      width = json_table_columns(nodes).length
      items.each_with_index do |item, index|
        base = Array.new(width)
        nested = []
        offset = 0
        nodes.each do |node|
          if node[:kind] == :nested
            child_width = json_table_columns(node[:columns]).length
            nested << [node, offset, child_width]
            offset += child_width
          else
            base[offset] = if node[:kind] == :ordinality
              index + 1
            else
              json_path_result(node[:spec], item, node[:program], vars, [], nil, source_text: node[:path])
            end
            offset += 1
          end
        end
        if nested.empty?
          rows << base
        else
          nested.each do |node, position, child_width|
            evaluation = JsonPathEvaluator.new(node[:program], item, vars: vars, context: self,
              passing: true, variable_bases: vars.length, source_text: node[:path]).run(silent: true)
            children = evaluation[:error] ? [] : evaluation[:items]
            child_rows = json_table_expand(node[:columns], children, vars)
            child_rows = [Array.new(child_width)] if child_rows.empty?
            child_rows.each do |child|
              result = base.dup
              result[position, child_width] = child
              rows << result
            end
          end
        end
      end
      rows
    end

    def source_relation(source)
      node = source[:expr] || source
      return super unless node[:kind] == :json_table
      columns = json_table_columns(node[:columns])
      raise Error.new('42701', 'duplicate JSON_TABLE column') unless columns.map { |column| column[:name] }.uniq.length == columns.length
      tree = compile_json_table_columns(node[:columns])
      passing = compile_json_passing(node[:passing], [])
      context = compile(node[:context], [])
      path = compile(node[:path], [])
      json_format_validate(context.type, node[:format_json])
      vars = evaluate_json_passing(passing, [])
      document = evaluate(context, [])
      path_text = evaluate(path, [])
      rows = []
      unless document.nil? || path_text.nil?
        document = json_document(document)
        program = JsonPathValue.parse(path_text)
        evaluation = JsonPathEvaluator.new(program, document, vars: vars, context: self, passing: true,
          variable_bases: vars.length, source_text: path_text.to_s).run(silent: true)
        items = evaluation[:error] ? [] : evaluation[:items]
        rows = json_table_expand(tree, items, vars)
      end
      charge(rows.length)
      columns = columns.each_with_index.map { |column, i| column.merge(table: source[:alias] || 'json_table', name: source[:columns]&.[](i) || column[:name]) }
      Relation.new(columns: columns, rows: rows)
    end
  end
  Engine.prepend(JsonSql)
end
