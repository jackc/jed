# frozen_string_literal: true

module JedNative
  # Typmods are planning facts. They follow a column through query boundaries
  # without evaluating an expression, advancing a sequence, or visiting rows.
  module QueryColumns
    def view_query_columns(query, ctes = nil, outer = [])
      ctes ||= @ctes.transform_values(&:columns)
      ctes = ctes.dup
      (query[:with] || []).each do |cte|
        body = cte[:query]
        if query[:recursive] && cte_reference_count(body, cte[:name]).positive? && %i[set_operation setop].include?(body[:kind])
          seed = rename_query_columns(view_query_columns(body[:left], ctes, outer), cte[:columns])
          additions = cte_search_cycle_columns(cte, seed)
          ctes[cte[:name]] = seed + additions.map { |column| column.merge(star_hidden: true) }
          recursive = view_query_columns(body[:right], ctes, outer)
          seed.zip(recursive).each do |left, right|
            if right && (left[:type] != common_type(left[:type], right[:type]) || left[:type_mod] != right[:type_mod])
              raise Error.new('42804', 'recursive term widens the anchor column type')
            end
          end
          ctes[cte[:name]] = seed + additions
        else
          ctes[cte[:name]] = rename_query_columns(view_query_columns(body, ctes, outer), cte[:columns])
        end
      end
      body = query.reject { |key, _| key == :with || key == :recursive }
      analyzer = ParameterBindings.new(self, body)
      analyzer.instance_variable_set(:@ctes, ctes)
      columns = analyzer.query_columns(body, outer)
      case body[:kind]
      when :set_operation, :setop
        left, right = view_query_columns(body[:left], ctes, outer), view_query_columns(body[:right], ctes, outer)
        columns.each_with_index.map { |column, index| column.merge(type_mod: common_query_modifier([left[index]&.dig(:type_mod), right[index]&.dig(:type_mod)])) }
      when :values
        columns.each_with_index.map do |column, index|
          mods = body[:rows].map { |row| query_expression_modifier(row[index], outer, ctes, analyzer) }
          column.merge(type_mod: common_query_modifier(mods))
        end
      when :select, :insert, :update, :delete
        scope = query_input_columns(body, ctes, outer, analyzer)
        projections = body[:kind] == :select ? body[:columns] : body[:returning]
        return columns unless projections
        expanded = projections.flat_map do |item|
          expr = item[:expr]
          if expr[:kind] == :star
            scope.select { |column| !column[:star_hidden] && (expr[:table] ? column[:table] == expr[:table] : !column[:qualified_only]) }.map do |column|
              { expr: { kind: :column, name: column[:name], table: column[:table] }, modifier: column[:type_mod] }
            end
          elsif expr[:kind] == :field_star
            type = analyzer.send(:expression, expr[:expr], scope)
            (@types[type] || []).map { |field| { expr: { kind: :field, expr: expr[:expr], name: field[:name] }, modifier: field[:type_mod] } }
          else
            [item]
          end
        end
        columns.each_with_index.map do |column, index|
          item = expanded[index]
          modifier = item && (item.key?(:modifier) ? item[:modifier] : query_expression_modifier(item[:expr], scope, ctes, analyzer))
          column.merge(type_mod: modifier)
        end
      else columns
      end
    end

    private

    def rename_query_columns(columns, names)
      columns.each_with_index.map { |column, index| column.merge(name: names&.[](index) || column[:name]) }
    end

    def query_input_columns(query, ctes, outer, analyzer)
      if %i[insert update delete].include?(query[:kind])
        target = @views[query[:table]]
        columns = target ? view_schema(target) : table_columns(table(query[:table]))
        name = query[:alias] || query[:table]
        result = columns.map { |column| column.merge(table: name) }
        %w[old new].each do |version|
          label = query[:returning_aliases]&.[](version.to_sym) || version
          result.concat(columns.map { |column| column.merge(table: label, qualified_only: true) })
        end
        return result + outer
      end
      scope = []
      (query[:from] || []).each_with_index do |source, index|
        visible = source[:lateral] || %i[function join_group].include?(source[:kind]) ? scope + outer : outer
        scope.concat(query_source_columns(source, ctes, visible, analyzer))
        (query[:joins] || []).select { |join| (join[:from_index] || 0) == index }.each do |join|
        joined = join[:relation]
        visible = joined[:lateral] || %i[function join_group].include?(joined[:kind]) ? scope + outer : outer
        right = query_source_columns(joined, ctes, visible, analyzer)
        using = join[:using] || (join[:natural] ? scope.map { |column| column[:name] } & right.map { |column| column[:name] } : [])
        right = right.map { |column| using.include?(column[:name]) ? column.merge(qualified_only: true) : column }
        scope.concat(right)
        end
      end
      scope + outer
    end

    def query_source_columns(source, ctes, outer, analyzer)
      columns = case source[:kind]
                when :table
                  if !source[:database] && ctes.key?(source[:name])
                    ctes[source[:name]]
                  elsif @views[source[:name]]
                    view_schema(@views[source[:name]])
                  else
                    introspection_columns(source[:name], source[:database]) || table_columns(table(source[:name]))
                  end
                when :subquery then view_query_columns(source[:query], ctes, outer)
                when :join_group
                  joined = query_input_columns(source, ctes, outer, analyzer)
                  joined.take(joined.length - outer.length).reject { |column| column[:qualified_only] }
                else analyzer.relation_columns(source, outer)
                end
      label = source[:alias] || source[:name]
      columns.each_with_index.map { |column, index| column.merge(table: label || column[:table], name: source[:columns]&.[](index) || column[:name]) }
    end

    def common_query_modifier(values)
      values.empty? || values.any? { |value| value != values.first } ? nil : values.first
    end

    def query_expression_modifier(expression, scope, ctes, analyzer)
      return nil unless expression
      case expression[:kind]
      when :column
        scope.find { |column| column[:name] == expression[:name] && (expression[:table] ? column[:table] == expression[:table] : !column[:qualified_only]) }&.dig(:type_mod)
      when :cast then expression[:type_mod]
      when :collate then query_expression_modifier(expression[:expr], scope, ctes, analyzer)
      when :subquery then view_query_columns(expression[:query], ctes, scope).first&.dig(:type_mod)
      when :field
        type = analyzer.send(:expression, expression[:expr], scope)
        @types[type]&.find { |field| field[:name] == expression[:name] }&.dig(:type_mod)
      when :case
        return nil unless expression[:else]
        common_query_modifier((expression[:branches].map(&:last) + [expression[:else]]).map { |value| query_expression_modifier(value, scope, ctes, analyzer) })
      when :call
        args = expression[:args] || []
        if %w[coalesce greatest least].include?(expression[:name])
          common_query_modifier(args.map { |value| query_expression_modifier(value, scope, ctes, analyzer) })
        elsif expression[:name] == 'nullif'
          query_expression_modifier(args.first, scope, ctes, analyzer)
        end
      end
    end
  end

  Engine.prepend(QueryColumns)
end
