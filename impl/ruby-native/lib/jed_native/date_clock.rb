# frozen_string_literal: true

module JedNative
  module DateClock
    OFFSETS = { 'today' => 0, 'now' => 0, 'tomorrow' => 1, 'yesterday' => -1 }.freeze

    def compile(node, columns)
      if node[:kind] == :cast && node[:type] == 'date' && node.dig(:expr, :kind) == :literal && (offset = date_clock_offset(node[:expr][:value]))
        return Expression.new(kind: :date_clock, type: 'date', value: offset)
      end
      super
    end

    def contextual_binary(operator, left, right)
      return super if operator == '||'
      [[left, right], [right, left]].each do |constant, other|
        next unless constant.kind == :literal && constant.extra&.dig(:untyped) && constant.value.is_a?(String) && other.type == 'date'
        if (offset = date_clock_offset(constant.value))
          constant.kind, constant.type, constant.value = :date_clock, 'date', offset
        else
          constant.value = cast_value(constant.value, 'date')
          constant.type = 'date'
        end
      end
      super
    end

    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :date_clock
      charge
      date_clock_value(expression.value)
    end

    def assign(value, column, source = nil)
      if column[:type] == 'date' && (offset = date_clock_offset(value))
        unless source&.kind == :literal && source.extra&.dig(:untyped)
          raise Error.new('22007', 'clock-relative date data requires an explicit cast')
        end
        charge
        value = date_clock_value(offset)
      end
      super(value, column, source)
    end

    def cast_value(value, type, source_type: nil)
      if type == 'date' && source_type == 'text' && (offset = date_clock_offset(value))
        return date_clock_value(offset)
      end
      super(value, type, source_type: source_type)
    end

    def constant_expression?(expression)
      return false if expression.is_a?(Expression) && expression.kind == :date_clock
      super
    end

    private

    def date_clock_offset(value)
      OFFSETS[value.strip.downcase] if value.is_a?(String)
    end

    def date_clock_value(offset)
      charge
      local = resolve_timezone.instant_to_local(clock_value('now').micros)
      DateValue.new(local.div(TimestampValue::DAY) + offset)
    end

    def validate_index_dependencies(expression)
      reject_index_dependency if expression.kind == :date_clock || expression.kind == :cast && expression.type == 'interval' && expression.args[0].type == 'text' && expression.args[0].kind != :literal
      super
    end
  end
  Engine.prepend(DateClock)
end
