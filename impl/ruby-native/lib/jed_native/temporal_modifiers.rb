# frozen_string_literal: true

module JedNative
  module TemporalModifierSyntax
    def type_name
      start = @index
      type, modifier = super
      if %w[timestamp timestamptz interval timestamp[] timestamptz[] interval[]].include?(type) && modifier
        syntax('temporal precision takes one unsigned integer') if modifier[:scale] || modifier[:precision]&.negative?
        if type.start_with?('interval') && modifier[:fields] && @tokens[start + 1].value == '('
          syntax('interval precision cannot precede a qualifier')
        end
        modifier[:precision] = [modifier[:precision], 6].min if modifier[:precision]
        raise Error.new('0A000', 'array element type modifiers are not supported') if type.end_with?('[]')
      end
      [type, modifier]
    end

    def interval_modifier
      modifier = super
      qualifier = DateTimeUnits::INTERVAL_QUALIFIERS[modifier[:fields]]
      syntax('invalid interval qualifier') unless qualifier
      syntax('only SECOND may carry precision') if modifier[:precision] && qualifier[0] != 'second'
      syntax('precision must be unsigned') if modifier[:precision]&.negative?
      modifier[:precision] = [modifier[:precision], 6].min if modifier[:precision]
      modifier
    end
  end
  Parser.prepend(TemporalModifierSyntax)

  module TemporalModifiers
    def compile(node, columns)
      if node[:kind] == :cast && node[:type] == 'interval' && node[:type_mod] && node.dig(:expr, :kind) == :literal && node.dig(:expr, :value).is_a?(String)
        value = IntervalParser.parse(node[:expr][:value], fields: node[:type_mod][:fields])
        value = explicit_modifier(value, 'interval', node[:type_mod])
        return Expression.new(kind: :literal, type: 'interval', value: value)
      end
      super
    end

    def assign(value, column, source = nil)
      if column[:type] == 'interval' && column[:type_mod] && value.is_a?(String)
        value = IntervalParser.parse(value, fields: column[:type_mod][:fields])
      end
      result = super(value, column, source)
      explicit_modifier(result, column[:type], column[:type_mod])
    end

    private

    def explicit_modifier(value, type, modifier)
      return super unless value && modifier && %w[timestamp timestamptz interval].include?(type)
      precision = modifier[:precision]
      if value.is_a?(TimestampValue)
        return value if value.micros.is_a?(Float) || !precision
        epoch = 946_684_800_000_000
        micros = round_temporal(value.micros - epoch, precision) + epoch
        TimestampValue.new(TimeZoneData.finite(micros), zoned: value.zoned)
      else
        months, days, micros = value.months, value.days, value.micros
        least = modifier[:fields]&.split&.last
        case least
        when 'year' then months = truncate_temporal(months, 12); days = micros = 0
        when 'month' then days = micros = 0
        when 'day' then micros = 0
        when 'hour' then micros = truncate_temporal(micros, 3_600_000_000)
        when 'minute' then micros = truncate_temporal(micros, 60_000_000)
        end
        micros = round_temporal(micros, precision) if precision
        IntervalValue.new(months, days, micros)
      end
    end

    def truncate_temporal(value, step)
      (value.negative? ? -1 : 1) * (value.abs / step) * step
    end

    def round_temporal(value, precision)
      step = 10**(6 - precision)
      (value.negative? ? -1 : 1) * ((value.abs + step / 2) / step) * step
    end
  end
  Engine.prepend(TemporalModifiers)
end
