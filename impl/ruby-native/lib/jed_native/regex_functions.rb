# frozen_string_literal: true

require_relative 'regex'
require_relative 'similar'

module JedNative
  module RegexFunctions
    NAMES = %w[regexp_replace regexp_match regexp_matches regexp_split_to_array regexp_split_to_table regexp_like regexp_count regexp_substr regexp_instr].freeze
    FLAGS_INDEX = { 'regexp_replace' => 3, 'regexp_count' => 3, 'regexp_substr' => 4, 'regexp_instr' => 5 }.freeze
    module_function

    def flags(name, args) = args[FLAGS_INDEX.fetch(name, 2)] || ''

    def validate_flags(name, value)
      raise Error.new('2201B', 'invalid regular expression option') unless value.each_char.all? { |char| %w[i g].include?(char) }
      if value.include?('g') && !%w[regexp_replace regexp_matches].include?(name)
        raise Error.new('22023', "#{name} does not support the global option")
      end
    end

    def compile(name, pattern, flags)
      validate_flags(name, flags)
      RegexProgram.new(pattern, insensitive: flags.include?('i'))
    end

    def call(name, args, context: nil, program: nil)
      return nil if args.any?(&:nil?)
      source, pattern = args
      start, nth, group, ending = 1, 1, 0, 0
      if %w[regexp_count regexp_substr regexp_instr].include?(name)
        start = args[2] || 1
        raise Error.new('22023', 'invalid start position') if start < 1
      end
      if %w[regexp_substr regexp_instr].include?(name)
        nth = args[3] || 1
        group = args[name == 'regexp_substr' ? 5 : 6] || 0
        ending = args[4] || 0 if name == 'regexp_instr'
        raise Error.new('22023', 'invalid match occurrence') if nth < 1
        raise Error.new('22023', 'invalid subexpression index') if group < 0
        raise Error.new('22023', 'invalid end option') unless [0, 1].include?(ending)
      end
      options = name == 'substring_regex' ? '' : flags(name, args)
      validate_flags(name, options)
      unless program
        program = compile(name, pattern, options)
        context&.charge(program.code.length)
      end
      original = RegexText.new(source)
      input = program.input(original)
      if %w[regexp_like regex_match regex_imatch].include?(name)
        return !program.search(input, 0, context: context).nil?
      end
      if %w[regexp_match substring_regex].include?(name)
        match = program.search(input, 0, context: context)
        return nil unless match
        if name == 'substring_regex'
          span = program.span(match, program.groups.positive? ? 1 : 0)
          return span && original.slice(*span)
        end
        return capture_array(program, match, original, context)
      end
      if %w[regexp_count regexp_substr regexp_instr].include?(name)
        count, position = 0, start - 1
        while position <= input.length
          match = program.search(input, position, context: context)
          break unless match
          count += 1
          if count == nth && name != 'regexp_count'
            span = program.span(match, group)
            return span ? span[ending] + 1 : 0 if name == 'regexp_instr'
            return span && original.slice(*span)
          end
          first, last = program.span(match)
          position = last == first ? last + 1 : last
        end
        return count if name == 'regexp_count'
        return name == 'regexp_instr' ? 0 : nil
      end
      if name == 'regexp_replace'
        template = replacement_tokens(args[2])
        bytes = 0
        replacement_pieces(program, input, template, options.include?('g'), args[2].bytesize, context) do |piece|
          bytes += piece.is_a?(String) ? piece.bytesize : original.byte_offset(piece[1]) - original.byte_offset(piece[0])
        end
        context&.charge(bytes)
        context&.consume_scalar_bytes(bytes)
        # Replay the deterministic matcher only after exact output admission.
        # The first pass owns its canonical VM/template cost; neither pass
        # retains match captures beyond the current match.
        output = +''
        replacement_pieces(program, input, template, options.include?('g'), 0, nil) do |piece|
          output << (piece.is_a?(String) ? piece : original.slice(*piece))
        end
        return output
      end
      if name == 'regexp_matches'
        output, position = [], 0
        while position <= input.length
          match = program.search(input, position, context: context)
          break unless match
          output << capture_array(program, match, original, context)
          break unless options.include?('g')
          first, last = program.span(match)
          position = last == first ? last + 1 : last
        end
        return output
      end
      output, position, previous_end, split_at = [], 0, 0, 0
      while position <= input.length
        match = program.search(input, position, context: context)
        break unless match
        first, last = program.span(match)
        if first < input.length && last > previous_end
          output << original.slice(split_at, first)
          split_at = last
        end
        previous_end = last
        position = last == first ? last + 1 : last
      end
      output << original.slice(split_at, original.length)
      name == 'regexp_split_to_array' ? ArrayValue.new(output, element_type: 'text') : output
    end

    def capture_array(program, match, original, context)
      groups = program.groups.zero? ? [0] : (1..program.groups)
      spans = groups.map { |group| program.span(match, group) }
      bytes = 16 + spans.sum { |span| 32 + (span ? original.byte_offset(span[1]) - original.byte_offset(span[0]) : 0) }
      context&.charge(bytes)
      context&.consume_scalar_bytes(bytes)
      ArrayValue.new(spans.map { |span| span && original.slice(*span) }, element_type: 'text')
    end

    def replacement_tokens(template)
      result, literal, escaped = [], +'', false
      template.each_char do |char|
        if escaped
          if char.between?('0', '9') || char == '&'
            result << literal unless literal.empty?
            literal = +''
            result << (char == '&' ? 0 : char.to_i)
          else
            literal << char
          end
          escaped = false
        elsif char == '\\'
          escaped = true
        else
          literal << char
        end
      end
      literal << '\\' if escaped
      result << literal unless literal.empty?
      result
    end

    def replacement_pieces(program, input, template, global, template_bytes, context)
      position, search_at, length = 0, 0, input.length
      while search_at <= length
        match = program.search(input, search_at, context: context)
        break unless match
        context&.charge(template_bytes)
        first, last = program.span(match)
        yield [position, first]
        template.each do |token|
          piece = token.is_a?(Integer) ? program.span(match, token) : token
          yield piece if piece
        end
        if first == last
          yield [last, last + 1] if last < length
          position = last + 1
        else
          position = last
        end
        break unless global
        search_at = last == first ? last + 1 : last
      end
      yield [position, length] if position <= length
    end
  end

  module RegexExpressions
    def dispatch(statement)
      return super if @regex_dispatch_active
      owns_regex = true
      @regex_dispatch_active = true
      @regex_charged = {}.compare_by_identity
      @regex_programs = {}.compare_by_identity
      super
    ensure
      @regex_dispatch_active = false if owns_regex
    end

    def compile(node, columns)
      similar = node[:kind] == :binary && ['similar to', 'not similar to'].include?(node[:op])
      if node[:kind] == :binary && (similar || %w[~ ~* !~ !~*].include?(node[:op]))
        right = node[:right]
        # SIMILAR TO desugars, as in PostgreSQL, to `~ similar_to_escape(p [, e])`
        # compiled dot-all (regex.md §8f.2).
        right = { kind: :call, name: 'similar_to_escape', args: [right, node[:escape]].compact } if similar
        op = similar ? (node[:op].start_with?('not') ? '!~' : '~') : node[:op]
        args = [compile(node[:left], columns), compile(right, columns)]
        raise Error.new('42804', 'regular expression operands must be text') unless args.all? { |arg| %w[text unknown null].include?(arg.type) }
        if similar && args[1].kind == :call && args[1].args.all? { |arg| arg.kind == :literal && arg.value.is_a?(String) }
          # A constant pattern and escape translate now, so a translation
          # error surfaces at resolve and the pattern precompiles.
          translated = Similar.translate(args[1].args[0].value, args[1].args[1]&.value)
          args[1] = Expression.new(kind: :literal, type: 'text', value: translated)
        end
        options = op.end_with?('*') ? 'i' : ''
        if args[1].kind == :literal && args[1].value
          program = (@regex_programs ||= {}.compare_by_identity)[node] ||=
            similar ? Similar.compile(args[1].value) : RegexProgram.new(args[1].value, insensitive: options == 'i')
        end
        return Expression.new(kind: :regex_operator, type: 'boolean', value: op, args: args, extra: { program: program, flags: options, charge_key: node, similar: similar })
      end
      return super unless node[:kind] == :call && (RegexFunctions::NAMES.include?(node[:name]) || node[:name] == 'substring')
      if %w[regexp_matches regexp_split_to_table].include?(node[:name]) && !@regex_source_compile
        raise Error.new('42883', 'set-returning regex functions require FROM')
      end
      expression = super
      name = node[:name]
      if name == 'substring'
        textual = expression.args[1..].all? { |arg| %w[text unknown null].include?(arg.type) }
        return expression unless expression.args.length.between?(2, 3) && textual
        if expression.args.length == 3
          # The SIMILAR substring (regex.md §8f.3): a NULL constant subject is
          # NULL without translating; a constant pattern and escape precompile.
          subject, pattern, escape = expression.args
          unless subject.kind == :literal && subject.value.nil?
            if [pattern, escape].all? { |arg| arg.kind == :literal && arg.value.is_a?(String) }
              matcher = (@regex_programs ||= {}.compare_by_identity)[node] ||= Similar::Matcher.new(pattern.value, escape.value)
            end
          end
          expression.type = 'text'
          expression.kind = :regex_function
          expression.value = 'substring_similar'
          expression.extra = { matcher: matcher, charge_key: node }
          return expression
        end
        name = 'substring_regex'
      end
      if %w[regexp_matches regexp_split_to_table].include?(name)
        raise Error.new('42883', 'invalid regular expression function arguments') unless expression.args.length.between?(2, 3) && expression.args.all? { |arg| %w[text null unknown].include?(arg.type) }
        expression.type = name == 'regexp_matches' ? 'text[]' : 'text'
      end
      pattern = expression.args[1]
      options = name == 'substring_regex' ? nil : expression.args[RegexFunctions::FLAGS_INDEX.fetch(name, 2)]
      if pattern&.kind == :literal && pattern.value && (!options || options.kind == :literal && options.value)
        program = (@regex_programs ||= {}.compare_by_identity)[node] ||= RegexFunctions.compile(name, pattern.value, options&.value || '')
      end
      expression.kind = :regex_function
      expression.value = name
      expression.extra = { program: program, charge_key: node }
      expression
    end

    def evaluate(expression, row, group = nil)
      return super unless %i[regex_operator regex_function].include?(expression.kind)
      charge
      return similar_substring(expression, row, group) if expression.value == 'substring_similar'
      args = expression.args.map { |arg| evaluate(arg, row, group) }
      return nil if args.any?(&:nil?)
      program = expression.extra[:program]
      charge_key = expression.extra[:charge_key] || expression
      if program && !(@regex_charged ||= {}.compare_by_identity).key?(charge_key)
        charge(program.code.length)
        @regex_charged[charge_key] = true
      end
      if expression.kind == :regex_operator
        unless program
          program = expression.extra[:similar] ? Similar.compile(args[1]) : RegexProgram.new(args[1], insensitive: expression.extra[:flags] == 'i')
          charge(program.code.length)
        end
        matched = program.match?(args[0], context: self)
        expression.value.start_with?('!') ? !matched : matched
      else
        RegexFunctions.call(expression.value, args, context: self, program: program)
      end
    end

    # STRICT, but a NULL subject value does not hide a bad escape: a NULL
    # constant argument is NULL at once, otherwise every argument is
    # evaluated and the pattern translated before the subject's NULL check.
    def similar_substring(expression, row, group)
      args = []
      expression.args.each do |arg|
        value = evaluate(arg, row, group)
        return nil if value.nil? && arg.kind == :literal
        args << value
      end
      source, pattern, escape = args
      return nil if pattern.nil? || escape.nil?
      matcher = expression.extra[:matcher]
      charge_key = expression.extra[:charge_key] || expression
      if matcher
        unless (@regex_charged ||= {}.compare_by_identity).key?(charge_key)
          charge(matcher.instructions)
          @regex_charged[charge_key] = true
        end
      else
        translated = Similar.translate(pattern, escape)
        charge(translated.bytesize)
        consume_scalar_bytes(translated.bytesize)
        matcher = Similar::Matcher.new(pattern, escape)
        charge(matcher.instructions)
      end
      return nil if source.nil?
      original = RegexText.new(source)
      span = matcher.span(original, self)
      span && original.slice(*span)
    end

    def source_relation(source)
      call = source[:expr] || source[:call] || source
      return super unless %i[function call srf].include?(source[:kind]) && %w[regexp_matches regexp_split_to_table].include?(call[:name])
      @regex_source_compile = true
      expression = compile(call, [])
      @regex_source_compile = false
      values = evaluate(expression, []) || []
      charge(values.length)
      name = source[:columns]&.first || source[:column_aliases]&.first || source[:alias] || call[:name]
      table = source[:alias] || call[:name]
      columns = [{ name: name, table: table, type: expression.type }]
      rows = values.map { |value| [value] }
      if source[:ordinality]
        columns << { name: source[:columns]&.[](1) || 'ordinality', table: table, type: 'i64' }
        rows.each_with_index { |row, i| row << i + 1 }
      end
      Relation.new(columns: columns, rows: rows)
    ensure
      @regex_source_compile = false
    end
  end
  Engine.prepend(RegexExpressions)
end
