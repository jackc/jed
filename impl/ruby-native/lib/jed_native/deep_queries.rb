# frozen_string_literal: true

module JedNative
  # Query shape is immutable within a catalog version. Reuse it across the
  # pushdown passes, and execute long single-input chains with an explicit stack.
  module DeepQueries
    def run(statement, params = [])
      @native_query_shape_cache = {}.compare_by_identity
      super
    ensure
      @native_query_shape_cache = nil
    end

    def view_query_columns(query, ctes = nil, outer = [])
      depth = @native_query_metadata_depth || 0
      if depth >= 16
        @native_query_metadata_depth = 0
        return Fiber.new { view_query_columns(query, ctes, outer) }.resume
      end
      @native_query_metadata_depth = depth + 1
      return super unless @native_query_shape_cache
      scope = [ctes || @ctes.transform_values(&:columns), outer]
      cached = (@native_query_shape_cache[query] ||= {})
      return cached[scope].map(&:dup) if cached.key?(scope)
      result = super
      cached[scope] = result.map(&:dup)
      result
    ensure
      @native_query_metadata_depth = depth
    end

    def select(statement)
      return super if @native_linear_execution || @explain_runtime_plans || !deep_linear_query?(statement)
      @native_linear_execution = true
      stack = []
      current = statement
      while (body = linear_query_body(current))
        rewritten = rewrite_join_inputs(current[:from], current[:joins] || [], current[:where])
        if rewritten
          sources, joins, predicate = rewritten
          stage = current.merge(from: sources, joins: joins, where: predicate)
          body = linear_query_body(stage)
        else
          stage = current
        end
        break unless body
        stack << stage
        current = body
      end
      relation = select(current)
      stack.reverse_each do |stage|
        source = stage[:from].first.merge(native_linear_relation: relation)
        relation = select(stage.merge(from: [source]))
      end
      relation
    ensure
      @native_linear_execution = false if stack
    end

    def source_relation(source)
      return super unless (relation = source[:native_linear_relation])
      label = source[:alias] || source[:name]
      names = source[:columns] || []
      raise Error.new('42601', 'too many relation column aliases') if names.length > relation.columns.length
      columns = relation.columns.each_with_index.map { |column, index| column.merge(name: names[index] || column[:name], table: label, source_ordinal: index) }
      result = Relation.new(columns: columns, rows: relation.rows)
      query_memory_transfer(relation, result) if @query_memory
      result
    end

    private

    def linear_query_body(query)
      return nil unless query[:kind] == :select && !query[:with] && (query[:from] || []).length == 1 && (query[:joins] || []).empty?
      source = query[:from].first
      return nil if source[:lateral] || source[:native_linear_relation]
      if source[:kind] == :subquery
        source[:query]
      elsif source[:kind] == :table && !@ctes.key?(source[:name]) && @views.key?(source[:name])
        source[:native_body_query] || @views[source[:name]][:query]
      end
    end

    def deep_linear_query?(statement)
      current = statement
      16.times do
        current = linear_query_body(current)
        return false unless current
      end
      true
    end
  end
  Engine.prepend(DeepQueries)
end
