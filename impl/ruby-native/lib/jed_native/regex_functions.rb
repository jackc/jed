# frozen_string_literal: true

require_relative 'regex'

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
      original = source.chars
      input = program.input(source)
      if %w[regexp_like regex_match regex_imatch].include?(name)
        return !program.search(input, 0, context: context).nil?
      end
      if %w[regexp_match substring_regex].include?(name)
        match = program.search(input, 0, context: context)
        return nil unless match
        if name == 'substring_regex'
          span = program.span(match, program.groups.positive? ? 1 : 0)
          return span && original[span[0]...span[1]].join
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
            return span && original[span[0]...span[1]].join
          end
          first, last = program.span(match)
          position = last == first ? last + 1 : last
        end
        return count if name == 'regexp_count'
        return name == 'regexp_instr' ? 0 : nil
      end
      if name == 'regexp_replace'
        template = replacement_tokens(args[2])
        matches, position = [], 0
        while position <= input.length
          match = program.search(input, position, context: context)
          break unless match
          context&.charge(args[2].bytesize)
          matches << match
          break unless options.include?('g')
          first, last = program.span(match)
          position = last == first ? last + 1 : last
        end
        prefix = byte_prefix(original)
        bytes = 0
        output_pieces(program, matches, template, original.length) do |piece|
          bytes += piece.is_a?(String) ? piece.bytesize : prefix[piece[1]] - prefix[piece[0]]
        end
        context&.charge(bytes)
        context&.consume_scalar_bytes(bytes)
        output = +''
        output_pieces(program, matches, template, original.length) do |piece|
          if piece.is_a?(String)
            output << piece
          else
            piece[0].upto(piece[1] - 1) { |index| output << original[index] }
          end
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
          output << original[split_at...first].join
          split_at = last
        end
        previous_end = last
        position = last == first ? last + 1 : last
      end
      output << original[split_at..].join
      name == 'regexp_split_to_array' ? ArrayValue.new(output, element_type: 'text') : output
    end

    def byte_prefix(original)
      sums = [0]
      original.each { |char| sums << sums[-1] + char.bytesize }
      sums
    end

    def capture_array(program, match, original, context)
      groups = program.groups.zero? ? [0] : (1..program.groups)
      spans = groups.map { |group| program.span(match, group) }
      prefix = byte_prefix(original)
      bytes = 16 + spans.sum { |span| 32 + (span ? prefix[span[1]] - prefix[span[0]] : 0) }
      context&.charge(bytes)
      context&.consume_scalar_bytes(bytes)
      ArrayValue.new(spans.map { |span| span && original[span[0]...span[1]].join }, element_type: 'text')
    end

    def replacement_tokens(template)
      chars, result, index = template.chars, [], 0
      while index < chars.length
        char = chars[index]
        if char == '\\' && index + 1 < chars.length
          index += 1
          char = chars[index]
          char = if char.between?('0', '9') then char.to_i
          elsif char == '&' then 0
          else char
          end
        end
        result << char
        index += 1
      end
      result
    end

    def output_pieces(program, matches, template, length)
      position = 0
      matches.each do |match|
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
      if node[:kind] == :binary && %w[~ ~* !~ !~*].include?(node[:op])
        args = [compile(node[:left], columns), compile(node[:right], columns)]
        raise Error.new('42804', 'regular expression operands must be text') unless args.all? { |arg| %w[text unknown null].include?(arg.type) }
        options = node[:op].end_with?('*') ? 'i' : ''
        if args[1].kind == :literal && args[1].value
          program = (@regex_programs ||= {}.compare_by_identity)[node] ||= RegexProgram.new(args[1].value, insensitive: options == 'i')
        end
        return Expression.new(kind: :regex_operator, type: 'boolean', value: node[:op], args: args, extra: { program: program, flags: options, charge_key: node })
      end
      return super unless node[:kind] == :call && (RegexFunctions::NAMES.include?(node[:name]) || node[:name] == 'substring')
      if %w[regexp_matches regexp_split_to_table].include?(node[:name]) && !@regex_source_compile
        raise Error.new('42883', 'set-returning regex functions require FROM')
      end
      expression = super
      name = node[:name]
      if name == 'substring'
        return expression unless expression.args.length == 2 && %w[text unknown null].include?(expression.args[1].type)
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
          program = RegexProgram.new(args[1], insensitive: expression.extra[:flags] == 'i')
          charge(program.code.length)
        end
        matched = program.match?(args[0], context: self)
        expression.value.start_with?('!') ? !matched : matched
      else
        RegexFunctions.call(expression.value, args, context: self, program: program)
      end
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
