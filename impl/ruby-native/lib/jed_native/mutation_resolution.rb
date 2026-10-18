# frozen_string_literal: true

module JedNative
  # Assignment admission is static even for an empty source. Only an untouched
  # bare literal in a plain SELECT projection inherits the destination's type.
  module MutationResolution
    def insert(statement)
      raise Error.new('42803', 'aggregate in INSERT VALUES') if local_aggregate?(statement[:rows])
      query = statement[:query]
      return super unless query
      view = @views[statement[:table]]
      stored = view ? { name: statement[:table], columns: view_schema(view) } : table(statement[:table])
      targets = statement[:columns] || stored[:columns].map { |column| column[:name] }
      indices = mutation_target_indices(stored, targets)
      metadata = query_metadata(query)[:columns]
      raise Error.new('42601', 'INSERT column count mismatch') unless metadata.length == indices.length
      projections = query[:kind] == :select ? query[:columns] : nil
      changed = false
      projections = projections&.each_with_index&.map do |projection, index|
        node = projection[:expr]
        next projection unless index < indices.length && projection_string_adapts?(query, projection, index)
        target = stored[:columns][indices[index]][:type]
        if target == 'date' && DateClock::OFFSETS.key?(node[:value].strip.downcase)
          raise Error.new('22007', 'clock-relative date data requires an explicit cast')
        end
        expression = Expression.new(kind: :literal, type: 'text', value: node[:value], extra: node)
        next projection unless context_assignable?(expression, target)
        value = cast_value(node[:value], target, source_type: 'text')
        metadata[index] = metadata[index].merge(type: target)
        changed = true
        projection.merge(expr: { kind: :literal, type: target, value: value })
      end
      metadata.zip(indices).each do |column, ordinal|
        expression = Expression.new(kind: :source, type: column[:type])
        unless %w[unknown null].include?(expression.type) || context_assignable?(expression, stored[:columns][ordinal][:type])
          raise Error.new('42804', 'INSERT source has incompatible type')
        end
      end
      statement = statement.merge(query: query.merge(columns: projections)) if changed
      super(statement)
    end

    def projection_string_adapts?(query, projection, ordinal)
      node = projection[:expr]
      return false unless node[:kind] == :literal && node[:untyped] && node[:value].is_a?(String)
      return false if query[:distinct]
      keys = (query[:group_by] || []) + (query[:order_by] || []).map { |order| order[:expr] }
      keys.none? do |key|
        key[:kind] == :literal && key[:value] == ordinal + 1 ||
          key[:kind] == :column && !key[:table] && key[:name] == projection[:alias]
      end
    end
  end
  Engine.prepend(MutationResolution)
end
