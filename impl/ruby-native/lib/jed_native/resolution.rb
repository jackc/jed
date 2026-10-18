# frozen_string_literal: true

module JedNative
  module Resolution
    COMPARISONS = %w[= <> != < <= > >= is\ distinct\ from is\ not\ distinct\ from].freeze
    FLOAT_OPERATORS = (COMPARISONS + %w[+ - * / % ^]).freeze

    def compile(node, columns)
      if node[:kind] == :in && !node[:query]
        left = node[:expr] || node[:left]
        comparisons = (node[:values] || node[:list] || []).map { |right| { kind: :binary, op: '=', left: left, right: right } }
        body = comparisons.reduce { |a, b| { kind: :binary, op: 'or', left: a, right: b } }
        return compile(node[:negated] ? { kind: :unary, op: 'not', expr: body } : body, columns) if body
      elsif node[:kind] == :between
        left = node[:expr]
        low = { kind: :binary, op: '>=', left: left, right: node[:low] || node[:lower] }
        high = { kind: :binary, op: '<=', left: left, right: node[:high] || node[:upper] }
        body = { kind: :binary, op: 'and', left: low, right: high }
        return compile(node[:negated] ? { kind: :unary, op: 'not', expr: body } : body, columns)
      end
      expression = super
      case expression.kind
      when :array
        expression.type = expression.type.sub(/(?:\[\])+\z/, '') + '[]'
      when :cast
        arg = expression.args.first
        if arg.kind == :literal && arg.value.is_a?(String)
          value = cast_value(arg.value, expression.type, source_type: arg.type)
          value = explicit_modifier(value, expression.type, expression.extra[:type_mod])
          return Expression.new(kind: :literal, type: expression.type, value: value)
        end
      when :binary
        left, right = expression.args
        if expression.value == '||' && !expression.args.any? { |arg| arg.type.end_with?('[]') }
          if expression.args.any? { |arg| arg.type == 'bytea' } && expression.args.all? { |arg| %w[bytea unknown].include?(arg.type) || arg.kind == :literal && arg.extra&.dig(:untyped) }
            expression.args.each do |arg|
              if arg.type != 'bytea'
                arg.value = cast_value(arg.value, 'bytea') if arg.kind == :literal
                arg.type = 'bytea'
              end
            end
            expression.type = 'bytea'
          elsif expression.type == 'text'
            expression.args = expression.args.map do |arg|
              if %w[text unknown].include?(arg.type)
                arg
              else
                Expression.new(kind: :cast, type: 'text', args: [arg])
              end
            end
          end
        end
        if COMPARISONS.include?(expression.value)
          validate_comparison(left, right)
        end
        if FLOAT_OPERATORS.include?(expression.value) && [left.type, right.type].sort == %w[f32 f64]
          expression.args = expression.args.map { |arg| widen_float(arg) }
        end
      when :call
        if expression.value == 'nullif'
          validate_comparison(*expression.args)
          expression.type = expression.args.first.type
        elsif expression.type == 'f64' && !Functions.aggregate?(expression.value)
          expression.args = expression.args.map { |arg| widen_float(arg) }
        end
      when :case
        operand, branches = expression.args
        branches.each { |test, _| validate_comparison(operand, test) } if operand
      when :quantified
        left, right = expression.args
        if right.type.end_with?('[]')
          validate_comparison(left, Expression.new(type: right.type[0...-2], kind: :element))
        end
      end
      expression
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :cast && expression.extra&.dig(:type_mod)
        charge
        value = cast_value(evaluate(expression.args.first, row, group), expression.type, source_type: expression.args.first.type)
        source = expression.args.first.type
        if !constant_expression?(expression) && expression.type == 'text' && (source.end_with?('[]') || %w[bytea jsonb].include?(source))
          charge(LogicalMemory.payload(value, expression.type))
        end
        return explicit_modifier(value, expression.type, expression.extra[:type_mod])
      end
      if expression.kind == :binary && expression.value == '||' && expression.type == 'bytea'
        charge
        left, right = expression.args.map { |arg| evaluate(arg, row, group) }
        return left.nil? || right.nil? ? nil : Bytea.new(left + right)
      end
      if expression.kind == :case && expression.args[0]
        charge
        operand, branches, fallback = expression.args
        value = evaluate(operand, row, group)
        branches.each do |condition, result|
          charge
          charge if [operand.type, condition.type].sort == %w[f32 f64]
          test = evaluate(condition, row, group)
          if !value.nil? && !test.nil? && Types.compare(value, test).zero?
            return cast_value(evaluate(result, row, group), expression.type)
          end
        end
        return cast_value(evaluate(fallback, row, group), expression.type)
      end
      if expression.kind == :binary && COMPARISONS.include?(expression.value) && expression.args.all? { |arg| arg.type == 'record' }
        charge
        left, right = expression.args.map { |arg| evaluate(arg, row, group) }
        return compare_sql_rows(expression.value, left, right)
      end
      super
    end

    private

    def explicit_modifier(value, type, modifier)
      return value if value.nil? || modifier.nil?
      if type == 'text'
        value[0, modifier.is_a?(Hash) ? modifier[:precision] : Array(modifier).first]
      elsif type == 'decimal'
        precision, scale = modifier.is_a?(Hash) ? [modifier[:precision], modifier[:scale]] : Array(modifier)
        result = value.round(scale || 0)
        raise Error.new('22003', 'numeric field overflow') if result.abs >= 10**(precision - (scale || 0))
        result
      else
        value
      end
    end

    def widen_float(expression)
      expression.type == 'f32' ? Expression.new(kind: :cast, type: 'f64', args: [expression]) : expression
    end

    def validate_comparison(left, right)
      return if left.type == 'unknown' || right.type == 'unknown'
      ensure_comparable_type(left.type)
      ensure_comparable_type(right.type)
      if left.type == 'record' && right.type == 'record'
        if left.kind == :row && right.kind == :row
          raise Error.new('42601', 'row comparison widths differ') unless left.args.length == right.args.length
          left.args.zip(right.args).each { |a, b| validate_comparison(a, b) }
        end
      else
        common_type(left.type, right.type)
      end
    end

    def ensure_comparable_type(type, seen = {})
      raise Error.new('42883', "type #{type} has no comparison operator") if %w[json jsonpath].include?(type)
      return ensure_comparable_type(type[0...-2], seen) if type.end_with?('[]')
      return if seen[type]
      seen[type] = true
      (@types[type] || []).each { |field| ensure_comparable_type(field[:type], seen) }
    end

    def compare_sql_rows(operator, left, right)
      distinct = operator.include?('distinct')
      if left.nil? || right.nil?
        return nil unless distinct
        equal = left.nil? && right.nil?
        return operator == 'is distinct from' ? !equal : equal
      end
      if distinct
        equal = left.zip(right).all? { |a, b| a.nil? || b.nil? ? a.nil? && b.nil? : Types.compare(a, b).zero? }
        return operator == 'is distinct from' ? !equal : equal
      end
      return nil if left.nil? || right.nil?
      equality = %w[= <> !=].include?(operator)
      unknown = false
      left.zip(right).each do |a, b|
        if a.nil? || b.nil?
          return nil unless equality
          unknown = true
          next
        end
        comparison = Types.compare(a, b)
        next if comparison.zero?
        return operator != '=' if equality
        return %w[< <=].include?(operator) ? comparison.negative? : comparison.positive?
      end
      return nil if unknown
      %w[= <= >=].include?(operator)
    end
  end
  Engine.prepend(Resolution)
end
