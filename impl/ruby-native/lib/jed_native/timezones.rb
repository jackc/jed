# frozen_string_literal: true

require_relative 'timezone_data'

module JedNative
  module ZonedFunctions
    def return_type(name, types)
      if name == 'timezone'
        unless types.length == 2 && %w[text unknown null].include?(types[0]) && %w[timestamp timestamptz unknown null].include?(types[1])
          raise Error.new('42883', 'timezone requires a text zone and timestamp')
        end
        return types[1] == 'timestamptz' ? 'timestamp' : 'timestamptz'
      elsif name == 'date_trunc'
        unless types.length.between?(2, 3) && %w[text unknown null].include?(types[0]) && %w[timestamp timestamptz interval unknown null].include?(types[1]) && (types.length == 2 || types[1] == 'timestamptz' && %w[text unknown null].include?(types[2]))
          raise Error.new('42883', 'invalid date_trunc argument types')
        end
        return types[1] == 'unknown' ? 'timestamptz' : types[1]
      end
      super
    end

    def call(name, args, context: nil, arg_types: nil)
      return super unless context
      if name == 'timezone'
        return nil if args.any?(&:nil?)
        zone, value = args
        return TimestampValue.new(value.micros, zoned: !value.zoned) if value.micros.is_a?(Float)
        context.charge
        resolved = context.resolve_timezone(zone)
        micros = value.zoned ? resolved.instant_to_local(value.micros) : resolved.local_to_instant(value.micros)
        return TimestampValue.new(micros, zoned: !value.zoned)
      elsif name == 'make_timestamptz'
        return nil if args.any?(&:nil?)
        wall = super('make_timestamp', args[0, 6], context: context, arg_types: arg_types&.first(6))
        context.charge
        return TimestampValue.new(context.resolve_timezone(args[6]).local_to_instant(wall.micros), zoned: true)
      elsif name == 'current_date'
        value = context.clock_value(name)
        context.charge
        return DateValue.new(context.resolve_timezone.instant_to_local(value.micros).div(TimestampValue::DAY))
      elsif name == 'date_trunc' && args[1].is_a?(TimestampValue) && args[1].zoned
        return nil if args.any?(&:nil?)
        unit, value, zone_name = args
        return value if value.micros.is_a?(Float)
        zone = context.resolve_timezone(zone_name)
        context.charge
        wall = TimestampValue.new(zone.instant_to_local(value.micros))
        truncated = super(name, [unit, wall], context: context, arg_types: ['text', 'timestamp'])
        context.charge
        return TimestampValue.new(zone.local_to_instant(truncated.micros), zoned: true)
      elsif %w[extract date_part].include?(name) && args[1].is_a?(TimestampValue) && args[1].zoned
        return nil if args.any?(&:nil?)
        unit, value = args
        return super if unit.downcase == 'epoch' || value.micros.is_a?(Float)
        context.charge
        offset = context.timezone_at(value.micros)[0]
        if %w[timezone timezone_hour timezone_minute].include?(unit.downcase)
          result = case unit.downcase
          when 'timezone' then offset
          when 'timezone_hour' then DateFormatting.trunc(offset, 3600)
          else DateFormatting.trunc(DateFormatting.remainder(offset, 3600), 60)
          end
          return name == 'date_part' ? result.to_f : Decimal.parse(result)
        end
        return super(name, [unit, TimestampValue.new(TimeZoneData.finite(value.micros + offset * 1_000_000))], context: context, arg_types: ['text', 'timestamp'])
      end
      super
    end
  end
  Functions.singleton_class.prepend(ZonedFunctions)

  module TimeZones
    def timezone_type?(type)
      return timezone_type?(type[0...-2]) if type.end_with?('[]')
      return true if %w[timestamptz tstzrange].include?(type)
      (@types[type] || []).any? { |field| timezone_type?(field[:type]) }
    end

    def create_table(statement)
      result = super
      stored = @tables[statement[:name]]
      if stored
        columns = stored[:columns].map do |column|
          next column unless column[:default] && timezone_type?(column[:type])
          expression = compile(column[:default], [])
          next column unless expression.kind == :literal
          value = assign(expression.value, column, expression)
          column.merge(default: { kind: :literal, type: column[:type], value: value })
        end
        @tables[statement[:name]] = stored.merge(columns: columns)
      end
      result
    end

    def compile(node, columns)
      previous = @timezone_literal
      @timezone_literal = true if node[:kind] == :cast && node.dig(:expr, :kind) == :literal && node.dig(:expr, :value).is_a?(String)
      super
    ensure
      @timezone_literal = previous
    end
    def timezone = @timezone || 'UTC'
    def timezone=(name)
      resolve_timezone(name)
      @native_select_plans&.clear if timezone != name
      @timezone = name.dup.freeze
    end
    def resolve_timezone(name = nil)
      name ||= timezone
      @environment ||= Environment.new
      TimeZoneData.fixed(name) || @environment.timezones[name] || (raise Error.new('22023', "time zone #{name} is not recognized"))
    end
    def timezone_at(micros) = resolve_timezone.offset_at(micros.div(1_000_000))
    def local_to_instant(micros) = resolve_timezone.local_to_instant(micros)

    def render_timestamp(value, json: false)
      return value.to_s if value.micros.is_a?(Float)
      offset = timezone_at(value.micros)[0]
      wall = TimestampValue.new(TimeZoneData.finite(value.micros + offset * 1_000_000)).to_s
      era = wall.end_with?(' BC') ? ' BC' : ''
      wall = wall.delete_suffix(' BC')
      hour, rest = offset.abs.divmod(3600)
      minute, second = rest.divmod(60)
      suffix = format('%s%02d', offset.negative? ? '-' : '+', hour)
      suffix += format(':%02d', minute) if json || minute != 0 || second != 0
      suffix += format(':%02d', second) if second != 0
      wall = wall.sub(' ', 'T') if json
      wall + suffix + era
    end

    # Canonical values retain UTC. The host-facing renderer uses this session's
    # zone recursively, including timestamps inside records, arrays and ranges.
    def render_value(value, type = nil, meter: false)
      return 'NULL' if value.nil?
      if value.is_a?(TimestampValue) && value.zoned
        charge if meter && !value.micros.is_a?(Float)
        return render_timestamp(value)
      elsif value.is_a?(ArrayValue)
        prefix = value.lower_bounds.any? { |bound| bound != 1 } ? value.dimensions.each_with_index.map { |size, i| "[#{value.lower_bounds[i]}:#{value.lower_bounds[i] + size - 1}]" }.join + '=' : ''
        offset = 0
        render = lambda do |depth|
          items = Array.new(value.dimensions[depth] || 0) do
            if depth + 1 < value.dimensions.length
              render.call(depth + 1)
            else
              element = value[offset]
              offset += 1
              text = element == true ? 't' : element == false ? 'f' : render_value(element, value.element_type, meter: meter)
              element.nil? ? 'NULL' : Types.container_quote(text, /[{},"\\\s]|\ANULL\z/i)
            end
          end
          '{' + items.join(',') + '}'
        end
        return prefix + render.call(0)
      elsif value.is_a?(CompositeValue)
        fields = @types[value.type_name]
        return '(' + value.each_with_index.map { |element, index| element.nil? ? '' : Types.record_quote(element == true ? 't' : element == false ? 'f' : render_value(element, fields&.[](index)&.dig(:type), meter: meter)) }.join(',') + ')'
      elsif value.is_a?(RangeValue)
        return 'empty' if value.empty
        bounds = [value.lower, value.upper].map { |bound| bound.nil? ? '' : Types.container_quote(render_value(bound, value.element_type, meter: meter), /[,()\[\]"\\\s]/) }
        return (value.lower_inc ? '[' : '(') + bounds.join(',') + (value.upper_inc ? ']' : ')')
      end
      Types.format(value, type)
    end

    def cast_value(value, type, source_type: nil)
      return nil if value.nil?
      if type == 'text' && (value.is_a?(TimestampValue) && value.zoned || value.is_a?(ArrayValue) || value.is_a?(CompositeValue) || value.is_a?(RangeValue))
        return render_value(value, source_type, meter: true)
      elsif type.end_with?('[]')
        element_type = type[0...-2]
        return Types.parse_array(value, element_type) { |item| cast_value(item, element_type) } if value.is_a?(String)
        if value.is_a?(ArrayValue)
          return ArrayValue.new(value.map { |item| cast_value(item, element_type) }, element_type: element_type, dimensions: value.dimensions, lower_bounds: value.lower_bounds)
        end
      elsif type == 'tstzrange' && value.is_a?(String) && value != 'empty'
        parts = Types.split_container(value[1...-1])
        raise Error.new('22P02', 'malformed range literal') unless parts.length == 2 && ['[', '('].include?(value[0]) && [']', ')'].include?(value[-1])
        return RangeValue.new(*parts.map { |part| cast_value(part, 'timestamptz') }, value[0] + value[-1], element_type: 'timestamptz')
      elsif type == 'timestamptz' && value.is_a?(String)
        explicit = /(?:Z|UTC|[+-]\d{1,2}(?::?\d{2})?(?::?\d{2})?)(?:\s+(?:BC|AD))?\s*\z/i.match?(value) && /[T ]\d{1,2}:\d{2}/.match?(value)
        parsed = TimestampValue.parse(value, zoned: true)
        return parsed if explicit || parsed.micros.is_a?(Float)
        charge if source_type == 'text' && !@timezone_literal
        return TimestampValue.new(local_to_instant(parsed.micros), zoned: true)
      elsif type == 'timestamptz' && value.is_a?(DateValue)
        return TimestampValue.new(value.days, zoned: true) if value.days.is_a?(Float)
        charge
        return TimestampValue.new(local_to_instant(value.days * TimestampValue::DAY), zoned: true)
      elsif value.is_a?(TimestampValue)
        if type == 'timestamptz' && !value.zoned
          return TimestampValue.new(value.micros, zoned: true) if value.micros.is_a?(Float)
          charge
          return TimestampValue.new(local_to_instant(value.micros), zoned: true)
        elsif type == 'date' && !value.zoned && value.micros.is_a?(Float)
          return DateValue.new(value.micros)
        elsif value.zoned && %w[timestamp date].include?(type)
          micros = value.micros
          unless micros.is_a?(Float)
            charge
            micros = resolve_timezone.instant_to_local(micros)
          end
          return type == 'date' ? DateValue.new(micros.is_a?(Float) ? micros : micros.div(TimestampValue::DAY)) : TimestampValue.new(micros)
        end
      end
      super
    end

    def json_image_text(value, type, expression = nil, pretty: false)
      if value.is_a?(TimestampValue) && value.zoned
        charge unless value.micros.is_a?(Float)
        return JsonValue.escape(render_timestamp(value, json: true))
      elsif value.is_a?(RangeValue)
        return JsonValue.escape(render_value(value, type, meter: true))
      end
      super
    end
  end
  Engine.prepend(TimeZones)
end
