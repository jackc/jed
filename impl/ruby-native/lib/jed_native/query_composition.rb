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
      raise Error.new('42803', 'aggregate functions are not allowed in VALUES') if local_aggregate?(stmt[:rows])
      width = stmt[:rows].first&.length || 0
      raise Error.new('42601', 'VALUES width mismatch') unless stmt[:rows].all? { |row| row.length == width }
      compiled = stmt[:rows].map do |row|
        row.map do |node|
          raise Error.new('42601', 'DEFAULT is only allowed in an INSERT VALUES slot') if node[:kind] == :default
          compile(node, [])
        end
      end
      types = Array.new(width, 'unknown')
      memory_owner = query_memory_owner if @query_memory
      rows = compiled.map do |row|
        charge
        values = row.each_with_index.map do |expr, index|
          type, value = values_cell(expr)
          types[index] = common_type(types[index], type)
          value
        end
        query_memory_add_row(memory_owner, values) if @query_memory
        values
      end
      columns = types.each_with_index.map do |type, index|
        { name: "column#{index + 1}", type: type == 'unknown' ? 'text' : type, table: '*VALUES*' }
      end
      rows.map! { |row| row.each_with_index.map { |value, index| cast_value(value, columns[index][:type]) } }
      relation = Relation.new(columns: columns, rows: rows)
      query_memory_attach(relation, memory_owner) if @query_memory
      relation = apply_query_tail(stmt, relation, set_operation: false)
      charge(relation.rows.length) unless relation_only
      relation
    end

    def sql_set_key(value)
      case value
      when Float then value.nan? ? :sql_nan : value
      when Array then value.map { |item| sql_set_key(item) }
      else value
      end
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
      if @query_memory
        memory_owner = query_memory_owner
        [left, right].each do |relation|
          owner = relation.instance_variable_get(:@native_memory_owner)
          if owner
            memory_owner.bytes += owner.bytes
            owner.bytes = 0
          end
        end
        state_owner = query_memory_owner
        types = columns.map { |column| column[:type] }
        remember = lambda do |set, row|
          unless set.key?(sql_set_key(row))
            query_memory_add(state_owner, MemorySchedule::ENTRY + query_memory_key_bytes(row, types))
            set[sql_set_key(row)] = 0
          end
          set[sql_set_key(row)] += 1
        end
        seen = {}
        rows = if operation == 'union'
          (lrows + rrows).select do |row|
            next true if stmt[:all]
            next false if seen.key?(sql_set_key(row))
            remember.call(seen, row)
            true
          end
        else
          counts = {}
          rrows.each { |row| remember.call(counts, row) }
          lrows.select do |row|
            found = counts.fetch(sql_set_key(row), 0).positive?
            counts[sql_set_key(row)] -= 1 if found && stmt[:all]
            next false unless operation == 'intersect' ? found : !found
            next true if stmt[:all]
            next false if seen.key?(sql_set_key(row))
            remember.call(seen, row)
            true
          end
        end
        query_memory_release(state_owner)
        retained = rows.sum { |row| query_memory_row_bytes(row, columns) }
        if retained > memory_owner.bytes
          query_memory_add(memory_owner, retained - memory_owner.bytes)
        else
          query_memory_release(memory_owner, memory_owner.bytes - retained)
        end
      else
        rows = case operation
             when 'union' then stmt[:all] ? lrows + rrows : (lrows + rrows).uniq { |row| sql_set_key(row) }
             when 'intersect', 'except'
               counts = rrows.map { |row| sql_set_key(row) }.tally
               input = stmt[:all] ? lrows : lrows.uniq { |row| sql_set_key(row) }
               input.select do |row|
                 found = counts.fetch(sql_set_key(row), 0).positive?
                 counts[sql_set_key(row)] -= 1 if found && stmt[:all]
                 operation == 'intersect' ? found : !found
               end
             else raise Error.new('42601', 'unknown set operation')
             end
      end
      relation = Relation.new(columns: columns, rows: rows)
      query_memory_attach(relation, memory_owner) if @query_memory
      apply_query_tail(stmt, relation, set_operation: true)
    end

    def source_relation(source)
      if source[:columns] && source[:kind] == :subquery
        width = query_metadata(source[:query], @outer.flat_map(&:first))[:columns].length
        raise Error.new('42P10', 'too many derived column aliases') if source[:columns].length > width
      end
      query = source[:query]
      if source[:kind] == :subquery && query[:kind] == :values &&
          (query[:order_by] || []).empty? && !query[:limit] && !query[:offset] && !query[:with]
        relation = select_values(query, relation_only: true)
        names = source[:columns] || []
        if names.length > relation.columns.length
          raise Error.new('42P10', 'too many VALUES column aliases')
        end
        columns = relation.columns.each_with_index.map do |column, index|
          column.merge(name: names[index] || column[:name], table: source[:alias])
        end
        result = Relation.new(columns: columns, rows: relation.rows)
        query_memory_transfer(relation, result) if @query_memory
        return result
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
            output = node
            output = output[:expr] while output[:kind] == :collate
            raise Error.new('42P01', 'table qualifier is not visible after a set operation') if output[:kind] == :column && output[:table]
            raise Error.new('0A000', 'set-operation ORDER BY accepts output columns or positions') unless output[:kind] == :column
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
      result = Relation.new(columns: relation.columns, rows: rows)
      if @query_memory
        kept = rows.to_h { |row| [row.object_id, true] }
        relation.rows.each do |row|
          query_memory_release(relation, query_memory_row_bytes(row, relation.columns)) unless kept[row.object_id]
        end
        query_memory_transfer(relation, result)
      end
      result
    end
  end
  Engine.prepend(QueryComposition)
end
