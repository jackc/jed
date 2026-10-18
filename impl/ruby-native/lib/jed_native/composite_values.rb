# frozen_string_literal: true

module JedNative
  module CompositeExpressions
    def compile(node, columns)
      if node[:kind] == :cast
        target = Types.normalize(node[:type])
        child = node[:expr]
        source_type = if child[:kind] == :row
          'record'
        elsif child[:kind] == :cast
          Types.normalize(child[:type])
        elsif child[:kind] == :column
          columns.find { |column| column[:name] == child[:name] && (!child[:table] || column[:table] == child[:table]) }&.dig(:type)
        end
        if @types[target] && child[:kind] == :row
          fields = @types[target]
          children = child[:args] || child[:values] || child[:elements]
          raise Error.new('42846', 'record field count differs') unless fields.length == children.length
          args = fields.each_with_index.map do |field, index|
            compile({ kind: :cast, expr: children[index], type: field[:type], type_mod: field[:type_mod] }, columns)
          end
          return Expression.new(kind: :composite_construct, type: target, args: args)
        end
        if @types[target] && (source_type == 'record' || @types[source_type] && source_type != target) || (@types[source_type] || source_type == 'record') && !(@types[target] || target == 'text' || target == source_type)
          raise Error.new('42846', 'composite cast is not supported for these types')
        end
        if @types[target] && child[:kind] == :literal && child[:value].is_a?(String)
          value = cast_value(child[:value], target)
          return Expression.new(kind: :composite_literal, type: target, value: value)
        end
      end
      super
    rescue Error => error
      if node[:kind] == :field && error.sqlstate == '42804' && error.message.include?('field access needs')
        raise Error.new('42809', 'field access requires a composite')
      end
      raise
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :composite_literal
        charge
        return expression.value
      elsif expression.kind == :composite_construct
        charge
        values = expression.args.map { |argument| evaluate(argument, row, group) }
        return CompositeValue.new(values, type_name: expression.type, field_names: @types[expression.type].map { |field| field[:name] })
      elsif expression.kind == :row
        charge
        return CompositeValue.new(expression.args.map { |argument| evaluate(argument, row, group) })
      elsif expression.kind == :unary && %w[is\ null is\ not\ null].include?(expression.value) && (@types[expression.args[0].type] || expression.args[0].type == 'record')
        charge
        value = evaluate(expression.args[0], row, group)
        return value.nil? || value.all?(&:nil?) if expression.value == 'is null'
        return !value.nil? && value.none?(&:nil?)
      end
      super
    end

    def assign(value, column, source = nil)
      validate_composite_assignment(source, column[:type]) if source && @types[column[:type]]
      if value && (fields = @types[column[:type]]) && source&.kind == :row
        raise Error.new('42846', 'record field count differs') unless fields.length == value.length
        fields = fields.each_with_index.map { |field, index| assign(value[index], field, source.args[index]) }
        return CompositeValue.new(fields, type_name: column[:type], field_names: @types[column[:type]].map { |field| field[:name] })
      end
      super
    end

    def cast_value(value, type, source_type: nil)
      if value.is_a?(String) && (fields = @types[type])
        text = value.strip
        raise Error.new('22P02', 'malformed record literal') unless text.start_with?('(') && text.end_with?(')')
        values = text == '()' && fields.empty? ? [] : Types.split_container(text[1...-1])
        raise Error.new('22P02', 'record field count differs') unless fields.length == values.length
        values = fields.each_with_index.map do |field, index|
          parsed = cast_value(values[index], field[:type])
          assign(parsed, field)
        end
        return CompositeValue.new(values, type_name: type, field_names: fields.map { |field| field[:name] })
      end
      super
    end

    def dispatch(statement)
      if statement[:kind] == :create_type
        statement[:fields].each { |field| validate_type(Types.normalize(field[:type]), field[:type_mod]) }
        depth = statement[:fields].map { |field| composite_depth(field[:type]) }.max || 0
        raise Error.new('54001', 'composite nesting exceeds 32 levels') if depth >= 32
      end
      super
    end

    def create_table(statement)
      (statement[:columns] || []).each do |column|
        next unless @types[column[:type]] && column[:default]
        begin
          validate_composite_assignment(compile(column[:default], []), column[:type])
        rescue Error => error
          raise Error.new('0A000', 'composite defaults cannot reference columns') if %w[42703 42P01].include?(error.sqlstate)
          raise
        end
      end
      super
    end

    def contextual_binary(operator, left, right)
      [[left, right], [right, left]].each do |record, named|
        next unless @types[named.type]
        if record.kind == :row
          validate_composite_assignment(record, named.type)
          record.type = named.type
        elsif record.kind == :literal && record.extra&.dig(:untyped) && !record.value.nil? && !record.value.is_a?(String) && !@types[record.type]
          raise Error.new('42804', 'composite and scalar are not comparable')
        end
      end
      super
    end

    private

    def composite_depth(type)
      type = Types.normalize(type).delete_suffix('[]')
      fields = @types[type]
      fields ? 1 + (fields.map { |field| composite_depth(field[:type]) }.max || 0) : 0
    end

    def validate_composite_assignment(source, target, field: false)
      return if %w[unknown null].include?(source.type) || source.type == target
      if (fields = @types[target]) && source.kind == :row
        raise Error.new('42846', 'record field count differs') unless fields.length == source.args.length
        fields.zip(source.args).each { |column, child| validate_composite_assignment(child, column[:type], field: true) }
        return
      end
      return if source.kind == :literal && source.extra&.dig(:untyped) && source.value.is_a?(String)
      return if !@types[target] && %w[i16 i32 i64 decimal f32 f64].include?(target) && %w[i16 i32 i64 decimal f32 f64].include?(source.type)
      return if !@types[target] && target.end_with?('[]') && source.type.end_with?('[]')
      raise Error.new(field || source.type == 'record' ? '42846' : '42804', 'value is not assignable to composite field')
    end

    def validate_comparison(left, right)
      if left.kind == :row && right.kind == :row && left.args.length != right.args.length
        raise Error.new('42804', 'row comparison widths differ')
      end
      super
    end
  end
  Engine.prepend(CompositeExpressions)
end
