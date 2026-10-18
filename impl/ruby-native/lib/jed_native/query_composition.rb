# frozen_string_literal: true

module JedNative
  # VALUES bodies and set operations share recursive type unification, but their
  # query boundaries are meaningful: VALUES finalizes an all-NULL column to text,
  # while a SELECT operand's unknown column can adopt the other operand's type.
  module QueryComposition
    def common_type(left, right)
      left, right = Types.normalize(left), Types.normalize(right)
      if left.end_with?('[]') && right.end_with?('[]')
        a, b = left[0...-2], right[0...-2]
        compatible = a == b || Types::INTEGER_BITS.key?(a) && Types::INTEGER_BITS.key?(b) || %w[f32 f64].include?(a) && %w[f32 f64].include?(b)
        raise Error.new('42804', "types #{left} and #{right} cannot be matched") unless compatible
        return "#{common_type(a, b)}[]"
      end
      super(left, right)
    end

    def select_values(stmt, relation_only: false)
      width = stmt[:rows].first&.length || 0
      raise Error.new('42601', 'VALUES width mismatch') unless stmt[:rows].all? { |row| row.length == width }
      compiled = stmt[:rows].map do |row|
        row.map do |node|
          raise Error.new('42601', 'DEFAULT is only allowed in an INSERT VALUES slot') if node[:kind] == :default
          compile(node, [])
        end
      end
      types = Array.new(width, 'unknown')
      rows = compiled.map do |row|
        charge
        row.each_with_index.map do |expr, index|
          type, value = values_cell(expr)
          types[index] = common_type(types[index], type)
          value
        end
      end
      columns = types.each_with_index.map do |type, index|
        { name: "column#{index + 1}", type: type == 'unknown' ? 'text' : type, table: '*VALUES*' }
      end
      rows.map! { |row| row.each_with_index.map { |value, index| cast_value(value, columns[index][:type]) } }
      relation = apply_query_tail(stmt, Relation.new(columns: columns, rows: rows), set_operation: false)
      charge(relation.rows.length) unless relation_only
      relation
    end

    def select_set(stmt)
      left, right = select(stmt[:left]), select(stmt[:right])
      raise Error.new('42601', 'set-operation width mismatch') unless left.columns.length == right.columns.length
      columns = left.columns.each_with_index.map do |column, index|
        column.merge(type: common_type(column[:type], right.columns[index][:type]), table: nil)
      end
      operation = stmt[:op].to_s
      unless operation == 'union' && stmt[:all]
        columns.each { |column| require_set_equality(column[:type]) }
      end
      lrows, rrows = [left, right].map do |relation|
        relation.rows.map { |row| row.each_with_index.map { |value, index| cast_value(value, columns[index][:type]) } }
      end
      rows = case operation
             when 'union' then stmt[:all] ? lrows + rrows : (lrows + rrows).uniq
             when 'intersect', 'except'
               counts = rrows.tally
               input = stmt[:all] ? lrows : lrows.uniq
               input.select do |row|
                 found = counts.fetch(row, 0).positive?
                 counts[row] -= 1 if found && stmt[:all]
                 operation == 'intersect' ? found : !found
               end
             else raise Error.new('42601', 'unknown set operation')
             end
      apply_query_tail(stmt, Relation.new(columns: columns, rows: rows), set_operation: true)
    end

    def source_relation(source)
      query = source[:query]
      if source[:kind] == :subquery && query[:kind] == :values &&
          (query[:order_by] || []).empty? && !query[:limit] && !query[:offset] && !query[:with]
        relation = select_values(query, relation_only: true)
        names = source[:columns] || []
        if names.length > relation.columns.length
          raise Error.new('42601', 'too many VALUES column aliases')
        end
        columns = relation.columns.each_with_index.map do |column, index|
          column.merge(name: names[index] || column[:name], table: source[:alias])
        end
        return Relation.new(columns: columns, rows: relation.rows)
      end
      super
    end

    private

    def values_cell(expression)
      if expression.kind == :subquery
        relation = select(expression.value)
        raise Error.new('42601', 'scalar subquery must have one column') unless relation.columns.length == 1
        raise Error.new('21000', 'scalar subquery returned multiple rows') if relation.rows.length > 1
        [relation.columns.first[:type], relation.rows.first&.first]
      else
        value = evaluate(expression, [])
        type = Types.normalize(expression.type)
        type = Types.infer(value) if type == 'unknown' && !value.nil?
        [type, value]
      end
    end

    def require_set_equality(type)
      element = type.delete_suffix('[]')
      if %w[json jsonpath].include?(element)
        raise Error.new('42883', "could not identify an equality operator for type #{type}")
      end
      (@types[element] || []).each { |field| require_set_equality(field[:type]) }
    end

    def apply_query_tail(stmt, relation, set_operation:)
      orders = stmt[:order_by] || []
      keys = orders.map do |order|
        node = order[:expr]
        if node[:kind] == :literal && Types::INTEGER_BITS.key?(node[:type])
          index = node[:value] - 1
          raise Error.new('42P10', 'ORDER BY position is not in select list') unless index.between?(0, relation.columns.length - 1)
          Expression.new(kind: :column, type: relation.columns[index][:type], value: index)
        else
          if set_operation
            raise Error.new('42P01', 'table qualifier is not visible after a set operation') if node[:kind] == :column && node[:table]
            raise Error.new('0A000', 'set-operation ORDER BY accepts output columns or positions') unless node[:kind] == :column
          end
          compile(node, relation.columns)
        end
      end
      rows = relation.rows
      unless keys.empty?
        keyed = rows.each_with_index.map { |row, index| [row, keys.map { |key| evaluate(key, row) }, index] }
        keyed.sort! do |left, right|
          comparison = 0
          orders.each_with_index do |order, index|
            comparison = order_compare(left[1][index], right[1][index], order)
            break unless comparison.zero?
          end
          comparison.zero? ? left[2] <=> right[2] : comparison
        end
        rows = keyed.map(&:first)
      end
      offset = stmt[:offset] ? evaluate(compile(stmt[:offset], []), []) : 0
      limit = stmt[:limit] ? evaluate(compile(stmt[:limit], []), []) : nil
      offset ||= 0
      raise Error.new('2201X', 'negative OFFSET') if offset.negative?
      raise Error.new('2201W', 'negative LIMIT') if limit && limit.negative?
      rows = rows.drop(offset) unless offset.zero?
      rows = rows.take(limit) if limit
      Relation.new(columns: relation.columns, rows: rows)
    end
  end
  Engine.prepend(QueryComposition)
end
