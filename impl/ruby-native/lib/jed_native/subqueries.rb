# frozen_string_literal: true

module JedNative
  # Subquery bodies contribute their own cost. An independent body is folded once
  # per statement; a correlated body runs at the expression site for each row.
  module Subqueries
    ROW_QUERY_OPERATORS = %w[= <> != < <= > >=].freeze

    def run(statement, params = [])
      @subquery_hoists = {}.compare_by_identity
      super
    ensure
      @subquery_hoists = nil
    end

    def compile(node, columns)
      if node[:kind] == :binary && ROW_QUERY_OPERATORS.include?(node[:op]) && node.dig(:left, :kind) == :row && node.dig(:right, :kind) == :subquery
        left = compile(node[:left], columns)
        right = compile_query_expression(node[:right], columns, row_fields: left.args)
        return Expression.new(kind: :row_query_compare, type: 'boolean', value: node[:op], args: [left, right])
      end
      if node[:kind] == :in && node[:query]
        return compile_query_membership(node, columns, false)
      end
      if node[:kind] == :quantified && node.dig(:right, :kind) == :subquery
        return compile_query_membership(node, columns, true)
      end
      return compile_query_expression(node, columns) if %i[subquery exists array_query].include?(node[:kind])
      super
    end

    def evaluate(expression, row, group = nil)
      case expression.kind
      when :row
        values = super
        CompositeValue.new(values, type_name: 'record', field_names: values.each_index.map { |index| "f#{index + 1}" })
      when :query_value
        charge
        relation = correlated(expression.extra[:scope], row) { select(expression.value) }
        begin
          query_expression_value(expression.extra[:query_kind], relation, expression.type, expression.extra[:row_value], expression.extra[:negated])
        ensure
          query_memory_release(relation) if @query_memory
        end
      when :row_query_compare
        charge
        left, right = expression.args.map { |argument| evaluate(argument, row, group) }
        compare_sql_rows(expression.value, left, right)
      when :quantified
        charge
        left, values = expression.args.map { |argument| evaluate(argument, row, group) }
        return nil if values.nil?
        all = expression.extra[:quantifier] == 'all'
        unknown = false
        values.each do |right|
          charge
          scalar_work(expr_operator = expression.extra[:op], left, right)
          charge if [expression.args[0].type, expression.args[1].type.delete_suffix('[]')].sort == %w[f32 f64]
          value = Types.binary(expr_operator, left, right, type: 'boolean')
          return !all if value == !all
          unknown ||= value.nil?
        end
        unknown ? nil : all
      when :query_membership
        charge
        left = evaluate(expression.args.first, row, group)
        data = expression.extra
        choices = if data[:correlated]
                    query_relation = correlated(data[:scope], row) { select(expression.value) }
                    query_relation.rows
                  else data[:rows]
                  end
        all = data[:quantifier] == 'all'
        result = all
        unknown = false
        choices.each do |values|
          charge
          right = data[:row_value] ? values : values.first
          scalar_work(data[:op], left, right) unless data[:row_value]
          value = data[:row_value] ? compare_sql_rows(data[:op], left, right) : Types.binary(data[:op], left, right, type: 'boolean')
          unknown ||= value.nil?
          if all
            return false if value == false
          elsif value == true
            result = true
            break if data[:quantified]
          end
        end
        result = nil if !result && unknown || all && unknown
        data[:negated] && !result.nil? ? !result : result
      else super
      end
    ensure
      query_memory_release(query_relation) if @query_memory && query_relation
    end

    private

    def compile_query_expression(node, columns, row_fields: nil)
      query = node[:query]
      metadata = query_metadata(query, @outer.flat_map(&:first) + columns)
      kind = node[:kind]
      expected = row_fields ? row_fields.length : 1
      if kind != :exists && metadata[:columns].length != expected
        raise Error.new('42601', 'subquery column count does not match its operand')
      end
      if row_fields
        row_fields.zip(metadata[:columns]).each { |left, right| validate_comparison(left, Expression.new(kind: :source, type: right[:type])) }
      end
      type = if row_fields then 'record'
             elsif kind == :exists then 'boolean'
             else metadata[:columns].first[:type]
             end
      type = 'text' if type == 'unknown'
      type += '[]' if kind == :array_query
      if metadata[:correlated]
        Expression.new(kind: :query_value, type: type, value: query,
                       extra: { scope: columns, query_kind: kind, row_value: !!row_fields, negated: node[:negated] })
      else
        relation = hoisted_query(query)
        value = query_expression_value(kind, relation, type, !!row_fields, node[:negated])
        Expression.new(kind: :literal, type: type, value: value, extra: { subquery_origin: true })
      end
    end

    def query_expression_value(kind, relation, type, row_value, negated)
      case kind
      when :exists then negated ? relation.rows.empty? : !relation.rows.empty?
      when :array_query then ArrayValue.new(relation.rows.map(&:first), element_type: type[0...-2])
      else
        raise Error.new('21000', 'scalar subquery returned multiple rows') if relation.rows.length > 1
        row_value ? relation.rows.first : relation.rows.first&.first
      end
    end

    def compile_query_membership(node, columns, quantified)
      left_node = quantified ? node[:left] : node[:expr] || node[:left]
      left = compile(left_node, columns)
      query = quantified ? node[:right][:query] : node[:query]
      metadata = query_metadata(query, @outer.flat_map(&:first) + columns)
      row_value = left_node[:kind] == :row
      fields = row_value ? left.args : [left]
      unless fields.length == metadata[:columns].length
        raise Error.new('42601', 'subquery column count does not match its operand')
      end
      begin
        fields.zip(metadata[:columns]).each { |field, column| validate_comparison(field, Expression.new(kind: :source, type: column[:type])) }
      rescue Error => error
        raise Error.new('42883', 'quantified comparison has incompatible operands') if quantified && error.sqlstate == '42804'
        raise
      end
      rows = hoisted_query(query).rows unless metadata[:correlated]
      if !quantified && !row_value && !metadata[:correlated]
        return Expression.new(kind: :literal, type: 'boolean', value: !!node[:negated], extra: { subquery_origin: true }) if rows.empty?
        comparisons = rows.map do |values|
          right = Expression.new(kind: :literal, type: metadata[:columns].first[:type], value: values.first, extra: { subquery_origin: true })
          arguments = [left, right]
          arguments = arguments.map { |argument| widen_float(argument) } if arguments.map(&:type).sort == %w[f32 f64]
          Expression.new(kind: :binary, type: 'boolean', value: '=', args: arguments)
        end
        expression = comparisons.reduce { |a, b| Expression.new(kind: :binary, type: 'boolean', value: 'or', args: [a, b]) }
        return node[:negated] ? Expression.new(kind: :unary, type: 'boolean', value: 'not', args: [expression]) : expression
      end
      Expression.new(kind: :query_membership, type: 'boolean', value: query, args: [left],
                     extra: { scope: columns, correlated: metadata[:correlated], rows: rows, row_value: row_value,
                              quantified: quantified, quantifier: quantified ? node[:quantifier] : 'any',
                              op: quantified ? node[:op] : '=', negated: node[:negated] })
    end

    def hoisted_query(query)
      return select(query) unless @subquery_hoists
      entries = (@subquery_hoists[query] ||= {})
      scope = @ctes.map { |name, relation| [name, relation.object_id] }
      entries[scope] ||= select(query)
    end
  end
  Engine.prepend(Subqueries)
end
