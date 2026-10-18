# frozen_string_literal: true

module JedNative
  # Anonymous records have structural metadata even when no value is produced.
  # Keep it beside a column's public pseudo-type rather than inventing catalog
  # type names for expression-local shapes.
  module RecordBindings
    def record_fields(node, scope)
      return nil unless node.is_a?(Hash)
      case node[:kind]
      when :row
        (node[:args] || node[:values] || []).each_with_index.map do |arg, i|
          { name: "f#{i + 1}", type: expression(arg, scope), record_fields: record_fields(arg, scope) }
        end
      when :column
        matches = scope.select { |column| column[:name] == node[:name] && (node[:table] ? column[:table] == node[:table] : !column[:qualified_only]) }
        matches.max_by { |column| column[:binding_scope] || @scope_depth }&.dig(:record_fields)
      when :array then common_record_fields((node[:args] || node[:elements] || node[:values] || []).map { |arg| record_fields(arg, scope) })
      when :subscript, :collate then record_fields(node[:expr], scope)
      when :cast then @engine.instance_variable_get(:@types)[Types.normalize(node[:type]).delete_suffix('[]')]
      when :subquery, :array_query then statement(node[:query], scope).first&.dig(:record_fields)
      when :field then record_fields(node[:expr], scope)&.find { |field| field[:name] == node[:name] }&.dig(:record_fields)
      when :call
        if %w[array_agg array_append array_prepend array_cat coalesce greatest least nullif unnest].include?(node[:name])
          common_record_fields((node[:args] || []).map { |arg| record_fields(arg, scope) })
        end
      when :binary
        common_record_fields([record_fields(node[:left], scope), record_fields(node[:right], scope)]) if node[:op] == '||'
      when :case
        common_record_fields((node[:branches].map(&:last) + [node[:else]]).map { |arg| record_fields(arg, scope) })
      end
    end

    def common_record_fields(shapes)
      shapes = shapes.compact
      return nil if shapes.empty?
      raise Error.new('42804', 'record field counts do not match') unless shapes.all? { |fields| fields.length == shapes.first.length }
      shapes.first.each_index.map do |i|
        fields = shapes.map { |shape| shape[i] }
        fields.first.merge(type: fields.map { |field| field[:type] }.reduce { |a, b| common(a, b) },
                           record_fields: common_record_fields(fields.map { |field| field[:record_fields] }))
      end
    end

    private

    def relation(node, outer)
      if node && node[:kind] == :function && node.dig(:expr, :name) == 'unnest'
        argument = node[:expr][:args].first
        type = expression(argument, outer)
        return @engine.record_unnest_columns(node, type, record_fields(argument, outer))
      end
      super
    end
  end
  ParameterBindings.prepend(RecordBindings)

  module RecordShapes
    def record_unnest_columns(source, type, fields)
      raise Error.new('42883', 'unnest requires an array') unless type.end_with?('[]')
      type = type.delete_suffix('[]')
      definitions = source[:definitions]
      if type == 'record'
        raise Error.new('42601', 'record-returning unnest requires column definitions') unless definitions
        raise Error.new('42701', 'duplicate column definition') unless definitions.map { |field| field[:name] }.uniq.length == definitions.length
        definitions.each { |field| validate_type(field[:type], field[:type_mod]) }
        raise Error.new('42804', 'record field count does not match its definition') unless fields && fields.length == definitions.length
        definitions.zip(fields).each do |declared, actual|
          raise Error.new('42804', 'record field type does not match its definition') unless record_field_compatible?(actual[:type], declared[:type])
        end
        columns = definitions
      else
        raise Error.new('42601', 'column definitions require an anonymous record') if definitions
        columns = [{ name: source[:alias] || 'unnest', type: type }]
      end
      raise Error.new('42P10', 'too many function column aliases') if (source[:columns] || []).length > columns.length
      columns.each_with_index.map { |column, i| column.merge(table: source[:alias] || 'unnest', name: source[:columns]&.[](i) || column[:name], source_ordinal: i) }
    end

    def compile(node, columns)
      raise Error.new('42883', 'unnest is only available as a table function') if node[:kind] == :call && node[:name] == 'unnest'
      if node[:kind] == :field
        value = compile(node[:expr], columns)
        fields = @types[value.type] || value.instance_variable_get(:@record_fields)
        raise Error.new('42809', 'field access requires a composite') unless fields
        index = fields.index { |field| field[:name] == node[:name] }
        raise Error.new('42703', 'unknown record field') unless index
        result = Expression.new(kind: :field, type: fields[index][:type], value: index, args: [value])
        result.instance_variable_set(:@record_fields, fields[index][:record_fields])
        return result
      end
      result = super
      if %w[record record[]].include?(result.type)
        analyzer = ParameterBindings.new(self, nil)
        fields = analyzer.record_fields(node, columns + @outer.reverse.flat_map(&:first))
        result.instance_variable_set(:@record_fields, fields)
      end
      result
    end

    def select(statement)
      result = super
      if result.columns.any? { |column| %w[record record[]].include?(column[:type]) }
        metadata = query_metadata(statement, @outer.flat_map(&:first))[:columns]
        result.columns = result.columns.each_with_index.map { |column, i| column.merge(record_fields: metadata[i]&.dig(:record_fields)) }
      end
      result
    end

    def select_values(statement, relation_only: false)
      result = super
      if result.columns.any? { |column| %w[record record[]].include?(column[:type]) }
        metadata = query_metadata(statement, @outer.flat_map(&:first))[:columns]
        result.columns = result.columns.each_with_index.map { |column, i| column.merge(record_fields: metadata[i]&.dig(:record_fields)) }
      end
      result
    end

    def expand_stars(projections, columns)
      expanded = projections.flat_map do |item|
        next item unless item.dig(:expr, :kind) == :field_star
        parent = item[:expr][:expr]
        expression = compile(parent, columns)
        fields = @types[expression.type] || expression.instance_variable_get(:@record_fields)
        raise Error.new('42809', 'star requires a composite value') unless fields
        fields.map { |field| { expr: { kind: :field, expr: parent, name: field[:name] }, alias: field[:name] } }
      end
      super(expanded, columns)
    end

    private

    def record_field_compatible?(source, target)
      return true if source == target
      return record_field_compatible?(source[0...-2], target[0...-2]) if source.end_with?('[]') && target.end_with?('[]')
      Types::INTEGER_BITS.key?(source) && Types::INTEGER_BITS.key?(target)
    end
  end

  # Install below the scan-filter wrapper: generated rows first enter that
  # wrapper, which owns pushed predicates and their execution checkpoints.
  module RecordSources
    def source_relation(source)
      return super unless source[:kind] == :function && source.dig(:expr, :name) == 'unnest'
      args = source[:expr][:args]
      raise Error.new('42883', 'unnest requires one argument') unless args.length == 1
      argument = compile(args.first, [])
      columns = record_unnest_columns(source, argument.type, argument.instance_variable_get(:@record_fields))
      values = evaluate(argument, [])
      values = values&.respond_to?(:elements) ? values.elements : values
      rows = []
      owner = query_memory_owner if @query_memory
      element = argument.type.delete_suffix('[]')
      spread = element == 'record'
      (values || []).each do |value|
        charge
        row = if spread
                value.nil? ? Array.new(columns.length) : columns.each_with_index.map { |column, i| assign(value[i], column) }
              else [value]
              end
        query_memory_add_row(owner, row) if @query_memory
        rows << row
      end
      relation = Relation.new(columns: columns, rows: rows)
      query_memory_attach(relation, owner) if @query_memory
      relation
    rescue StandardError
      query_memory_release(owner) if @query_memory && owner
      raise
    end

  end
  Engine.prepend(RecordSources)
end
