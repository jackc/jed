# frozen_string_literal: true

module JedNative
  module ExtendedExpressions
    def compile(node, columns)
      case node[:kind]
      when :subquery, :exists, :array_query
        outer = @outer.flat_map(&:first) + columns
        metadata = query_metadata(node[:query], outer)
        kind = node[:kind]
        if kind != :exists && metadata[:columns].length != 1
          raise Error.new('42601', 'subquery must return exactly one column')
        end
        type = kind == :exists ? 'boolean' : metadata[:columns][0][:type]
        type = 'text' if type == 'unknown'
        type += '[]' if kind == :array_query
        if metadata[:correlated]
          Expression.new(kind: kind, type: type, value: node[:query], extra: { scope: columns, negated: node[:negated] })
        else
          relation = select(node[:query])
          value = case kind
          when :exists then node[:negated] ? relation.rows.empty? : !relation.rows.empty?
          when :array_query then ArrayValue.new(relation.rows.map(&:first), element_type: type[0...-2])
          else
            raise Error.new('21000', 'scalar subquery returned multiple rows') if relation.rows.length > 1
            relation.rows.first&.first
          end
          Expression.new(kind: :literal, type: type, value: value)
        end
      when :collate
        raise Error.new('42704', "collation #{node[:name]} is not loaded") unless node[:name] == 'C'
        super(node[:expr], columns)
      when :field
        value = compile(node[:expr], columns)
        fields = @types[value.type]
        if value.type == 'record' && value.kind == :row
          fields = value.args.each_with_index.map { |e, i| { name: "f#{i + 1}", type: e.type } }
        end
        raise Error.new('42804', 'field access needs a composite') unless fields
        index = fields.index { |f| f[:name] == node[:name] }
        raise Error.new('42703', "unknown field #{node[:name]}") unless index
        Expression.new(kind: :field, type: fields[index][:type], value: index, args: [value])
      when :subscript
        value = compile(node[:expr], columns)
        raise Error.new('42804', 'subscript needs an array') unless value.type.end_with?('[]')
        specs = node[:specs].map do |spec|
          spec.transform_values { |v| v.is_a?(Hash) ? compile(v, columns) : v }
        end
        slice = specs.any? { |s| s[:slice] }
        Expression.new(kind: :subscript, type: slice ? value.type : value.type[0...-2], args: [value], extra: specs)
      when :quantified
        Expression.new(kind: :quantified, type: 'boolean', args: [compile(node[:left], columns), compile(node[:right], columns)], extra: node)
      when :is_json
        Expression.new(kind: :is_json, type: 'boolean', args: [compile(node[:expr], columns)], extra: node)
      when :binary
        if node[:escape] && %w[like ilike not\ like not\ ilike].include?(node[:op])
          node = node.merge(right: { kind: :call, name: 'like_escape', args: [node[:right], node[:escape]] }).reject { |key, _| key == :escape }
        end
        expression = super
        left, right = expression.args
        # Untyped constants adopt the declared type at their use site; a typed
        # expression never gets this permission merely because it is constant.
        contextual_binary(expression.value, left, right)
        expression.type = binary_type(expression.value, left.type, right.type)
        if Types::INTEGER_BITS.key?(left.type) && Types::INTEGER_BITS.key?(right.type) && %w[+ - * / % = <> != < <= > >=].include?(expression.value)
          bits = Types::INTEGER_BITS[expression.type]
          expression.extra = { integer_kernel: true, minimum: bits && -(1 << (bits - 1)), maximum: bits && (1 << (bits - 1)) - 1 }
        end
        expression
      else super
      end
    end

    def contextual_binary(op, left, right)
      return if %w[-> ->> #> #>> ? ?| ?& ||].include?(op)
      comparison = %w[= <> != < <= > >= is\ distinct\ from is\ not\ distinct\ from].include?(op)
      [[left, right], [right, left]].each do |constant, other|
        next unless constant.kind == :param || constant.kind == :literal && constant.extra&.dig(:untyped)
        next if %i[literal param].include?(other.kind)
        numeric = %w[i16 i32 i64 f32 f64 decimal]
        next unless comparison || numeric.include?(constant.type) && numeric.include?(other.type)
        if constant.kind == :literal
          if Types::INTEGER_BITS.key?(constant.type) && other.type == 'decimal'
            constant.extra = (constant.extra || {}).merge(native_integer_bound: true)
          end
          constant.value = cast_value(constant.value, other.type)
        else
          cast_value(@params[constant.value], other.type)
          constant.extra = { context_type: true }
        end
        constant.type = other.type
      end
    end

    def binary_type(op, left, right)
      return 'boolean' if %w[= <> != < <= > >= and or like ilike not\ like not\ ilike ~ ~* !~ !~* is\ distinct\ from is\ not\ distinct\ from @> <@ && ? ?| ?& -|- << >> &< &> @? @@].include?(op)
      return 'text' if %w[->> #>>].include?(op)
      return left if %w[-> #> #-].include?(op)
      if op == '||' && (left.end_with?('[]') || right.end_with?('[]'))
        a = left.end_with?('[]') ? left[0...-2] : left
        b = right.end_with?('[]') ? right[0...-2] : right
        return "#{common_type(a, b)}[]"
      end
      return 'text' if op == '||' && (left == 'text' || right == 'text')
      return 'i32' if left == 'date' && right == 'date' && op == '-'
      return 'date' if left == 'date' && Types::INTEGER_BITS.key?(right)
      return 'interval' if %w[timestamp timestamptz].include?(left) && %w[timestamp timestamptz].include?(right) && op == '-'
      return left == 'date' ? 'timestamp' : left if %w[date timestamp timestamptz].include?(left) && right == 'interval'
      common_type(left, right)
    end

    def evaluate(expr, row, group = nil)
      case expr.kind
      when :field
        charge
        value = evaluate(expr.args[0], row, group)
        value && value[expr.value]
      when :subscript
        charge
        value = evaluate(expr.args[0], row, group)
        return nil unless value
        if value.respond_to?(:dimensions) && !expr.extra.any? { |spec| spec[:slice] }
          return nil unless value.dimensions.length == expr.extra.length
          offset = 0
          expr.extra.each_with_index do |spec, axis|
            index = evaluate(spec[:index], row, group)
            base = value.lower_bounds[axis]
            return nil unless index && index >= base && index < base + value.dimensions[axis]
            offset = offset * value.dimensions[axis] + index - base
          end
          return value[offset]
        end
        expr.extra.each do |spec|
          base = value.respond_to?(:lower_bound) ? value.lower_bound : 1
          if spec[:slice]
            low = spec[:low] ? evaluate(spec[:low], row, group) : base
            high = spec[:high] ? evaluate(spec[:high], row, group) : base + value.length - 1
            return nil unless low && high
            low, high = [low - base, 0].max, [high - base, value.length - 1].min
            value = ArrayValue.new(low > high ? [] : value[low..high], element_type: expr.type[0...-2])
          else
            index = evaluate(spec[:index], row, group)
            return nil unless index && index >= base && index < base + value.length
            value = value[index - base]
          end
          return nil if value.nil?
        end
        value
      when :quantified
        charge
        left = evaluate(expr.args[0], row, group)
        right_expr = expr.args[1]
        values = if right_expr.kind == :subquery
          correlated(right_expr.extra[:scope], row) { select(right_expr.value).rows.map(&:first) }
        else
          evaluate(right_expr, row, group)
        end
        return nil if values.nil?
        all = expr.extra[:quantifier] == 'all'
        unknown = false
        values.each do |right|
          value = Types.binary(expr.extra[:op], left, right, type: 'boolean')
          return !all if value == !all
          unknown ||= value.nil?
        end
        unknown ? nil : all
      when :array_query
        rel = correlated(expr.extra[:scope], row) { select(expr.value) }
        raise Error.new('42601', 'ARRAY subquery must return one column') unless rel.columns.length == 1
        ArrayValue.new(rel.rows.map(&:first), element_type: rel.columns[0][:type])
      when :is_json
        value = evaluate(expr.args[0], row, group)
        return nil if value.nil?
        begin
          json = value.is_a?(JsonValue) ? value : JsonValue.parse(value.to_s)
          kind = expr.extra[:json_kind]
          answer = kind.nil? || kind == 'value' || (kind == 'array' && json.value.is_a?(Array)) || (kind == 'object' && json.value.is_a?(Hash)) || (kind == 'scalar' && !json.value.is_a?(Array) && !json.value.is_a?(Hash))
        rescue Error
          answer = false
        end
        expr.extra[:negated] ? !answer : answer
      else super
      end
    end
  end
  Engine.prepend(ExtendedExpressions)
end
