# frozen_string_literal: true

require_relative 'lexer'

module JedNative
  # A handwritten recursive-descent / precedence-climbing parser. ASTs are plain
  # hashes tagged by :kind. Operators keep their SQL spelling in :op. Type names
  # are canonical strings; optional modifiers live in :type_mod.
  #
  # SELECT :from contains its leading relations; :joins contains authored joins
  # {kind: :inner/:left/:right/:full/:cross, relation:, on:, using:, natural:}.
  # A relation is :table {name:, database:, alias:, columns:}, :subquery {query:,
  # alias:, columns:}, :function {expr:, alias:, columns:}, or :join_group.
  # :with is [{name:, columns:, query:, materialized: nil/true/false, search:,
  # cycle:}]; :recursive belongs to the statement containing the WITH.
  # :set_operation nodes have :op, :all, :left, :right; ORDER/LIMIT attach to the whole node.
  # Function :over is a window name or {base:, partition_by:, order_by:, frame:}.
  class Parser
    MAX_EXPR_DEPTH = 256
    LABEL_STOPS = %w[array as char character create day except fetch filter for from grant group having hour intersect into isnull limit minute month notnull offset on order over overlaps precision returning second to union varying where window with within without year].to_h { |word| [word, true] }.freeze
    RELATION_STOPS = (LABEL_STOPS.keys + %w[join inner left right full outer cross natural using set values do]).to_h { |word| [word, true] }.freeze
    PRECEDENCE = {
      'or' => 10, 'and' => 20,
      '=' => 30, '<>' => 30, '<' => 30, '>' => 30, '<=' => 30, '>=' => 30,
      '~' => 30, '~*' => 30, '!~' => 30, '!~*' => 30,
      '||' => 40, '@>' => 40, '<@' => 40, '&&' => 40,
      '<<' => 40, '>>' => 40, '&<' => 40, '&>' => 40, '-|-' => 40,
      '->' => 40, '->>' => 40, '#>' => 40, '#>>' => 40,
      '?' => 40, '?|' => 40, '?&' => 40, '#-' => 40, '@?' => 40, '@@' => 40,
      '+' => 50, '-' => 50, '*' => 60, '/' => 60, '%' => 60
    }.freeze
    TYPE_ALIASES = {
      'smallint' => 'i16', 'int2' => 'i16', 'int' => 'i32', 'integer' => 'i32', 'int4' => 'i32',
      'bigint' => 'i64', 'int8' => 'i64', 'numeric' => 'decimal', 'bool' => 'boolean',
      'real' => 'f32', 'float4' => 'f32', 'float8' => 'f64', 'double precision' => 'f64',
      'character varying' => 'varchar', 'character' => 'char',
      'timestamp with time zone' => 'timestamptz', 'timestamp without time zone' => 'timestamp',
      'int4range' => 'i32range', 'int8range' => 'i64range'
    }.freeze

    def initialize(sql)
      @tokens = Lexer.new(sql).tokens
      @index = 0
      @depth = 0
      compute_parenthesized_queries
    end

    def parse
      statements = []
      until current.kind == :eof
        next if accept(';')
        statements << statement
        syntax('unexpected trailing token') unless current.kind == :eof || accept(';')
      end
      statements
    end

    # Useful for stored expressions whose grammar is independent of a statement.
    def parse_expression
      value = expression
      syntax('unexpected trailing token') unless current.kind == :eof
      value
    end

    private

    def current
      @tokens[@index]
    end

    def token(offset = 0)
      @tokens[@index + offset] || @tokens[-1]
    end

    def at?(value, offset = 0)
      candidate = token(offset)
      (candidate.kind == :word || candidate.kind == :symbol) && candidate.value == value
    end

    def accept(value)
      return false unless at?(value)
      @index += 1
      true
    end

    def expect(value)
      syntax("expected #{value}") unless accept(value)
    end

    def advance
      result = current
      @index += 1
      result
    end

    def identifier
      syntax('expected identifier') unless identifier_token?
      advance.value
    end

    def identifier_token?(offset = 0)
      [:word, :identifier].include?(token(offset).kind)
    end

    def syntax(message)
      value = current.kind == :eof ? 'end of input' : current.value.inspect
      raise Error.new('42601', "#{message} at #{value} (byte #{current.offset + 1})")
    end

    def unsupported(message)
      raise Error.new('0A000', message)
    end

    def nested
      @depth += 1
      @depth_peak = [@depth_peak || 0, @depth].max
      raise Error.new('54001', 'statement too complex: nesting depth exceeds 256') if @depth > MAX_EXPR_DEPTH
      yield
    ensure
      @depth -= 1
    end

    def separated
      result = [yield]
      result << yield while accept(',')
      result
    end

    def name_list
      expect('(')
      names = separated { identifier }
      expect(')')
      names
    end

    def qualified_name
      first = identifier
      accept('.') ? [identifier, first] : [first, nil]
    end

    def if_exists
      return false unless at?('if') && at?('exists', 1)
      @index += 2
      true
    end

    def if_not_exists
      return false unless at?('if') && at?('not', 1) && at?('exists', 2)
      @index += 3
      true
    end

    def statement
      return with_statement if at?('with')
      return query if at?('select') || at?('values') || at?('(')
      case current.value
      when 'create' then create_statement
      when 'drop' then drop_statement
      when 'alter' then alter_statement
      when 'insert' then insert_statement
      when 'update' then update_statement
      when 'delete' then delete_statement
      when 'begin', 'start' then begin_statement
      when 'commit', 'end'
        advance
        accept('transaction') || accept('work')
        { kind: :commit }
      when 'rollback', 'abort'
        advance
        accept('transaction') || accept('work')
        if accept('to')
          accept('savepoint')
          { kind: :rollback_to, name: identifier }
        else
          { kind: :rollback }
        end
      when 'savepoint'
        advance
        { kind: :savepoint, name: identifier }
      when 'release'
        advance
        accept('savepoint') if identifier_token?(1)
        { kind: :release, name: identifier }
      when 'analyze'
        advance
        name, database = qualified_name
        { kind: :analyze, table: name, database: database, columns: at?('(') ? name_list : nil }
      when 'explain' then explain_statement
      when 'reindex' then reindex_statement
      when 'truncate'
        advance
        accept('table')
        { kind: :truncate, tables: separated { identifier } }
      else
        syntax('expected SQL statement')
      end
    end

    def begin_statement
      if accept('start')
        expect('transaction')
      else
        expect('begin')
        accept('transaction') || accept('work')
      end
      mode = nil
      if accept('read')
        mode = accept('only') ? :read_only : (expect('write'); :read_write)
      end
      { kind: :begin, mode: mode, read_only: mode == :read_only }
    end

    def explain_statement
      expect('explain')
      options = { analyze: false, verbose: false, costs: true, lane: false }
      if accept('analyze')
        options[:analyze] = true
      elsif at?('(') && !query_start?(1)
        advance
        seen = {}
        separated do
          option = identifier.to_sym
          syntax('unknown or repeated EXPLAIN option') unless options.key?(option) && !seen[option]
          seen[option] = true
          options[option] = boolean_option
        end
        expect(')')
      end
      body = statement
      supported = %i[select set_operation values insert update delete].include?(body[:kind]) || body[:kind] == :create_table && body[:query]
      syntax('EXPLAIN requires a query or data-modifying statement') unless supported
      { kind: :explain, statement: body, **options }
    end

    def boolean_option
      return true if accept('true') || accept('on')
      return false if accept('false') || accept('off')
      true
    end

    def reindex_statement
      expect('reindex')
      options = {}
      if accept('(')
        separated do
          key = identifier
          unsupported('REINDEX TABLESPACE is not supported') if key == 'tablespace'
          syntax('unknown REINDEX option') unless %w[verbose concurrently].include?(key)
          value = boolean_option
          if current.kind == :string || current.kind == :integer
            text = advance.value.to_s.downcase
            syntax('invalid boolean option') unless %w[true false on off 1 0].include?(text)
            value = %w[true on 1].include?(text)
          end
          options[key.to_sym] = value
        end
        expect(')')
      end
      object = identifier
      unsupported('REINDEX SCHEMA is not supported') if object == 'schema'
      syntax('expected INDEX, TABLE, DATABASE or SYSTEM') unless %w[index table database system].include?(object)
      options[:concurrently] = true if at?('concurrently') && (identifier_token?(1) || %w[database system].include?(object)) && accept('concurrently')
      name, database = identifier_token? ? qualified_name : [nil, nil]
      syntax('expected REINDEX target') if name.nil? && %w[index table].include?(object)
      { kind: :reindex, object: object.to_sym, name: name, database: database, options: options }
    end

    def with_statement
      expect('with')
      recursive = accept('recursive')
      definitions = separated do
        name = identifier
        columns = at?('(') ? name_list : nil
        expect('as')
        materialized = nil
        if accept('not')
          expect('materialized')
          materialized = false
        elsif accept('materialized')
          materialized = true
        end
        expect('(')
        body = nested { statement }
        expect(')')
        cte = { name: name, columns: columns, query: body, materialized: materialized }
        if accept('search')
          order = identifier
          syntax('expected DEPTH or BREADTH') unless %w[depth breadth].include?(order)
          expect('first')
          expect('by')
          keys = separated { identifier }
          expect('set')
          cte[:search] = { order: order, columns: keys, column: identifier }
        end
        if accept('cycle')
          keys = separated { identifier }
          expect('set')
          mark = identifier
          yes = literal(true, 'boolean')
          no = literal(false, 'boolean')
          if accept('to')
            syntax('CYCLE marks must be unsigned constants') if at?('-') || at?('+')
            yes = expression(31)
            expect('default')
            syntax('CYCLE marks must be unsigned constants') if at?('-') || at?('+')
            no = expression(31)
          end
          expect('using')
          cte[:cycle] = { columns: keys, mark: mark, value: yes, default: no, path: identifier }
        end
        cte
      end
      body = statement
      syntax('multiple WITH clauses') if body[:with]
      body.merge(with: definitions, recursive: recursive)
    end

    def query
      previous_peak, query_base = @depth_peak, @depth
      @depth_peak = @depth
      result = set_expression(0)
      if accept('order')
        expect('by')
        syntax('multiple ORDER BY clauses') if result[:order_by] && !result[:order_by].empty?
        result[:order_by] = separated { order_term }
      end
      2.times do
        if accept('limit')
          syntax('multiple LIMIT clauses') if result[:limit]
          result[:limit] = accept('all') ? nil : expression(31)
        elsif accept('offset')
          syntax('multiple OFFSET clauses') if result[:offset]
          result[:offset] = expression(31)
          accept('rows') || accept('row')
        else
          break
        end
      end
      result.merge(native_query_base: query_base, native_depth: @depth_peak - query_base)
    ensure
      @depth_peak = [previous_peak || 0, @depth_peak || 0].max
    end

    def set_expression(minimum)
      left = if accept('(')
               body = nested { at?('with') ? with_statement : query }
               expect(')')
               body
             elsif at?('values')
               values_query
             else
               select_query
             end
      chain_depth = 0
      while %w[union intersect except].any? { |word| at?(word) }
        precedence = at?('intersect') ? 2 : 1
        break if precedence < minimum
        op = advance.value
        all = accept('all')
        accept('distinct') unless all
        right = nested { set_expression(precedence + 1) }
        left = { kind: :set_operation, op: op, all: all, left: left, right: right }
        chain_depth += 1
        complexity!(chain_depth)
      end
      left
    end

    def select_query
      expect('select')
      distinct = at?('distinct') && !at?('from', 1) && token(1).kind != :eof && accept('distinct')
      distinct_on = nil
      distinct_on = parenthesized_expressions if distinct && accept('on')
      accept('all') unless distinct
      result = { kind: :select, columns: select_items, from: [], joins: [], distinct: distinct,
                 group_by: [], order_by: [], windows: {} }
      result[:distinct_on] = distinct_on if distinct_on
      if accept('into')
        temporary = !at?('.', 1) && (accept('temp') || accept('temporary'))
        unsupported('UNLOGGED tables are not supported') if accept('unlogged')
        accept('table')
        name, database = qualified_name
        result[:into] = { name: name, database: database, temporary: temporary }
      end
      result[:from], result[:joins] = from_clause if accept('from')
      result[:where] = expression if accept('where')
      if accept('group')
        expect('by')
        result[:group_by] = separated { group_item }
      end
      result[:having] = expression if accept('having')
      if accept('window')
        separated do
          name = identifier
          expect('as')
          expect('(')
          raise Error.new('42P20', "window #{name} is already defined") if result[:windows].key?(name)
          result[:windows][name] = window_definition
          expect(')')
        end
      end
      result
    end

    def values_query
      expect('values')
      { kind: :values, rows: separated { parenthesized_expressions }, order_by: [] }
    end

    def select_items
      separated do
        expr = accept('*') ? { kind: :star } : expression
        explicit = accept('as')
        quoted = current.kind == :identifier
        name = identifier if explicit || alias_available?(LABEL_STOPS)
        syntax('a star projection cannot have an alias') if name && expr[:kind] == :star
        { expr: expr, alias: name, alias_quoted: quoted, alias_explicit: explicit }
      end
    end

    def alias_available?(stops)
      current.kind == :identifier || (current.kind == :word && !stops[current.value])
    end

    def from_clause
      relations = []
      joins = []
      loop do
        relations << table_reference
        joins.concat(join_clauses.map { |join| join.merge(from_index: relations.length - 1) })
        break unless accept(',')
      end
      [relations, joins]
    end

    def table_reference
      lateral = accept('lateral')
      if accept('(')
        relation = if query_start?
                     { kind: :subquery, query: nested { at?('with') ? with_statement : query } }
                   else
                     from, joins = nested { from_clause }
                     syntax('parenthesized FROM requires a joined table') if from.length != 1 || joins.empty? && from.first[:kind] != :join_group || lateral
                     { kind: :join_group, from: from, joins: joins }
                   end
        expect(')')
      else
        first_quoted = current.kind == :identifier
        last_quoted = at?('.', 1) ? token(2).kind == :identifier : first_quoted
        name, database = qualified_name
        relation = if at?('(')
                     syntax('table function names cannot be qualified') if database
                     { kind: :function, expr: function_call(name, false) }
                   else
                     { kind: :table, name: name, database: database, quoted: last_quoted, database_quoted: first_quoted }
                   end
      end
      relation[:lateral] = lateral
      relation[:native_ref_depth] = @depth + 1 if relation[:kind] == :table
      if relation[:kind] == :function && accept('with')
        syntax('WITH ORDINALITY is not supported')
      end
      explicit = accept('as')
      if explicit && at?('(')
        relation[:definitions] = relation_column_definitions
      elsif explicit || alias_available?(RELATION_STOPS)
        relation[:alias_quoted] = current.kind == :identifier
        relation[:alias_explicit] = explicit
        relation[:alias] = identifier
        if at?('(')
          if relation[:kind] == :function && identifier_token?(1) && identifier_token?(2)
            relation[:definitions] = relation_column_definitions
          else
            expect('(')
            quotes = []
            relation[:columns] = separated do
              quotes << (current.kind == :identifier)
              identifier
            end
            expect(')')
            relation[:columns_quoted] = quotes
          end
        end
      end
      relation
    end

    def relation_column_definitions
      expect('(')
      definitions = separated do
        name = identifier
        type, modifier = type_name
        { name: name, type: type, type_mod: modifier }
      end
      expect(')')
      definitions
    end

    def join_clauses
      joins = []
      while %w[join inner left right full cross natural].any? { |word| at?(word) }
        natural = accept('natural')
        kind = :inner
        if %w[inner left right full cross].any? { |word| at?(word) }
          kind = advance.value.to_sym
          accept('outer') if %i[left right full].include?(kind)
        end
        syntax('NATURAL CROSS JOIN is not valid') if natural && kind == :cross
        expect('join')
        relation = table_reference
        join = { kind: kind, relation: relation, natural: natural }
        if accept('on')
          syntax('CROSS and NATURAL joins do not accept ON') if kind == :cross || natural
          join[:on] = expression
        elsif accept('using')
          syntax('CROSS and NATURAL joins do not accept USING') if kind == :cross || natural
          join[:using] = name_list
        elsif kind != :cross && !natural
          syntax('JOIN requires ON or USING')
        end
        joins << join
      end
      joins
    end

    def group_item
      if %w[rollup cube].any? { |word| at?(word) } && at?('(', 1)
        kind = advance.value.to_sym
        expect('(')
        sets = separated { group_set }
        expect(')')
        { kind: kind, sets: sets }
      elsif at?('grouping') && at?('sets', 1)
        @index += 2
        expect('(')
        sets = separated { group_set }
        expect(')')
        { kind: :grouping_sets, sets: sets }
      elsif at?('(')
        items = group_set
        items.length == 1 ? items.first : { kind: :grouping_sets, sets: [items] }
      else
        expression
      end
    end

    def group_set
      return [expression] unless accept('(')
      items = at?(')') ? [] : separated { expression }
      expect(')')
      items
    end

    def order_term
      expr = expression
      direction = accept('desc') ? :desc : (accept('asc'); :asc)
      nulls = nil
      if accept('nulls')
        nulls = accept('first') ? :first : (expect('last'); :last)
      end
      { expr: expr, direction: direction, desc: direction == :desc, nulls: nulls }
    end

    def window_definition
      result = { partition_by: [], order_by: [] }
      result[:base] = identifier if identifier_token? && !%w[partition order rows range groups].any? { |word| at?(word) }
      if accept('partition')
        expect('by')
        result[:partition_by] = separated { expression }
      end
      if accept('order')
        expect('by')
        result[:order_by] = separated { order_term }
      end
      if %w[rows range groups].any? { |word| at?(word) }
        frame = { mode: advance.value.to_sym }
        if accept('between')
          frame[:start] = frame_bound
          expect('and')
          frame[:end] = frame_bound
        else
          frame[:start] = frame_bound
          frame[:end] = { kind: :current_row }
        end
        if accept('exclude')
          frame[:exclude] = if accept('current')
                              expect('row')
                              :current_row
                            elsif accept('no')
                              expect('others')
                              :no_others
                            else
                              choice = identifier.to_sym
                              syntax('expected CURRENT ROW, GROUP, TIES or NO OTHERS') unless %i[group ties].include?(choice)
                              choice
                            end
        end
        result[:frame] = frame
      end
      result
    end

    def frame_bound
      if accept('unbounded')
        direction = identifier
        syntax('expected PRECEDING or FOLLOWING') unless %w[preceding following].include?(direction)
        { kind: :unbounded, direction: direction.to_sym }
      elsif accept('current')
        expect('row')
        { kind: :current_row }
      else
        value = expression(31)
        direction = identifier
        syntax('expected PRECEDING or FOLLOWING') unless %w[preceding following].include?(direction)
        { kind: :offset, value: value, direction: direction.to_sym }
      end
    end

    def query_start?(offset = 0)
      at?('select', offset) || (at?('values', offset) && at?('(', offset + 1)) || at?('with', offset) || @paren_queries[@index + offset]
    end

    def compute_parenthesized_queries
      closes = {}
      stack = []
      @tokens.each_with_index do |item, index|
        next unless item.kind == :symbol
        stack << index if item.value == '('
        closes[stack.pop] = index if item.value == ')' && !stack.empty?
      end
      @paren_queries = {}
      (@tokens.length - 1).downto(0) do |index|
        next unless closes[index]
        inner = @tokens[index + 1]
        direct = inner.kind == :word && (inner.value == 'select' || inner.value == 'with' || (inner.value == 'values' && @tokens[index + 2].value == '('))
        following = @tokens[closes[index] + 1]
        continuation = following && ((following.kind == :symbol && following.value == ')') || (following.kind == :word && %w[union intersect except order limit offset].include?(following.value)))
        @paren_queries[index] = (direct || @paren_queries[index + 1]) && continuation
      end
    end

    def complexity!(extra)
      @depth_peak = [@depth_peak || 0, @depth + extra].max
      raise Error.new('54001', 'statement too complex: nesting depth exceeds 256') if @depth + extra > MAX_EXPR_DEPTH
    end

    def literal(value, type, untyped = false)
      { kind: :literal, value: value, type: type, untyped: untyped }
    end

    def expression(minimum = 0)
      nested do
        left = prefix
        chain = 0
        compared = false
        loop do
          if minimum <= 30 && (at?('is') || at?('between') || at?('in') || at?('like') || at?('ilike') || (at?('not') && %w[in between like ilike].any? { |word| at?(word, 1) }))
            syntax('comparison predicates cannot be chained') if compared
            compared = true
            left = predicate(left)
          elsif minimum <= 65 && at?('at') && at?('time', 1) && at?('zone', 2)
            @index += 3
            right = expression(66)
            left = { kind: :call, name: 'timezone', args: [right, left] }
          else
            op = current.kind == :word || current.kind == :symbol ? current.value : nil
            precedence = PRECEDENCE[op]
            break unless precedence && precedence >= minimum
            syntax('comparison operators cannot be chained') if precedence == 30 && compared
            compared = precedence == 30 if precedence <= 30
            syntax('star is only valid as a projection') if left[:kind] == :star
            advance
            if precedence == 30 && %w[any some all].any? { |word| at?(word) } && at?('(', 1)
              quantifier = advance.value
              expect('(')
              right = query_start? ? { kind: :subquery, query: at?('with') ? with_statement : query } : expression
              expect(')')
              left = { kind: :quantified, op: op, left: left, right: right, quantifier: quantifier == 'some' ? 'any' : quantifier }
            else
              right = expression(precedence + 1)
              syntax('star is only valid as a projection') if right[:kind] == :star
              left = { kind: :binary, op: op, left: left, right: right }
            end
          end
          chain += 1
          complexity!(chain)
        end
        left
      end
    end

    def predicate(left)
      if accept('is')
        negated = accept('not')
        if accept('distinct')
          expect('from')
          return { kind: :binary, op: negated ? 'is not distinct from' : 'is distinct from', left: left, right: expression(31) }
        end
        if accept('json')
          json_kind = %w[value scalar object array].any? { |word| at?(word) } ? advance.value : 'value'
          unique = nil
          if accept('with') || accept('without')
            unique = @tokens[@index - 1].value == 'with'
            expect('unique')
            expect('keys')
          end
          return { kind: :is_json, expr: left, negated: negated, json_kind: json_kind, unique: unique }
        end
        if %w[null true false unknown].any? { |word| at?(word) }
          target = advance.value
          return { kind: :unary, op: "is #{negated ? 'not ' : ''}#{target}", expr: left }
        end
        syntax('expected NULL, DISTINCT, boolean or JSON after IS')
      end
      negated = accept('not')
      if accept('in')
        expect('(')
        if query_start?
          right = { kind: :subquery, query: at?('with') ? with_statement : query }
          node = { kind: :in, expr: left, query: right[:query], negated: negated }
        else
          values = separated { expression(31) }
          node = { kind: :in, expr: left, values: values, negated: negated }
        end
        expect(')')
        node
      elsif accept('between')
        symmetric = accept('symmetric')
        accept('asymmetric') unless symmetric
        low = expression(31)
        expect('and')
        high = expression(31)
        { kind: :between, expr: left, low: low, high: high, negated: negated, symmetric: symmetric }
      else
        op = accept('ilike') ? 'ilike' : (expect('like'); 'like')
        right = expression(31)
        escape = accept('escape') ? expression(31) : nil
        { kind: :binary, op: negated ? "not #{op}" : op, left: left, right: right, escape: escape }
      end
    end

    def prefix
      if (at?('-') || at?('+') || at?('not')) && !at?('.', 1)
        op = advance.value
        value = expression(op == 'not' ? 25 : 70)
        if op == '-' && value[:kind] == :literal && %w[i32 i64 decimal].include?(value[:type])
          number = value[:type] == 'decimal' ? "-#{value[:value]}" : -value[:value]
          type = value[:type] == 'decimal' ? 'decimal' : 'i64'
          return literal(number, type, true)
        end
        return op == '+' ? value : { kind: :unary, op: op, expr: value }
      end
      value = primary
      chain = 0
      loop do
        if accept('::')
          type, modifier = type_name
          value = { kind: :cast, expr: value, type: type, type_mod: modifier }
        elsif accept('collate')
          value = { kind: :collate, expr: value, name: identifier }
        elsif accept('[')
          specs = []
          loop do
            low = at?(':') || at?(']') ? nil : expression
            slice = accept(':')
            high = slice && !at?(']') ? expression : nil
            expect(']')
            specs << { index: slice ? nil : low, low: slice ? low : nil, high: high, slice: slice }
            break unless accept('[')
          end
          value = { kind: :subscript, expr: value, specs: specs }
        elsif accept('.')
          syntax('a row field base needs an enclosing pair of parentheses') if value[:kind] == :row && value[:bare_row]
          value = accept('*') ? { kind: :field_star, expr: value } : { kind: :field, expr: value, name: identifier }
        else
          break
        end
        chain += 1
        complexity!(chain)
      end
      value
    end

    def primary
      case current.kind
      when :integer
        value = advance.value
        return literal(value, 'i64', true)
      when :decimal then return literal(advance.value, 'decimal', true)
      when :string then return literal(advance.value, 'text', true)
      when :param then return { kind: :param, index: advance.value }
      end
      if accept('(')
        if query_start?
          value = { kind: :subquery, query: nested { at?('with') ? with_statement : query } }
        else
          value = expression
          if accept(',')
            value = { kind: :row, args: [value] + separated { expression }, bare_row: true }
          elsif value[:kind] == :row
            value = value.merge(bare_row: false)
          end
        end
        expect(')')
        return value
      end
      syntax('expected expression') unless identifier_token?
      # Qualification wins over keyword meaning: null.x is a column reference.
      if at?('.', 1)
        table_quoted = current.kind == :identifier
        table = identifier
        expect('.')
        return { kind: :star, table: table, table_quoted: table_quoted } if accept('*')
        quoted = current.kind == :identifier
        return { kind: :column, name: identifier, table: table, quoted: quoted, table_quoted: table_quoted }
      end
      quoted = current.kind == :identifier
      name = advance.value
      unless quoted
        case name
        when 'null' then return literal(nil, 'null') unless at?('(')
        when 'true' then return literal(true, 'boolean') unless at?('(')
        when 'false' then return literal(false, 'boolean') unless at?('(')
        when 'default' then return { kind: :default } unless at?('(')
        when 'current_timestamp' then return { kind: :call, name: 'now', args: [] } unless at?('(')
        when 'current_date', 'current_time', 'localtimestamp', 'localtime'
          return { kind: :call, name: name, args: [] } unless at?('(')
        when 'case' then return case_expression
        when 'exists'
          expect('(')
          body = nested { at?('with') ? with_statement : query }
          expect(')')
          return { kind: :exists, query: body }
        when 'array'
          if accept('[')
            args = at?(']') ? [] : separated { at?('[') ? nested_array : expression }
            expect(']')
            return { kind: :array, args: args }
          elsif accept('(')
            body = nested { at?('with') ? with_statement : query }
            expect(')')
            return { kind: :array_query, query: body }
          end
        when 'cast'
          if accept('(')
            expr = expression
            expect('as')
            type, modifier = type_name
            expect(')')
            return { kind: :cast, expr: expr, type: type, type_mod: modifier }
          end
        when 'extract'
          if accept('(')
            field = current.kind == :string ? advance.value : identifier
            expect('from')
            expr = expression
            expect(')')
            return { kind: :call, name: 'extract', args: [literal(field, 'text'), expr] }
          end
        end
      end
      name = 'character varying' if !quoted && name == 'character' && accept('varying')
      if current.kind == :string
        value = { kind: :cast, expr: literal(advance.value, 'text'), type: TYPE_ALIASES.fetch(name, name), type_mod: nil }
        value[:type_mod] = interval_modifier if name == 'interval' && interval_field?
        return value
      end
      return function_call(name, quoted) if at?('(')
      { kind: :column, name: name, quoted: quoted }
    end

    def nested_array
      nested do
        expect('[')
        args = at?(']') ? [] : separated { at?('[') ? nested_array : expression }
        expect(']')
        { kind: :array, args: args }
      end
    end

    def case_expression
      operand = at?('when') ? nil : expression
      branches = []
      while accept('when')
        condition = expression
        expect('then')
        branches << [condition, expression]
      end
      syntax('CASE requires WHEN') if branches.empty?
      fallback = accept('else') ? expression : literal(nil, 'null')
      expect('end')
      { kind: :case, operand: operand, branches: branches, else: fallback }
    end

    def parenthesized_expressions
      expect('(')
      values = at?(')') ? [] : separated { expression }
      expect(')')
      values
    end

    def function_call(name, quoted)
      expect('(')
      return json_special(name) if !quoted && %w[json json_value json_query json_exists json_table].include?(name)
      if !quoted && name == 'position'
        substring = expression(31)
        expect('in')
        string = expression(31)
        expect(')')
        return { kind: :call, name: name, args: [string, substring] }
      end
      distinct = accept('distinct')
      syntax('DISTINCT requires an argument') if distinct && (at?(')') || at?('*'))
      accept('all') unless distinct
      args = []
      names = []
      variadic = false
      order = []
      unless at?(')')
        loop do
          if accept('variadic')
            variadic = true
          end
          arg_name = nil
          if identifier_token? && at?('=>', 1)
            arg_name = identifier
            advance
          end
          syntax('positional argument follows named argument') if !arg_name && names.any?
          syntax('duplicate named argument') if arg_name && names.include?(arg_name)
          arg = accept('*') ? { kind: :star } : expression
          args << arg
          names << arg_name
          if !quoted && args.length == 1 && name == 'substring' && (at?('from') || at?('for'))
            start = literal(1, 'i64', true)
            count = nil
            if accept('from')
              start = expression
              count = expression if accept('for')
            else
              expect('for')
              count = expression
              start = expression if accept('from')
            end
            expect(')')
            return { kind: :call, name: name, args: count ? [arg, start, count] : [arg, start] }
          elsif !quoted && args.length == 1 && name == 'overlay' && accept('placing')
            replacement = expression
            expect('from')
            start = expression
            count = accept('for') ? expression : nil
            expect(')')
            return { kind: :call, name: name, args: count ? [arg, replacement, start, count] : [arg, replacement, start] }
          end
          break unless accept(',')
          syntax('VARIADIC argument must be last') if variadic
        end
        if accept('order')
          syntax('ORDER BY cannot follow aggregate *') if args.any? { |arg| arg[:kind] == :star }
          expect('by')
          order = separated { order_term }
        end
      end
      expect(')')
      if !quoted && name == 'row'
        return { kind: :row, args: args }
      end
      result = { kind: :call, name: name, args: args, distinct: distinct }
      result[:arg_names] = names if names.any?
      result[:variadic] = true if variadic
      result[:order_by] = order unless order.empty?
      if accept('within')
        expect('group')
        expect('(')
        expect('order')
        expect('by')
        result[:within_group] = separated { order_term }
        expect(')')
      end
      if accept('filter')
        expect('(')
        expect('where')
        result[:filter] = expression
        expect(')')
      end
      if accept('over')
        if accept('(')
          result[:over] = window_definition
          expect(')')
        else
          result[:over] = identifier
        end
      end
      if !quoted && %w[coalesce greatest least nullif grouping].include?(name)
        syntax("invalid arguments for #{name}") if args.empty? || args.any? { |arg| arg[:kind] == :star } || name == 'nullif' && args.length != 2
        syntax("invalid modifier for #{name}") if distinct || variadic || names.any? || result[:filter] || result[:over] || result[:within_group] || !order.empty?
      end
      syntax('WITHIN GROUP cannot combine with DISTINCT or argument ORDER BY') if result[:within_group] && (distinct || !order.empty?)
      result
    end

    def type_name
      name = identifier
      if name == 'double' && accept('precision')
        name = 'double precision'
      elsif name == 'character' && accept('varying')
        name = 'character varying'
      end
      modifier = nil
      if accept('(')
        precision = signed_integer
        scale = accept(',') ? signed_integer : nil
        expect(')')
        modifier = { precision: precision, scale: scale }
      end
      if %w[timestamp time].include?(name) && (at?('with') || at?('without'))
        zone = advance.value
        expect('time')
        expect('zone')
        name = "#{name} #{zone} time zone"
      end
      modifier = (modifier || {}).merge(interval_modifier) if name == 'interval' && interval_field?
      name = TYPE_ALIASES.fetch(name, name)
      dimensions = 0
      while accept('[')
        advance if current.kind == :integer
        expect(']')
        dimensions += 1
      end
      name += '[]' if dimensions.positive?
      [name, modifier]
    end

    def interval_field?
      %w[year month day hour minute second].any? { |word| at?(word) }
    end

    def interval_modifier
      first = identifier
      last = accept('to') ? identifier : nil
      precision = nil
      if accept('(')
        precision = signed_integer
        expect(')')
      end
      { fields: last ? "#{first} to #{last}" : first, precision: precision }
    end

    def signed_integer
      negative = accept('-')
      accept('+') unless negative
      syntax('expected integer') unless current.kind == :integer
      value = advance.value
      negative ? -value : value
    end

    def create_statement
      expect('create')
      replace = false
      if accept('or')
        expect('replace')
        replace = true
      end
      temporary = accept('temp') || accept('temporary')
      unsupported('UNLOGGED tables are not supported') if accept('unlogged')
      unique = accept('unique')
      recursive = accept('recursive')
      unsupported('materialized views are not supported') if accept('materialized')
      if accept('table')
        create_table(temporary)
      elsif accept('index')
        create_index(unique)
      elsif accept('sequence')
        skip = if_not_exists
        name, database = qualified_name
        options = sequence_options
        syntax('SEQUENCE NAME is only allowed in an identity definition') if options.key?(:name)
        { kind: :create_sequence, name: name, database: database, if_not_exists: skip, options: options }
      elsif accept('type')
        name = identifier
        expect('as')
        expect('(')
        fields = separated do
          field = identifier
          type, modifier = type_name
          nullable = true
          if accept('not')
            expect('null')
            nullable = false
          end
          { name: field, type: type, type_mod: modifier, nullable: nullable }
        end
        expect(')')
        { kind: :create_type, name: name, fields: fields }
      elsif accept('view')
        skip = if_not_exists
        syntax('OR REPLACE cannot combine with IF NOT EXISTS') if skip && replace
        name, database = qualified_name
        columns = at?('(') ? name_list : nil
        syntax('recursive view requires a column list') if recursive && !columns
        options = accept('with') ? option_list : {}
        expect('as')
        body = at?('with') ? with_statement : query
        check = nil
        if accept('with')
          check = accept('local') ? :local : (accept('cascaded'); :cascaded)
          expect('check')
          expect('option')
        end
        { kind: :create_view, name: name, database: database, columns: columns, query: body,
          temporary: temporary, replace: replace, recursive: recursive, if_not_exists: skip,
          options: options, check_option: check }
      else
        syntax('expected TABLE, INDEX, SEQUENCE, TYPE or VIEW')
      end
    end

    def create_table(temporary)
      skip = if_not_exists
      name, database = qualified_name
      result = { kind: :create_table, name: name, database: database, temporary: temporary,
                 if_not_exists: skip, columns: [], constraints: [] }
      if at?('(')
        # A CTAS rename list contains names only. Column definitions always have
        # a type after their first name, so this lookahead is unambiguous.
        if identifier_token?(1) && (at?(',', 2) || at?(')', 2))
          result[:column_names] = name_list
        else
          expect('(')
          separated do
            if table_constraint_start?
              result[:constraints] << table_constraint
            else
              result[:columns] << column_definition
            end
          end
          expect(')')
        end
      end
      result[:on_commit] = on_commit if at?('on')
      if accept('as')
        result[:query] = at?('with') ? with_statement : query
        result[:with_data] = true
        if accept('with')
          result[:with_data] = !accept('no')
          result[:with_no_data] = !result[:with_data]
          expect('data')
        end
      elsif result[:columns].empty?
        syntax('expected table definition or AS query')
      end
      result
    end

    def on_commit
      expect('on')
      expect('commit')
      return :drop if accept('drop')
      action = accept('delete') ? :delete_rows : (expect('preserve'); :preserve_rows)
      expect('rows')
      action
    end

    def table_constraint_start?
      (at?('constraint') && identifier_token?(1) && %w[primary unique check foreign exclude].any? { |word| at?(word, 2) }) || (at?('primary') && at?('key', 1)) ||
        (at?('foreign') && at?('key', 1)) ||
        (at?('check') && at?('(', 1)) || (at?('unique') && at?('(', 1)) || at?('exclude')
    end

    def table_constraint
      name = accept('constraint') ? identifier : nil
      result = if accept('primary')
                 expect('key')
                 { kind: :primary_key, columns: name_list }
               elsif accept('unique')
                 { kind: :unique, columns: name_list }
               elsif accept('check')
                 expect('(')
                 expr = expression
                 expect(')')
                 { kind: :check, expr: expr }
               elsif accept('foreign')
                 expect('key')
                 columns = name_list
                 references.merge(kind: :foreign_key, columns: columns)
               elsif accept('exclude')
                 method = accept('using') ? identifier : 'btree'
                 expect('(')
                 elements = separated do
                   expr = expression
                   expect('with')
                   op = advance.value
                   { expr: expr, op: op }
                 end
                 expect(')')
                 predicate = nil
                 if accept('where')
                   expect('(')
                   predicate = expression
                   expect(')')
                 end
                 { kind: :exclude, method: method, elements: elements, where: predicate }
               else
                 syntax('expected table constraint')
               end
      result[:name] = name
      result
    end

    def column_definition
      name = identifier
      type, modifier = type_name
      column = { name: name, type: type, type_mod: modifier, nullable: true, primary_key: false,
                 unique: false, checks: [] }
      loop do
        constraint_name = accept('constraint') ? identifier : nil
        if accept('primary')
          expect('key')
          column[:primary_key] = true
          column[:nullable] = false
        elsif accept('not')
          expect('null')
          column[:nullable] = false
        elsif accept('null')
          column[:nullable] = true
        elsif accept('default')
          syntax('multiple defaults or identity clauses') if column[:default] || column[:identity] || %w[smallserial serial bigserial serial2 serial4 serial8].include?(type)
          column[:default] = expression
        elsif accept('unique')
          column[:unique] = true
          column[:unique_name] = constraint_name
        elsif accept('check')
          expect('(')
          column[:checks] << { kind: :check, expr: expression, name: constraint_name }
          expect(')')
        elsif at?('references')
          column[:references] = references.merge(name: constraint_name)
        elsif accept('collate')
          column[:collation] = identifier
        elsif accept('generated')
          syntax('multiple defaults or identity clauses') if column[:identity] || column[:default] || %w[smallserial serial bigserial serial2 serial4 serial8].include?(type)
          column[:identity] = identity_definition
        else
          syntax('expected column constraint') if constraint_name
          break
        end
      end
      column
    end

    def references
      expect('references')
      table, database = qualified_name
      result = { table: table, database: database, ref_columns: at?('(') ? name_list : nil,
                 match: :simple, on_delete: :no_action, on_update: :no_action }
      result[:match] = identifier.to_sym if accept('match')
      while accept('on')
        action = identifier
        syntax('expected UPDATE or DELETE') unless %w[update delete].include?(action)
        behavior = if accept('no')
                     expect('action')
                     :no_action
                   elsif accept('set')
                     accept('null') ? :set_null : (expect('default'); :set_default)
                   else
                     identifier.to_sym
                   end
        result["on_#{action}".to_sym] = behavior
      end
      result
    end

    def identity_definition
      mode = if accept('always')
               :always
             else
               expect('by')
               expect('default')
               :by_default
             end
      expect('as')
      expect('identity')
      options = {}
      if accept('(')
        options = sequence_options
        expect(')')
      end
      { mode: mode, options: options }
    end

    def create_index(unique)
      skip = if_not_exists
      name = at?('on') && (!at?('on', 1) || at?('(', 2) || at?('.', 2)) ? nil : identifier
      syntax('IF NOT EXISTS requires an index name') if skip && !name
      expect('on')
      table, database = qualified_name
      method = accept('using') ? identifier : 'btree'
      expect('(')
      elements = separated do
        expr = expression
        direction_explicit = at?('desc') || at?('asc')
        direction = accept('desc') ? :desc : (accept('asc'); :asc)
        nulls = nil
        nulls = accept('first') ? :first : (expect('last'); :last) if accept('nulls')
        { expr: expr, direction: direction, direction_explicit: direction_explicit, nulls: nulls }
      end
      expect(')')
      include_columns = accept('include') ? name_list : []
      where = accept('where') ? expression : nil
      { kind: :create_index, name: name, table: table, database: database, unique: unique,
        if_not_exists: skip, method: method, elements: elements, columns: elements.map { |item| item[:expr][:name] },
        include: include_columns, where: where }
    end

    def sequence_options
      options = {}
      loop do
        key = { 'as' => :type, 'owned' => :owned_by, 'sequence' => :name }[current.value]
        key ||= token(1).value.to_sym if at?('no') && token(1).value
        key ||= current.value.to_sym if %w[increment minvalue maxvalue start restart cache cycle].include?(current.value)
        syntax('duplicate sequence option') if key && options.key?(key)
        if accept('as')
          options[:type], = type_name
        elsif accept('increment')
          accept('by')
          options[:increment] = signed_integer
        elsif accept('minvalue')
          options[:minvalue] = signed_integer
        elsif accept('maxvalue')
          options[:maxvalue] = signed_integer
        elsif accept('start')
          accept('with')
          options[:start] = signed_integer
        elsif accept('restart')
          accept('with')
          options[:restart] = current.kind == :integer || at?('-') ? signed_integer : true
        elsif accept('cache')
          options[:cache] = signed_integer
        elsif accept('cycle')
          options[:cycle] = true
        elsif accept('no')
          key = identifier
          syntax('expected MINVALUE, MAXVALUE or CYCLE') unless %w[minvalue maxvalue cycle].include?(key)
          options[key.to_sym] = key == 'cycle' ? false : nil
        elsif accept('owned')
          expect('by')
          if accept('none')
            options[:owned_by] = nil
          else
            table = identifier
            expect('.')
            options[:owned_by] = [table, identifier]
          end
        elsif accept('sequence')
          expect('name')
          options[:name] = identifier
        else
          break
        end
      end
      options
    end

    def option_list
      expect('(')
      options = {}
      separated do
        name = identifier
        raise Error.new('22023', 'option specified more than once') if options.key?(name)
        value = true
        if accept('=')
          value = current.kind == :integer || at?('-') ? signed_integer : advance.value
        end
        options[name] = value
      end
      expect(')')
      options
    end

    def drop_statement
      expect('drop')
      object = identifier
      syntax('expected TABLE, INDEX, VIEW, SEQUENCE or TYPE') unless %w[table index view sequence type].include?(object)
      skip = if_exists
      targets = separated do
        name, database = qualified_name
        { name: name, database: database }
      end
      cascade = accept('cascade')
      accept('restrict') unless cascade
      { kind: "drop_#{object}".to_sym, name: targets.first[:name], database: targets.first[:database],
        names: targets.map { |target| target[:name] }, targets: targets, if_exists: skip, cascade: cascade }
    end

    def alter_statement
      expect('alter')
      object = identifier
      skip = if_exists
      name, database = qualified_name
      result = { kind: "alter_#{object}".to_sym, name: name, table: name, database: database, if_exists: skip }
      if accept('rename')
        if accept('to')
          return result.merge(action: :rename, new_name: identifier)
        end
        constraint = accept('constraint')
        accept('column') unless constraint
        old = identifier
        expect('to')
        return result.merge(action: constraint ? :rename_constraint : :rename_column, column: old, new_name: identifier)
      end
      if object == 'sequence'
        return result.merge(options: sequence_options)
      end
      syntax('expected TABLE, VIEW or SEQUENCE') unless %w[table view].include?(object)
      result[:actions] = separated do
        unsupported('ALTER OWNER and SET SCHEMA are not supported') if at?('owner') || at?('set') && at?('schema', 1)
        if accept('add')
          if table_constraint_start?
            { kind: :add_constraint, constraint: table_constraint }
          else
            accept('column')
            absent = if_not_exists
            { kind: :add_column, column: column_definition, if_not_exists: absent }
          end
        elsif accept('drop')
          if accept('constraint')
            absent = if_exists
            target = identifier
            cascade = accept('cascade')
            accept('restrict') unless cascade
            { kind: :drop_constraint, name: target, if_exists: absent, cascade: cascade }
          elsif accept('primary')
            expect('key')
            cascade = accept('cascade')
            accept('restrict') unless cascade
            { kind: :drop_primary_key, cascade: cascade }
          else
            accept('column')
            absent = if_exists
            target = identifier
            cascade = accept('cascade')
            accept('restrict') unless cascade
            { kind: :drop_column, column: target, if_exists: absent, cascade: cascade }
          end
        elsif accept('alter')
          accept('column')
          target = identifier
          action = alter_column_action.merge(column: target)
          if at?('set') || at?('restart')
            actions = [action]
            actions << alter_column_action.merge(column: target) while at?('set') || at?('restart')
            keys = actions.flat_map { |item| item[:kind] == :set_generated ? [:generated] : (item[:options] || {}).keys }
            syntax('duplicate identity option') unless keys.uniq.length == keys.length
            { kind: :alter_identity, column: target, actions: actions }
          else
            action
          end
        elsif accept('set')
          { kind: :set_options, options: option_list }
        elsif accept('reset')
          { kind: :reset_options, options: name_list }
        else
          syntax('expected ALTER action')
        end
      end
      result
    end

    def alter_column_action
      if accept('set')
        if accept('default')
          { kind: :set_default, expr: expression }
        elsif accept('not')
          expect('null')
          { kind: :set_not_null }
        elsif accept('data')
          expect('type')
          type, modifier = type_name
          { kind: :set_type, type: type, type_mod: modifier, using: accept('using') ? expression : nil }
        elsif accept('generated')
          mode = accept('always') ? :always : (expect('by'); expect('default'); :by_default)
          { kind: :set_generated, mode: mode }
        else
          syntax('invalid identity sequence option') if %w[as restart sequence owned].any? { |word| at?(word) }
          options = sequence_options
          syntax('invalid identity sequence option') if options.empty? || (options.keys & %i[type name owned_by]).any?
          { kind: :set_identity_options, options: options }
        end
      elsif accept('drop')
        if accept('default')
          { kind: :drop_default }
        elsif accept('not')
          expect('null')
          { kind: :drop_not_null }
        elsif accept('identity')
          { kind: :drop_identity, if_exists: if_exists }
        else
          syntax('expected DEFAULT, NOT NULL or IDENTITY')
        end
      elsif accept('type')
        type, modifier = type_name
        { kind: :set_type, type: type, type_mod: modifier, using: accept('using') ? expression : nil }
      elsif accept('add')
        expect('generated')
        { kind: :add_identity, identity: identity_definition }
      elsif at?('restart')
        { kind: :set_identity_options, options: sequence_options }
      else
        syntax('expected column alteration')
      end
    end

    def insert_statement
      expect('insert')
      expect('into')
      table, database = qualified_name
      table_alias = accept('as') ? identifier : nil
      columns = at?('(') && (!query_start?(1) || (identifier_token?(1) && (at?(',', 2) || at?(')', 2)))) ? name_list : nil
      overriding = nil
      if accept('overriding')
        overriding = identifier
        expect('value')
      end
      result = { kind: :insert, table: table, database: database, alias: table_alias,
                 columns: columns, overriding: overriding }
      if accept('default')
        expect('values')
        result[:rows] = [[]]
        result[:default_values] = true
      else
        body = at?('with') ? with_statement : query
        if body[:kind] == :values && (body[:order_by] || []).empty? && !body[:limit] && !body[:offset] && !body[:with]
          result[:rows] = body[:rows]
        else
          result[:query] = body
        end
      end
      if accept('on')
        expect('conflict')
        conflict = {}
        if at?('(')
          expect('(')
          conflict[:elements] = separated do
            expr = expression
            if expr[:kind] == :collate && expr[:expr][:kind] == :collate
              raise Error.new('42601', 'repeated COLLATE in conflict target')
            end
            expr = expr.merge(opclass: identifier) if identifier_token? && !%w[asc desc nulls].include?(current.value)
            expr = expr.merge(direction: advance.value) if at?('asc') || at?('desc')
            if accept('nulls')
              raise Error.new('42601', 'expected FIRST or LAST') unless at?('first') || at?('last')
              expr = expr.merge(nulls: advance.value)
            end
            expr
          end
          expect(')')
          conflict[:columns] = conflict[:elements].map { |expr| expr[:name] }
          conflict[:predicate] = expression if accept('where')
        elsif accept('on')
          expect('constraint')
          conflict[:constraint] = identifier
        end
        expect('do')
        if accept('nothing')
          conflict[:action] = :nothing
        else
          expect('update')
          expect('set')
          conflict[:action] = :update
          conflict[:assignments] = assignments
          conflict[:where] = expression if accept('where')
        end
        result[:on_conflict] = conflict
      end
      parse_returning(result)
      result
    end

    def assignments
      separated do
        if at?('(')
          columns = name_list
          expect('=')
          { columns: columns, expr: expression }
        else
          column = identifier
          specs = []
          while accept('[')
            low = at?(':') ? nil : expression
            slice = accept(':')
            high = slice && !at?(']') ? expression : nil
            expect(']')
            specs << { index: slice ? nil : low, low: slice ? low : nil, high: high, slice: slice }
          end
          expect('=')
          value = { column: column, expr: expression }
          value[:subscripts] = specs unless specs.empty?
          value
        end
      end
    end

    def update_statement
      expect('update')
      table, database = qualified_name
      table_alias = accept('as') ? identifier : nil
      expect('set')
      result = { kind: :update, table: table, database: database, alias: table_alias, assignments: assignments }
      result[:from], result[:joins] = from_clause if accept('from')
      result[:where] = expression if accept('where')
      parse_returning(result)
      result
    end

    def delete_statement
      expect('delete')
      expect('from')
      table, database = qualified_name
      table_alias = accept('as') ? identifier : nil
      result = { kind: :delete, table: table, database: database, alias: table_alias }
      result[:using], result[:joins] = from_clause if accept('using')
      result[:where] = expression if accept('where')
      parse_returning(result)
      result
    end

    def parse_returning(result)
      return unless accept('returning')
      if at?('with') && at?('(', 1) && %w[old new].include?(token(2).value) && at?('as', 3)
        advance
        expect('(')
        result[:returning_aliases] = {}
        separated do
          kind = identifier
          syntax('expected OLD or NEW') unless %w[old new].include?(kind)
          expect('as')
          name = identifier
          syntax('duplicate RETURNING alias') if result[:returning_aliases].key?(kind.to_sym) || result[:returning_aliases].value?(name)
          result[:returning_aliases][kind.to_sym] = name
        end
        expect(')')
      end
      result[:returning] = select_items
    end

    def json_special(name)
      context = expression
      format = json_format
      if name == 'json'
        unique = nil
        if accept('with') || accept('without')
          unique = @tokens[@index - 1].value == 'with'
          expect('unique')
          expect('keys')
        end
        expect(')')
        return { kind: :json_constructor, expr: context, format_json: format, unique: unique }
      end
      expect(',')
      path = expression
      path_name = accept('as') ? identifier : nil
      passing = json_passing
      result = { kind: name.to_sym, context: context, path: path, path_name: path_name,
                 format_json: format, passing: passing }
      if name == 'json_table'
        expect('columns')
        result[:columns] = json_table_columns
      else
        if accept('returning')
          result[:returning], result[:type_mod] = type_name
          json_format
        end
        result.merge!(json_wrapper) if name == 'json_query'
        result.merge!(json_behaviors)
      end
      expect(')')
      result
    end

    def json_format
      return nil unless accept('format')
      expect('json')
      accept('encoding') ? identifier : true
    end

    def json_passing
      return [] unless accept('passing')
      separated do
        expr = expression
        format = json_format
        expect('as')
        { name: identifier, expr: expr, format_json: format }
      end
    end

    def json_wrapper
      result = { wrapper: :without, keep_quotes: true }
      if accept('with')
        result[:wrapper] = accept('conditional') ? :conditional : (accept('unconditional'); :unconditional)
        accept('array')
        expect('wrapper')
      elsif accept('without')
        accept('array')
        expect('wrapper')
      end
      if accept('keep') || accept('omit')
        result[:keep_quotes] = @tokens[@index - 1].value == 'keep'
        expect('quotes')
        if accept('on')
          expect('scalar')
          expect('string')
        end
      end
      result
    end

    def json_behaviors
      result = {}
      while %w[error null true false unknown empty default].any? { |word| at?(word) }
        kind = advance.value.to_sym
        value = nil
        if kind == :default
          value = expression
        elsif kind == :empty
          kind = accept('object') ? :empty_object : (accept('array'); :empty_array)
        end
        expect('on')
        target = identifier
        syntax('expected EMPTY or ERROR') unless %w[empty error].include?(target)
        key = "on_#{target}".to_sym
        syntax('duplicate JSON behavior') if result.key?(key)
        result[key] = { kind: kind, default: value }
      end
      result
    end

    def json_table_columns
      nested do
        expect('(')
        columns = separated do
          if accept('nested')
            accept('path')
            syntax('expected JSON path string') unless current.kind == :string
            path = advance.value
            name = accept('as') ? identifier : nil
            expect('columns')
            { kind: :nested, path: path, name: name, columns: json_table_columns }
          else
            name = identifier
            if accept('for')
              expect('ordinality')
              { kind: :ordinality, name: name }
            else
              type, modifier = type_name
              exists = accept('exists')
              format = json_format
              path = nil
              if accept('path')
                syntax('expected JSON path string') unless current.kind == :string
                path = advance.value
              end
              { kind: exists ? :exists : :regular, name: name, type: type, type_mod: modifier,
                format_json: format, path: path, **json_wrapper, **json_behaviors }
            end
          end
        end
        expect(')')
        columns
      end
    end
  end
end
