# frozen_string_literal: true

module JedNative
  module JoinEdges
    def estimate_predicate(rows, expression)
      if expression&.kind == :binary && %w[not\ like not\ ilike !~ !~*].include?(expression.value)
        return rows - (rows + 199) / 200
      end
      super
    end

    def materialize_from(sources, joins, predicate = nil)
      previous = @join_edge_width
      @join_edge_width = sources.length + joins.length
      if (normalized = normalize_using_joins(sources, joins, predicate))
        edges, where, layout = normalized
        relation, remaining = super(sources, edges, where)
        rows = relation.rows.map do |row|
          layout.map { |column| cast_value(row[column[:join_origin]], column[:type]) }
        end
        columns = layout.map { |column| column.reject { |key, _| %i[join_origin join_source].include?(key) } }
        result = Relation.new(columns: columns, rows: rows)
        query_memory_transfer(relation, result) if @query_memory
        return [result, remaining]
      end
      if sources.first&.dig(:kind) == :join_group && !sources.first[:alias]
        sources = [sources.first.merge(native_inline_group: true)] + sources.drop(1)
      end
      if sources.length > 1 && !joins.empty? && joins.all? { |edge| %i[inner cross].include?(edge[:kind]) } &&
          joins.none? { |edge| edge[:using] || edge[:natural] } &&
          (sources + joins.map { |edge| edge[:relation] }).all? { |source| source[:kind] == :table && !source[:lateral] }
        flattened = []
        analyzer = ParameterBindings.new(self, nil)
        sources.each_with_index do |source, index|
          local = analyzer.relation_columns(source)
          joins.select { |edge| (edge[:from_index] || 0) == index }.each do |edge|
            local += analyzer.relation_columns(edge[:relation])
            compile_filter(edge[:on], local) if edge[:on]
          end
          flattened << { kind: :cross, relation: source, from_index: 0 } if index.positive?
          flattened.concat(joins.select { |edge| (edge[:from_index] || 0) == index }.map { |edge| edge.merge(from_index: 0) })
        end
        sources, joins = [sources.first], flattened
      end
      super(sources, joins, predicate)
    ensure
      @join_edge_width = previous
    end

    def join(left, source, kind, on, using)
      if @join_edge_width == 2 && !source[:native_join_algorithm] && source[:kind] == :join_group
        source = source.merge(native_fixed_hash: true)
      end
      super
    end

    def source_relation(source)
      if source[:kind] == :join_group
        previous = @native_scan_statement
        @native_scan_statement = source[:native_group_statement] || { kind: :select, from: source[:from], joins: source[:joins], columns: [{ expr: { kind: :star } }] }
        child = source[:native_scan_profile]&.children&.first
        plan_nodes(child).each { |node| node.actual = 0 } if child
        started = @cost
      end
      result = super
      child.actual = @cost - started if child
      result
    ensure
      @native_scan_statement = previous if source[:kind] == :join_group
    end

    def normalize_using_joins(sources, joins, predicate)
      return unless sources.length == 1 && joins.any? { |edge| edge[:using] || edge[:natural] }
      return unless joins.all? { |edge| %i[inner cross].include?(edge[:kind]) }
      inputs = sources + joins.map { |edge| edge[:relation] }
      return unless inputs.all? { |source| source[:kind] == :table && !source[:lateral] && @tables.key?(source[:name]) && !@ctes.key?(source[:name]) }
      analyzer = ParameterBindings.new(self, nil)
      offset = 0
      scopes = inputs.map do |source|
        analyzer.relation_columns(source).map do |column|
          result = column.merge(join_origin: offset, join_source: column)
          offset += 1
          result
        end
      end
      layout = scopes.first
      edges = joins.each_with_index.map do |edge, index|
        right = scopes[index + 1]
        names = edge[:using] || (edge[:natural] ? layout.reject { |column| column[:qualified_only] }.map { |column| column[:name] } & right.map { |column| column[:name] } : [])
        pairs = names.map do |name|
          left_matches = layout.each_index.select { |i| layout[i][:name] == name && !layout[i][:qualified_only] }
          right_matches = right.each_index.select { |i| right[i][:name] == name && !right[i][:qualified_only] }
          raise Error.new('42702', "ambiguous USING column #{name}") if left_matches.length > 1 || right_matches.length > 1
          raise Error.new('42703', "unknown USING column #{name}") unless left_matches.length == 1 && right_matches.length == 1
          [left_matches.first, right_matches.first]
        end
        conditions = pairs.map do |li, ri|
          { kind: :binary, op: '=', left: using_origin_node(layout[li]), right: using_origin_node(right[ri]) }
        end
        conditions.unshift(rewrite_using_references(edge[:on], layout + right)) if edge[:on]
        unless pairs.empty?
          merged = pairs.map { |li, ri| layout[li].merge(table: nil, type: common_type(layout[li][:type], right[ri][:type])) }
          li, ri = pairs.transpose
          visible = layout.each_index.reject { |i| li.include?(i) }.map { |i| layout[i] } + right.each_index.reject { |i| ri.include?(i) }.map { |i| right[i] }
          hidden = pairs.flat_map { |l, r| [layout[l], right[r]].map { |column| column.merge(qualified_only: true) } }
          layout = merged + visible + hidden
        else
          layout += right
        end
        edge.merge(using: nil, natural: false, on: join_conjunctions(conditions))
      end
      [edges, rewrite_using_references(predicate, layout), layout]
    end

    def using_origin_node(column)
      original = column[:join_source]
      { kind: :column, name: original[:name], table: original[:table] }
    end

    def rewrite_using_references(node, layout)
      case node
      when Hash
        if node[:kind] == :column && !node[:table] && !node[:native_ordinal]
          matches = layout.select { |column| column[:name] == node[:name] && !column[:qualified_only] }
          return using_origin_node(matches.first) if matches.length == 1
        end
        return node if %i[subquery exists array_query].include?(node[:kind])
        node.transform_values { |value| rewrite_using_references(value, layout) }
      when Array then node.map { |value| rewrite_using_references(value, layout) }
      else node
      end
    end
  end
  Engine.prepend(JoinEdges)
end
