# frozen_string_literal: true

module JedNative
  # The implicit argument-coercion pass (spec/design/arg-coercion.md §4). It runs only for a
  # generic-path call that matches no built-in row (and no host signature) as written: every
  # built-in row and host function of the call's name and arity is scored by the summed cost of
  # its conversions from compare.toml's [[arg_coercion]] table, and the minimum wins. A built-in
  # wins a tie with a host function; tied built-ins fall to the functions.md §9.1 unknown-position
  # rule; anything still tied is 42725. Coercion charges nothing.
  module ArgCoercion
    ADOPTED = { 'integer' => 'i64', 'float' => 'f64' }.freeze
    # Resolver-routed functions keep their own hand-written typing (arg-coercion.md §8).
    ROUTED = %w[coalesce greatest least nullif div gcd lcm mod width_bucket make_timestamp make_timestamptz
                make_date make_interval].freeze
    Candidate = Struct.new(:entry, :host, :score, :targets)
    module_function

    def eligible?(node)
      return false unless node[:kind] == :call && !node[:variadic] && !node[:arg_names] && !node[:over]
      name = node[:name]
      return false if name.nil? || name != name.downcase || ROUTED.include?(name)
      args = node[:args]
      return false if args.nil? || args.empty? || args.any? { |arg| arg[:kind] == :star }
      return false if node[:distinct] || node[:within_group] || !(node[:order_by] || []).empty?
      return false if JsonImages::JSON_IMAGES.include?(name) || name.start_with?('json') || name.start_with?('regexp_')
      return false if Types::RANGES.key?(Types.normalize(name)) || Functions.aggregate?(name)
      true
    end

    def builtin_exact?(name, types)
      !!Functions.resolve_signature(name, types)
    rescue Error => error
      raise unless error.sqlstate == '42883'
      false
    end

    # One argument's source: [:unknown], [:integer_literal, value], [:decimal_literal], or
    # [:typed, base_type]. A bare numeric constant keeps its default type (i64, decimal) as well.
    def source(kind, untyped, type, value)
      return [:unknown] if %w[null unknown].include?(type)
      if kind == :literal && untyped
        return [:integer_literal, value] if type == 'i64' && value.is_a?(Integer)
        return [:decimal_literal] if type == 'decimal'
      end
      [:typed, Types.base(type)]
    end

    def own_type(source)
      case source[0]
      when :integer_literal then 'i64'
      when :decimal_literal then 'decimal'
      when :typed then source[1]
      end
    end

    # The cheapest table row from the source into a type the slot accepts: [cost, type], the first
    # row in table order on a tie, or nil. A literal also takes its default type's rows; an integer
    # constant narrows only when its value fits.
    def cheapest(source)
      own = own_type(source)
      return nil unless own
      literal = %i[integer_literal decimal_literal].include?(source[0]) ? source[0].to_s : nil
      best = nil
      FunctionCatalog::ARG_COERCIONS.each do |(from, to), cost|
        next unless from == own || from == literal
        next unless yield(to)
        if source[0] == :integer_literal && %w[i16 i32].include?(to)
          low, high = Types::INTEGER_BOUNDS.fetch(to)
          next unless source[1] >= low && source[1] < high
        end
        best = [cost, to] if best.nil? || cost < best[0]
      end
      best
    end

    def slot_matches?(slot, type)
      return true if %w[any anycompatible].include?(slot)
      return type.end_with?('[]') if %w[anyarray anycompatiblearray].include?(slot)
      return Types::RANGES.key?(type) if slot == 'anyrange'
      Functions.family(type) == slot
    end

    def score_builtin(entry, sources, types)
      targets = Array.new(sources.length)
      score = 0
      entry[0].each_with_index do |slot, index|
        source = sources[index]
        if source[0] == :unknown
          targets[index] = [ADOPTED.fetch(slot, slot), :unknown] unless slot.start_with?('any')
          next
        end
        next if slot_matches?(slot, types[index])
        found = cheapest(source) { |to| Functions.family(to) == slot }
        return nil unless found
        score += found[0]
        targets[index] = [found[1], source[0] == :typed ? :value : :literal]
      end
      Candidate.new(entry, nil, score, targets)
    end

    def score_host(function, sources)
      targets = Array.new(sources.length)
      score = 0
      function.args.each_with_index do |slot, index|
        source = sources[index]
        return nil if source[0] == :unknown
        next if own_type(source) == slot
        found = cheapest(source) { |to| to == slot }
        return nil unless found
        score += found[0]
        targets[index] = [found[1], source[0] == :typed ? :value : :literal]
      end
      Candidate.new(nil, function, score, targets)
    end

    # The selected candidate, or nil when none admits the call.
    def select(name, sources, types, hosts = [])
      candidates = []
      (FunctionCatalog::SIGNATURES[name] || []).each do |entry|
        next if entry[3] || entry[0].length != sources.length
        candidate = score_builtin(entry, sources, types)
        candidates << candidate if candidate
      end
      hosts.each do |function|
        next unless function.args.length == sources.length
        candidate = score_host(function, sources)
        candidates << candidate if candidate
      end
      return nil if candidates.empty?
      minimum = candidates.map(&:score).min
      kept = candidates.select { |candidate| candidate.score == minimum }
      builtins = kept.select(&:entry)
      kept = builtins unless builtins.empty?
      return kept.first if kept.length == 1
      raise Error.new('42725', "function #{name} is ambiguous") unless kept.first.entry
      pick_by_unknown_positions(name, kept, sources)
    end

    def pick_by_unknown_positions(name, kept, sources)
      sources.each_with_index do |source, index|
        next unless source[0] == :unknown
        choices = kept.map { |candidate| candidate.entry[0][index] }.uniq
        choice = choices.include?('text') ? 'text' : (choices.length == 1 ? choices.first : nil)
        raise Error.new('42725', "function #{name} is ambiguous") unless choice
        kept = kept.select { |candidate| candidate.entry[0][index] == choice }
      end
      raise Error.new('42725', "function #{name} is ambiguous") unless kept.length == 1
      kept.first
    end

    def expression_sources(arguments)
      arguments.map do |argument|
        source(argument.kind, argument.extra.is_a?(Hash) && argument.extra[:untyped], argument.type, argument.value)
      end
    end
  end

  module CoerceFunctionArguments
    # Adopts a selected candidate's coercions in compiled arguments. A numeric constant is resolved
    # again in its slot's type (literal adaptation); an integer value bound for a decimal slot
    # converts inside the call through the uncharged numeric context; an unknown argument adopts
    # its slot's type. Widening within a tower needs no Ruby conversion.
    def adopt_coercions(arguments, candidate)
      candidate.targets.each_with_index.map do |target, index|
        argument = arguments[index]
        next argument unless target
        type, mode = target
        if mode == :value
          next argument unless type == 'decimal' && Types::INTEGER_BITS.key?(Types.base(argument.type))
          next Expression.new(kind: :numeric_context, type: 'decimal', args: [argument])
        end
        argument.value = cast_value(argument.value, type, source_type: argument.type) if argument.kind == :literal
        argument.type = type
        argument
      end
    end

    def compile(node, columns)
      if ArgCoercion.eligible?(node) && FunctionCatalog::SIGNATURES.key?(node[:name])
        arguments = node[:args].map { |argument| compile(argument, columns) }
        if FunctionResolution.needed?(node)
          begin
            targets = FunctionResolution.targets(node, arguments.map(&:type))
            arguments.each_with_index do |argument, index|
              next unless targets[index]
              argument.value = cast_value(argument.value, targets[index], source_type: argument.type)
              argument.type = targets[index]
            end
          rescue Error => error
            raise unless error.sqlstate == '42883'
          end
        end
        types = arguments.map(&:type)
        unless ArgCoercion.builtin_exact?(node[:name], types)
          candidate = ArgCoercion.select(node[:name], ArgCoercion.expression_sources(arguments), types)
          arguments = adopt_coercions(arguments, candidate) if candidate
        end
        node = node.merge(args: arguments.map { |argument| { kind: :resolved_expression, expression: argument } })
      end
      super(node, columns)
    end
  end
  Engine.prepend(CoerceFunctionArguments)

  module CoerceBindingArguments
    # Statement typing: a call that matches no row as written takes the selected candidate's
    # coerced types for its result type. Unknown parameters keep the §9.1 typing below.
    def function(node, scope, wanted)
      name = node[:name].to_s.downcase
      if ArgCoercion.eligible?(node) && FunctionCatalog::SIGNATURES.key?(name) && !node[:args].any? { |argument| argument[:kind] == :param }
        types = node[:args].map { |argument| expression(argument, scope) }
        sources = node[:args].each_with_index.map do |argument, index|
          ArgCoercion.source(argument[:kind], argument[:untyped], types[index], argument[:value])
        end
        unless ArgCoercion.builtin_exact?(name, types)
          candidate = ArgCoercion.select(name, sources, types)
          if candidate
            coerced = types.each_with_index.map { |type, index| candidate.targets[index] ? candidate.targets[index][0] : type }
            return Functions.return_type(name, coerced)
          end
        end
      end
      super
    end
  end
  ParameterBindings.prepend(CoerceBindingArguments)
end
