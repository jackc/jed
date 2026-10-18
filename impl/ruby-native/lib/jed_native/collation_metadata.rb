# frozen_string_literal: true

module JedNative
  # Collation is resolved alongside each output column, including an empty
  # relation. Runtime string wrappers merely preserve those facts in row lanes.
  module CollationMetadata
    def query_metadata(statement, outer = [])
      metadata = super
      return metadata unless @environment && !@environment.collations.empty?
      metadata.merge(columns: view_query_columns(statement, nil, outer))
    end

    def view_query_columns(query, ctes = nil, outer = [])
      return super unless @environment && !@environment.collations.empty?
      ctes = (ctes || @ctes.transform_values(&:columns)).dup
      (query[:with] || []).each do |cte|
        body = cte[:query]
        if query[:recursive] && cte_reference_count(body, cte[:name]).positive? && %i[set_operation setop].include?(body[:kind])
          seed = rename_query_columns(view_query_columns(body[:left], ctes, outer), cte[:columns])
          additions = cte_search_cycle_columns(cte, seed)
          ctes[cte[:name]] = seed + additions.map { |column| column.merge(star_hidden: true) }
          recursive = view_query_columns(body[:right], ctes, outer)
          seed.zip(recursive).each do |left, right|
            a, b = left[:collation_label], right && right[:collation_label]
            raise Error.new('42P21', 'recursive term changes the anchor collation') if a&.name != b&.name || a&.conflict != b&.conflict
          end
          ctes[cte[:name]] = seed + additions
        else
          ctes[cte[:name]] = rename_query_columns(view_query_columns(body, ctes, outer), cte[:columns])
        end
      end
      body = query.reject { |key, _| key == :with || key == :recursive }
      columns = super(body, ctes, outer)
      analyzer = ParameterBindings.new(self, body)
      analyzer.instance_variable_set(:@ctes, ctes)
      labels = case body[:kind]
      when :set_operation, :setop
        left, right = view_query_columns(body[:left], ctes, outer), view_query_columns(body[:right], ctes, outer)
        columns.each_index.map do |index|
          a, b = left[index][:collation_label], right[index][:collation_label]
          label = merge_query_collation(a, b)
          if label&.none && !(body[:op].to_s == 'union' && body[:all])
            raise Error.new('42P21', 'set operation has incompatible collations')
          end
          label
        end
      when :values
        columns.each_index.map do |index|
          body[:rows].map { |row| query_expression_collation(row[index], outer, ctes, analyzer) }.reduce(nil) { |left, right| merge_query_collation(left, right) }
        end
      when :select, :insert, :update, :delete
        scope = query_input_columns(body, ctes, outer, analyzer)
        projections = body[:kind] == :select ? body[:columns] : body[:returning]
        projections.to_a.flat_map do |projection|
          expression = projection[:expr]
          if expression[:kind] == :star
            scope.select { |column| !column[:star_hidden] && (expression[:table] ? column[:table] == expression[:table] : !column[:qualified_only]) }.map { |column| column_collation(column) }
          elsif expression[:kind] == :field_star
            type = analyzer.send(:expression, expression[:expr], scope)
            (@types[type] || []).map { |column| column_collation(column) }
          else
            [query_expression_collation(expression, scope, ctes, analyzer)]
          end
        end
      else []
      end
      result = columns.each_with_index.map do |column, index|
        label = labels[index]
        column.merge(collation: label&.name, collation_label: label)
      end
      if (body[:order_by] || []).any?
        scope = body[:kind] == :select ? query_input_columns(body, ctes, outer, analyzer) : []
        body[:order_by].each do |order|
          expression = order[:expr]
          label = if expression[:kind] == :literal && expression[:value].is_a?(Integer)
            result[expression[:value] - 1]&.dig(:collation_label)
          elsif expression[:kind] == :column && !expression[:table] && (column = result.find { |entry| entry[:name] == expression[:name] })
            column[:collation_label]
          else
            query_expression_collation(expression, scope + result, ctes, analyzer)
          end
          raise Error.new(label.conflict ? '42P21' : '42P22', 'indeterminate collation in ORDER BY') if label&.conflict || label&.none
        end
      end
      if body[:kind] == :select
        raise Error.new('42P21', 'indeterminate collation in DISTINCT') if body[:distinct] && labels.any? { |label| label&.conflict }
        scope ||= query_input_columns(body, ctes, outer, analyzer)
        query_expression_collation(body[:where], scope, ctes, analyzer)
        query_expression_collation(body[:having], scope, ctes, analyzer)
        (body[:joins] || []).each { |join| query_expression_collation(join[:on], scope, ctes, analyzer) }
        validate_window_collations(body, scope, ctes, analyzer)
        (body[:group_by] || []).each do |key|
          label = query_expression_collation(key, scope, ctes, analyzer)
          raise Error.new(label.conflict ? '42P21' : '42P22', 'indeterminate grouping collation') if label&.conflict || label&.none
        end
      end
      result
    end

    def column_collation(column)
      column[:collation_label] || (collation_label(column[:collation] || 'C') if collatable_type?(column[:type]))
    end

    def merge_query_collation(left, right)
      label = CollationRules.combine([left, right])
      label&.conflict ? CollationLabel.new(none: true).freeze : label
    end

    def query_expression_collation(node, scope, ctes, analyzer)
      return nil unless node
      case node[:kind]
      when :literal
        collation_label(default_collation, default: true) if node[:value].is_a?(String)
      when :column
        column = scope.find { |entry| entry[:name] == node[:name] && (node[:table] ? entry[:table] == node[:table] : !entry[:qualified_only]) }
        column && column_collation(column)
      when :collate then collation_label(node[:name], explicit: true)
      when :cast
        return nil unless collatable_type?(Types.normalize(node[:type]))
        label = query_expression_collation(node[:expr], scope, ctes, analyzer)
        label = nil if label&.none && node[:type_mod]
        label || collation_label(default_collation, default: true)
      when :subquery, :array_query
        view_query_columns(node[:query], ctes, scope).first&.dig(:collation_label)
      when :field
        type = analyzer.send(:expression, node[:expr], scope)
        column = @types[type]&.find { |field| field[:name] == node[:name] }
        column && column_collation(column)
      when :unary
        query_expression_collation(node[:expr] || node[:operand], scope, ctes, analyzer)
        nil
      when :in, :between, :quantified
        children = [node[:expr] || node[:left], node[:right], node[:low], node[:high]] + (node[:values] || [])
        labels = children.compact.map { |child| query_expression_collation(child, scope, ctes, analyzer) }
        labels << view_query_columns(node[:query], ctes, scope).first&.dig(:collation_label) if node[:query]
        label = CollationRules.combine(labels)
        raise Error.new('42P22', 'indeterminate collation') if label&.conflict || label&.none
        nil
      when :case, :call, :binary, :array, :subscript
        children = case node[:kind]
        when :case then node[:branches].map { |branch| branch.is_a?(Array) ? branch.last : branch[:then] } + [node[:else]]
        when :call then node[:within_group] ? node[:within_group].map { |order| order[:expr] } : node[:args] || []
        when :binary then [node[:left], node[:right]]
        when :array then node[:elements] || node[:args] || []
        else [node[:expr]]
        end
        labels = children.map { |child| query_expression_collation(child, scope, ctes, analyzer) }
        label = CollationRules.combine(labels)
        type = analyzer.send(:expression, node, scope)
        if node[:kind] == :call && %w[width_bucket min max greatest least nullif].include?(node[:name]) && (label&.conflict || label&.none)
          raise Error.new('42P22', 'indeterminate collation')
        end
        if collatable_type?(type)
          return label
        end
        raise Error.new('42P22', 'indeterminate collation') if label&.conflict && (node[:kind] == :binary || %w[width_bucket min max].include?(node[:name]))
        nil
      end
    end

    def validate_window_collations(node, scope, ctes, analyzer)
      case node
      when Array then node.each { |item| validate_window_collations(item, scope, ctes, analyzer) }
      when Hash
        if node.key?(:partition_by)
          (node[:partition_by] + (node[:order_by] || []).map { |item| item[:expr] }).each do |key|
            label = query_expression_collation(key, scope, ctes, analyzer)
            raise Error.new(label.conflict ? '42P21' : '42P22', 'indeterminate window collation') if label&.conflict || label&.none
          end
        end
        node.each_value { |item| validate_window_collations(item, scope, ctes, analyzer) if item.is_a?(Hash) || item.is_a?(Array) }
      end
    end

    def query_source_columns(source, ctes, outer, analyzer)
      columns = super
      if source[:kind] == :function && source.dig(:expr, :name) == 'unnest'
        label = query_expression_collation(source[:expr][:args].first, outer, ctes, analyzer)
        return columns.map { |column| column.merge(collation: label&.name, collation_label: label) }
      end
      return columns unless %i[subquery table].include?(source[:kind]) && (source[:kind] == :subquery || ctes.key?(source[:name]) || @views.key?(source[:name]))
      columns.map do |column|
        label = column_collation(column)
        if label
          label = label.conflict ? CollationLabel.new(none: true).freeze : CollationLabel.new(**label.to_h.merge(explicit: false)).freeze
        end
        column.merge(collation: label&.name, collation_label: label)
      end
    end

    def source_relation(source)
      relation = super
      if source[:kind] == :function && source.dig(:expr, :name) == 'unnest' && @environment && !@environment.collations.empty?
        analyzer = ParameterBindings.new(self, source)
        columns = query_source_columns(source, @ctes.transform_values(&:columns), @outer.flat_map(&:first), analyzer)
        return decorate_collation_relation(relation, columns)
      end
      return relation unless @environment && !@environment.collations.empty? && (source[:kind] == :subquery || @ctes.key?(source[:name]) || @views.key?(source[:name]))
      columns = relation.columns.map do |column|
        label = column_collation(column)
        if label
          label = label.conflict ? CollationLabel.new(none: true).freeze : CollationLabel.new(**label.to_h.merge(explicit: false)).freeze
        end
        column.merge(collation: label&.name, collation_label: label)
      end
      decorate_collation_relation(relation, columns)
    end

    def select(statement)
      return super unless @environment && !@environment.collations.empty?
      columns = view_query_columns(statement, nil, @outer.flat_map(&:first))
      relation = super
      decorate_collation_relation(relation, columns)
    end

    def apply_query_tail(statement, relation, set_operation:)
      if @environment && !@environment.collations.empty?
        columns = view_query_columns(statement, nil, @outer.flat_map(&:first))
        relation = decorate_collation_relation(relation, columns)
      end
      super
    end

    def decorate_collation_relation(relation, columns)
      return relation unless relation.columns.length == columns.length
      schema = relation.columns.each_with_index.map { |column, index| column.merge(collation: columns[index][:collation], collation_label: columns[index][:collation_label]) }
      rows = relation.rows.map { |row| row.each_with_index.map { |value, index| CollationRules.wrap(value, columns[index][:collation_label]) } }
      Relation.new(columns: schema, rows: rows)
    end
  end
  Engine.prepend(CollationMetadata)
end
