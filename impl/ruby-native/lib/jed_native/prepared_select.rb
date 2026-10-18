# frozen_string_literal: true

module JedNative
  # The common single-relation lane keeps resolved offsets and parameter sites
  # beside a parsed statement. A table object is immutable after publication;
  # replacing it on any write invalidates this plan and its lookup map together.
  # Other relational shapes continue through the general executor.
  module PreparedSelect
    Plan = Struct.new(:table, :source, :columns, :projections, :predicate, :orders,
                      :parameters, :parameter_types, :lookup, :index, keyword_init: true)
    CACHE_ENTRIES = 64
    STATEFUL_FUNCTIONS = %w[nextval currval setval lastval current_setting now
                            current_date current_time localtime localtimestamp
                            clock_timestamp uuidv4 gen_random_uuid uuidv7 random].freeze

    # Build one lookup map per published table, at the write/import boundary.
    # Plans share it; preparing another SQL string must not scan the table or
    # retain another full copy of the same index.
    def refresh_primary_indexes
      @tables.each do |name, stored|
        signature = [stored[:rows].object_id, stored[:columns].object_id, stored[:constraints].object_id]
        next if stored[:native_primary_signature] == signature
        primary = stored[:constraints].find { |constraint| constraint[:kind] == :primary_key }
        ordinal = if primary && primary[:columns].length == 1
          stored[:columns].index { |column| column[:name] == primary[:columns][0] }
        end
        ordinal = nil unless ordinal && Types::INTEGER_BITS.key?(stored[:columns][ordinal][:type])
        index = ordinal && stored[:rows].to_h { |row| [row[ordinal], row] }.freeze
        @tables[name] = stored.merge(native_primary_signature: signature,
                                    native_primary_ordinal: ordinal, native_primary_index: index,
                                    native_primary_sorted: index && index.sort_by(&:first).freeze)
      end
      @native_select_plans&.delete_if do |_statement, plan|
        plan.source && !@tables[plan.source[:name]].equal?(plan.table)
      end
    end

    # A resolved integer primary-key bound is safe to test before reading any
    # stored row. Residual expressions are still evaluated on every candidate.
    def primary_scan(stored, predicate)
      ordinal = stored[:native_primary_ordinal]
      return nil unless ordinal && predicate
      bounds = []
      visit = lambda do |expression|
        if expression.kind == :binary && expression.value == 'and'
          expression.args.each { |arg| visit.call(arg) }
        elsif expression.kind == :binary && %w[= < <= > >=].include?(expression.value)
          left, right = expression.args
          operator = expression.value
          if right.kind == :column
            left, right = right, left
            operator = { '<' => '>', '<=' => '>=', '>' => '<', '>=' => '<=', '=' => '=' }.fetch(operator)
          end
          if left.kind == :column && left.value == ordinal && %i[literal param outer].include?(right.kind) && Types::INTEGER_BITS.key?(right.type)
            bounds << [operator, evaluate(right, [])]
          end
        end
      end
      visit.call(predicate)
      return nil if bounds.empty?
      return [] if bounds.any? { |_, value| value.nil? }
      minimum, maximum, equal = nil, nil, nil
      bounds.each do |operator, value|
        case operator
        when '='
          return [] if equal && equal != value
          equal = value
        when '>', '>='
          value += 1 if operator == '>'
          minimum = minimum ? [minimum, value].max : value
        when '<', '<='
          value -= 1 if operator == '<'
          maximum = maximum ? [maximum, value].min : value
        end
      end
      return [] if minimum && maximum && minimum > maximum
      return [] if equal && (minimum && equal < minimum || maximum && equal > maximum)
      charge unless stored[:rows].empty?
      if equal
        row = stored[:native_primary_index][equal]
        row ? [row] : []
      else
        sorted = stored[:native_primary_sorted]
        first = minimum ? sorted.bsearch_index { |key, _| key >= minimum } || sorted.length : 0
        last = maximum ? sorted.bsearch_index { |key, _| key > maximum } || sorted.length : sorted.length
        sorted[first...last].map(&:last)
      end
    end

    def select(statement)
      return super unless cacheable_select?(statement)
      source = statement[:from]&.first
      stored = source && table(source[:name])
      types = @params.map { |value| Types.infer(value) }
      @native_select_plans ||= {}.compare_by_identity
      plan = @native_select_plans[statement]
      unless plan && plan.table.equal?(stored) && plan.parameter_types == types
        plan = resolve_select_plan(statement, source, stored, types)
        @native_select_plans.shift if @native_select_plans.length >= CACHE_ENTRIES
        @native_select_plans[statement] = plan
      end
      plan.parameters.each do |parameter|
        cast_value(@params[parameter.value], parameter.type) if parameter.extra&.dig(:context_type)
      end
      input = if plan.lookup
        key = evaluate(plan.lookup, [])
        charge unless key.nil? || stored[:rows].empty?
        row = key.nil? ? nil : plan.index[key]
        row ? [row] : []
      elsif stored
        candidates = primary_scan(stored, plan.predicate)
        charge unless candidates || stored[:rows].empty?
        candidates || stored[:rows]
      else
        [[]]
      end
      charge(input.length) if stored
      output, sort_keys = [], []
      row_index = 0
      while row_index < input.length
        row = input[row_index]
        row_index += 1
        next if plan.predicate && evaluate(plan.predicate, row) != true
        values = Array.new(plan.projections.length)
        column_index = 0
        while column_index < values.length
          values[column_index] = evaluate(plan.projections[column_index], row)
          column_index += 1
        end
        output << values
        unless plan.orders.empty?
          sort_keys << plan.orders.map { |kind, expression, _| kind == :output ? values[expression] : evaluate(expression, row) }
        end
      end
      unless plan.orders.empty?
        indices = (0...output.length).to_a
        indices.sort! do |left, right|
          compare = 0
          index = 0
          while index < plan.orders.length
            compare = order_compare(sort_keys[left][index], sort_keys[right][index], plan.orders[index][2])
            break unless compare.zero?
            index += 1
          end
          compare
        end
        output = indices.map { |index| output[index] }
      end
      charge(output.length)
      Relation.new(columns: plan.columns, rows: output)
    end

    private

    def cacheable_select?(statement)
      return false unless @outer.empty? && @ctes.empty?
      return false unless statement[:kind] == :select
      return false if statement[:with] || statement[:distinct] || statement[:distinct_on] || statement[:having] || statement[:limit] || statement[:offset]
      return false unless (statement[:joins] || []).empty? && (statement[:group_by] || []).empty? && (statement[:windows] || {}).empty?
      sources = statement[:from] || []
      return false if sources.length > 1
      if (source = sources.first)
        return false unless source[:kind] == :table && @tables.key?(source[:name]) && !source[:database] && !source[:columns] && !source[:column_aliases]
        return false if @ctes.key?(source[:name])
      end
      statements = statement[:columns].map { |c| c[:expr] } + [statement[:where]] + (statement[:order_by] || []).map { |o| o[:expr] }
      statements.all? { |expression| cacheable_expression?(expression) }
    end

    def cacheable_expression?(node)
      return true unless node
      case node[:kind]
      when :literal, :column, :param, :star then true
      when :cast, :unary then cacheable_expression?(node[:expr])
      when :binary then cacheable_expression?(node[:left]) && cacheable_expression?(node[:right])
      when :call
        !STATEFUL_FUNCTIONS.include?(node[:name]) && !node[:over] && !node[:filter] && !node[:within_group] && !Functions.aggregate?(node[:name]) && (node[:args] || []).all? { |arg| cacheable_expression?(arg) }
      else false
      end
    end

    def resolve_select_plan(statement, source, stored, types)
      columns = stored ? table_columns(stored, source[:alias] || source[:name]) : []
      projections = expand_stars(statement[:columns], columns)
      expressions = projections.map { |p| compile(p[:expr], columns) }
      outputs = projections.zip(expressions).map { |p, e| { name: p[:alias] || expression_name(p[:expr]), type: e.type } }
      predicate = compile_filter(statement[:where], columns)
      orders = (statement[:order_by] || []).map do |order|
        node = order[:expr]
        if node[:kind] == :literal && node[:value].is_a?(Integer)
          index = node[:value] - 1
          raise Error.new('42P10', 'ORDER BY position out of range') unless index.between?(0, outputs.length - 1)
          [:output, index, order]
        elsif node[:kind] == :column && !node[:table] && (index = outputs.index { |c| c[:name] == node[:name] })
          [:output, index, order]
        else
          [:input, compile(node, columns), order]
        end
      end
      parameters = []
      visit = lambda do |expression|
        return unless expression.is_a?(Expression)
        parameters << expression if expression.kind == :param
        (expression.args || []).each { |arg| visit.call(arg) }
      end
      (expressions + [predicate] + orders.filter_map { |kind, e, _| e if kind == :input }).each { |e| visit.call(e) }
      lookup, index = nil, nil
      if stored && predicate&.kind == :binary && predicate.value == '='
        if (ordinal = stored[:native_primary_ordinal])
          left, right = predicate.args
          left, right = right, left if right.kind == :column
          if left.kind == :column && left.value == ordinal && %i[literal param].include?(right.kind) && Types::INTEGER_BITS.key?(right.type)
            lookup = right
            index = stored[:native_primary_index]
          end
        end
      end
      Plan.new(table: stored, source: source, columns: outputs, projections: expressions,
               predicate: predicate, orders: orders, parameters: parameters,
               parameter_types: types, lookup: lookup, index: index)
    end
  end
  Engine.prepend(PreparedSelect)
end
