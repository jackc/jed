# frozen_string_literal: true

module JedNative
  class JsonPathEvaluator
    def initialize(path, root, vars: {}, context: nil, variable_bases: 1, passing: false, source_text: nil)
      @path, @root, @vars, @context = JsonPathValue.parse(path), root, vars, context
      @strict = @path.strict
      raise Error.new('22023', 'vars argument is not an object') unless @vars.is_a?(Hash)
      @locations = {}.compare_by_identity
      @locations_by_base = {}
      @container_bases = {}.compare_by_identity
      @active_base = 0
      @next_base = variable_bases + 1
      @passing = passing
      @source_text = source_text
    end

    def run(exists: false, silent: false)
      @existence = exists
      charge((@source_text || @path.to_s).bytesize)
      charge_regex(@path.root)
      items = []
      catch(:jsonpath_first_item) do
        evaluate(@path.root, @root, nil, false) do |item|
          items << item
          throw :jsonpath_first_item if exists && !@strict
        end
      end
      { items: items, error: nil }
    rescue Error => error
      raise unless silent && error.sqlstate.start_with?('22')
      { items: items || [], error: error }
    end

    private
    def charge(amount = 1) = @context&.charge(amount)
    def json_error(code, message) = raise(Error.new(code, message))
    def keys(object) = object.keys.sort_by { |key| [key.bytesize, key.b] }
    def unwrap(items) = @strict ? items : items.flat_map { |item| item.is_a?(Array) ? item : [item] }
    def numeric?(value) = value.is_a?(Integer) || value.is_a?(Decimal)
    def charge_regex(node)
      case node
      when Hash
        charge(node[:program].code.length) if node[:kind] == :regex
        node.each_value { |value| charge_regex(value) unless value.is_a?(RegexProgram) }
      when Array then node.each { |value| charge_regex(value) }
      end
    end

    def evaluate(node, current, last, suppressed, &output)
      case node[:kind]
      when :root then with_base(0) { output.call(@root) }
      when :current then output.call(current)
      when :last then output.call(Decimal.new(last || 0))
      when :variable
        json_error('42704', "could not find jsonpath variable #{node[:name]}") unless @vars.key?(node[:name])
        base = @passing ? @vars.keys.index(node[:name]) + 1 : 1
        with_base(base) { output.call(@vars[node[:name]]) }
      when :literal then output.call(node[:value])
      when :path
        previous = @unary_continuation
        @unary_continuation = true if node[:base][:kind] == :unary
        begin
          evaluate(node[:base], current, last, suppressed) { |item| walk(node[:steps], item, current, last, suppressed, &output) }
        ensure
          @unary_continuation = previous
        end
      when :arithmetic
        left = unwrap(collect(node[:left], current, last, suppressed))
        right = unwrap(collect(node[:right], current, last, suppressed))
        json_error('22038', 'left arithmetic operand is not a single number') unless left.length == 1 && numeric?(left[0])
        json_error('22038', 'right arithmetic operand is not a single number') unless right.length == 1 && numeric?(right[0])
        a, b = Decimal.parse(left[0]), Decimal.parse(right[0])
        charge
        charge(decimal_extra(node[:op], a, b)) unless b.zero? && %w[/ %].include?(node[:op])
        result = Types.binary(node[:op], a, b, type: 'decimal')
        charge
        output.call(result)
      when :unary
        values = unwrap(collect(node[:expr], current, last, suppressed))
        values.each do |value|
          charge
          unless numeric?(value)
            next if @existence && !@strict && !@unary_continuation
            json_error('2203B', 'unary arithmetic operand is not a number')
          end
          charge
          output.call(node[:op] == '-' ? -value : value)
        end
      else output.call(predicate(node, current, last, suppressed))
      end
    end

    def with_base(base)
      previous = @active_base
      @active_base = base
      yield
    ensure
      @active_base = previous
    end

    def collect(node, current, last, suppressed)
      result = []
      evaluate(node, current, last, suppressed) { |item| result << item }
      result
    end

    def walk(steps, item, current, last, suppressed, &output)
      # Keep continuation traversal explicit. A flat path with thousands of
      # accessors uses no native call stack and emits in depth-first order.
      frames = [[0, [[item, last, suppressed, @active_base]].each, @active_base]]
      until frames.empty?
        index, iterator, origin = frames[-1]
        begin
          @active_base = origin
          value, array_last, suppress, base = iterator.next
        rescue StopIteration
          frames.pop
          next
        end
        if index == steps.length
          with_base(base) { output.call(value) }
        else
          frames << [index + 1, apply(steps[index], value, current, array_last, suppress, base), base]
        end
      end
    end

    def apply(step, value, current, last, suppressed, origin)
      Enumerator.new do |out|
        unwrapping = %i[member members filter].include?(step[:kind]) || step[:kind] == :method && !%w[type size].include?(step[:name])
        targets = !@strict && unwrapping && value.is_a?(Array) ? value : [value]
        targets.each do |target|
          charge
          emit = lambda do |item, next_last = last, suppress = suppressed|
            charge
            out << [item, next_last, suppress, @container_bases[item] || origin]
          end
          case step[:kind]
          when :member
            if target.is_a?(Hash)
              found = false
              keys(target).each do |key|
                charge
                next unless key == step[:name]
                emit.call(target[key])
                found = true
                break
              end
              navigation('2203A', suppressed) unless found
            else
              navigation('2203A', suppressed)
            end
          when :members
            if target.is_a?(Hash)
              keys(target).each { |key| emit.call(target[key]) }
            else
              navigation('2203C', suppressed)
            end
          when :elements, :subscript
            unless target.is_a?(Array)
              if @strict
                navigation('22039', suppressed)
                next
              end
              target = [target]
            end
            if step[:kind] == :elements
              target.each { |element| emit.call(element, target.length - 1) }
            else
              step[:bounds].each do |low, high|
                first = subscript(low, current, target.length - 1, suppressed)
                final = high ? subscript(high, current, target.length - 1, suppressed) : first
                if first < 0 || final >= target.length || first > final
                  navigation('22033', suppressed)
                end
                first = [first, 0].max
                final = [final, target.length - 1].min
                first.upto(final) { |index| emit.call(target[index], target.length - 1) }
              end
            end
          when :filter
            emit.call(target) if predicate(step[:expr], target, last, suppressed) == true
          when :method
            if step[:name] == 'size' && !target.is_a?(Array) && @strict
              navigation('22039', suppressed)
              next
            end
            method_items(step, target, origin).each { |result| emit.call(result) }
          when :descendants
            low, high = step.values_at(:low, :high)
            emit.call(target, last, true) if low == 0
            children = container_children(target)
            pending = children.reverse.map { |child| [child, 1] }
            until pending.empty?
              child, level = pending.pop
              next if high != :last && level > high
              charge
              nested = container_children(child)
              selected = if low == :last then high == :last && nested.empty? && !child.is_a?(Array) && !child.is_a?(Hash)
              else level >= low
              end
              emit.call(child, last, true) if selected
              if high == :last || level < high
                nested.reverse_each { |descendant| pending << [descendant, level + 1] }
              end
            end
          end
        end
      end
    end

    def navigation(code, suppressed)
      json_error(code, 'JSON path navigation failed') if @strict && !suppressed
    end
    def container_children(value)
      value.is_a?(Array) ? value : value.is_a?(Hash) ? keys(value).map { |key| value[key] } : []
    end
    def subscript(node, current, last, suppressed)
      values = collect(node, current, last, suppressed)
      json_error('22033', 'array subscript is not a single number') unless values.length == 1 && numeric?(values[0])
      index = values[0].to_i
      json_error('22033', 'array subscript is outside the integer range') unless index.between?(-2_147_483_648, 2_147_483_647)
      index
    end

    def predicate(node, current, last, suppressed)
      case node[:kind]
      when :connective
        left = predicate(node[:left], current, last, suppressed)
        right = predicate(node[:right], current, last, suppressed)
        if node[:op] == '&&'
          left == false || right == false ? false : left.nil? || right.nil? ? nil : true
        else
          left == true || right == true ? true : left.nil? || right.nil? ? nil : false
        end
      when :not
        value = predicate(node[:expr], current, last, suppressed)
        value.nil? ? nil : !value
      when :unknown then predicate(node[:expr], current, last, suppressed).nil?
      when :exists
        found = false
        catch(:jsonpath_exists) do
          evaluate(node[:expr], current, last, suppressed) do |_item|
            found = true
            throw :jsonpath_exists unless @strict
          end
        end
        found
      when :comparison, :starts, :regex
        left = unwrap(collect(node[:left] || node[:expr], current, last, suppressed))
        right = node[:kind] == :regex ? [nil] : collect(node[:right], current, last, suppressed)
        right = unwrap(right) if node[:kind] == :comparison
        yes, unknown = false, false
        left.each do |a|
          right.each do |b|
            charge
            result = if node[:kind] == :regex
              a.is_a?(String) ? node[:program].match?(a, context: @context) : nil
            elsif node[:kind] == :starts
              if a.is_a?(String) && b.is_a?(String)
                charge([[a.length, b.length].min - 1, 0].max)
                a.start_with?(b)
              end
            else
              compare(node[:op], a, b)
            end
            return true if result == true && !@strict
            return nil if result.nil? && @strict
            yes ||= result == true
            unknown ||= result.nil?
          end
        end
        yes ? true : unknown ? nil : false
      else
        json_error('42601', 'expected JSON path predicate')
      end
    rescue Error => error
      raise unless error.sqlstate.start_with?('22')
      nil
    end

    def compare(op, a, b)
      if a.nil? || b.nil?
        return op == '!=' if a.nil? != b.nil?
        comparison = 0
      elsif numeric?(a) && numeric?(b)
        charge(decimal_extra('=', Decimal.parse(a), Decimal.parse(b)))
        comparison = a <=> b
      elsif a.is_a?(String) && b.is_a?(String)
        charge([[a.length, b.length].min - 1, 0].max)
        comparison = a <=> b
      elsif [true, false].include?(a) && [true, false].include?(b)
        comparison = (a ? 1 : 0) <=> (b ? 1 : 0)
      else
        return nil
      end
      case op
      when '==' then comparison.zero?
      when '!=' then !comparison.zero?
      when '<' then comparison < 0
      when '<=' then comparison <= 0
      when '>' then comparison > 0
      when '>=' then comparison >= 0
      end
    end

    def decimal_extra(op, a, b)
      left = [1, (a.coefficient.abs.to_s.length + 3) / 4].max
      right = [1, (b.coefficient.abs.to_s.length + 3) / 4].max
      work = case op
      when '*' then left * right
      when '/', '%' then left * right
      else [left, right].max
      end
      [work - 1, 0].max
    end

    def method_items(step, value, origin)
      name, args = step.values_at(:name, :args)
      if %w[datetime date time time_tz timestamp timestamp_tz].include?(name)
        json_error('0A000', 'JSON path datetime methods are not supported')
      end
      if name == 'keyvalue'
        json_error('2203C', 'keyvalue requires an object') unless value.is_a?(Hash)
        initialize_layout unless @layout_initialized
        identifier = @locations_by_base[origin]&.[](value) || 0
        return Enumerator.new do |output|
          keys(value).each do |key|
            object = { 'id' => Decimal.new(identifier), 'key' => key, 'value' => value[key] }
            base = @next_base
            @next_base += 1
            layout(object, 0, base)
            @container_bases[object] = base
            output << object
          end
        end
      end
      result = case name
      when 'type' then JsonValue.new(value).type_name
      when 'size' then Decimal.new(value.is_a?(Array) ? value.length : 1)
      when 'abs', 'floor', 'ceiling'
        json_error('22036', 'JSON path numeric method requires a number') unless numeric?(value)
        number = Decimal.parse(value)
        name == 'abs' ? number.abs : name == 'floor' ? number.floor : number.ceil
      when 'bigint', 'integer'
        json_error('22036', 'invalid integer item') unless numeric?(value) || value.is_a?(String)
        Decimal.new(Types.cast(value, name == 'integer' ? 'i32' : 'i64'))
      when 'boolean'
        if value == true || value == false
          value
        elsif numeric?(value)
          text = value.to_s
          json_error('22036', 'invalid boolean item') unless text.match?(/\A[+-]?[0-9]+\z/)
          !Types.cast(text, 'i32').zero?
        elsif value.is_a?(String) && value == value.strip
          Types.cast(value, 'boolean')
        else
          json_error('22036', 'invalid boolean item')
        end
      when 'number', 'decimal'
        json_error('22036', 'invalid decimal item') unless numeric?(value) || value.is_a?(String)
        number = Decimal.parse(value)
        if name == 'decimal' && !args.empty?
          precision, scale = args[0], args[1] || 0
          json_error('22036', 'decimal modifier is outside integer range') unless args.all? { |arg| arg.between?(-2_147_483_648, 2_147_483_647) }
          json_error('22023', 'invalid decimal precision or scale') unless precision.between?(1, 1000) && scale.between?(-1000, 1000)
          number = number.round(scale)
          bound = precision - scale
          overflow = bound >= 0 ? number.abs >= Decimal.new(10**bound) : number.abs >= Decimal.new(1, -bound)
          json_error('22036', 'decimal item is out of range') if overflow
        end
        number
      when 'double'
        json_error('22036', 'invalid double item') unless numeric?(value) || value.is_a?(String)
        number = Types.cast(value, 'f64')
        json_error('22036', 'invalid finite double item') unless number.finite?
        original = Decimal.parse(value)
        json_error('22036', 'double item underflow') if number.zero? && !original.zero?
        numeric?(value) ? value : Decimal.parse(format('%.15g', number))
      when 'string'
        json_error('22036', 'invalid string item') unless numeric?(value) || value.is_a?(String) || value == true || value == false
        value == true ? 'true' : value == false ? 'false' : value.to_s
      end
      [result]
    rescue Error => error
      raise if %w[22023 0A000 2203C].include?(error.sqlstate)
      raise Error.new('22036', 'JSON path item method conversion failed')
    end

    def initialize_layout
      @layout_initialized = true
      layout(@root, 0, 0)
      if @passing
        @vars.each_with_index { |(_name, value), index| layout(value, 0, index + 1) }
      else
        layout(@vars, 0, 1)
      end
    end
    def align4(value) = (value + 3) & ~3
    def layout(value, position, base)
      if value.is_a?(Array) || value.is_a?(Hash)
        position = align4(position)
        (@locations_by_base[base] ||= {}.compare_by_identity)[value] = base * 10_000_000_000 + position
        children = value.is_a?(Array) ? value : keys(value).map { |key| value[key] }
        cursor = position + 4 + (value.is_a?(Array) ? 4 : 8) * children.length
        cursor += value.keys.sum(&:bytesize) if value.is_a?(Hash)
        children.each { |child| cursor = layout(child, cursor, base) }
        cursor
      elsif value.is_a?(String)
        position + value.bytesize
      elsif numeric?(value)
        number = Decimal.parse(value)
        coefficient = number.coefficient.abs.to_s
        top = coefficient.length - number.scale - 1
        bottom = coefficient.length - coefficient.sub(/0+\z/, '').length - number.scale
        groups = number.zero? ? 0 : top.div(4) - bottom.div(4) + 1
        header = number.scale <= 63 && top.div(4).between?(-64, 63) ? 6 : 8
        align4(position) + header + groups * 2
      else
        position
      end
    end
  end
end
