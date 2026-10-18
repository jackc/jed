# frozen_string_literal: true

module JedNative
  module CreateTableAs
    def create_table(statement)
      return super unless statement[:query]
      query = statement[:query]
      columns = view_query_columns(query)
      names = statement[:column_names] || []
      if columns.any? { |column| (label = column[:collation_label]) && (label.none || label.conflict) }
        raise Error.new('42P22', 'table column needs a determinate collation')
      end
      raise Error.new('42601', 'too many table column names') if names.length > columns.length
      name = statement[:name]
      return Result.new if statement[:if_not_exists] && relation_exists?(name)
      raise Error.new('42P07', 'relation already exists') if relation_exists?(name)
      raise Error.new('42P16', 'ON COMMIT requires a temporary table') if statement[:on_commit] && !statement[:temporary]
      columns = columns.each_with_index.map do |column, index|
        { name: names[index] || column[:name], type: column[:type] == 'unknown' ? 'text' : Types.normalize(column[:type]),
          type_mod: column[:type_mod], collation: column[:collation], nullable: true }
      end
      raise Error.new('42701', 'duplicate column') unless columns.map { |column| column[:name] }.uniq.length == columns.length
      raise Error.new('42P16', 'anonymous record columns cannot be stored') if columns.any? { |column| %w[record record[]].include?(column[:type]) }
      relation = select(query) unless statement[:with_no_data]
      @tables = @cte_working_tables if @cte_working_tables
      super(statement.merge(query: nil, columns: columns))
      if relation
        rows = relation.rows
        @tables[name] = @tables.fetch(name).merge(rows: rows, rowids: (1..rows.length).to_a, next_rowid: rows.length + 1)
      end
      @cte_working_tables = @tables if @cte_working_tables
      Result.new(rows_affected: relation ? relation.rows.length : 0)
    end

    def expression_name(node)
      node[:kind] == :field ? node[:name] : super
    end

    def expand_stars(projections, columns)
      projections = projections.flat_map do |item|
        expression = item[:expr]
        next item unless expression[:kind] == :field_star
        parent = expression[:expr]
        type = compile(parent, columns).type
        fields = @types[type] || raise(Error.new('42809', 'star requires a composite value'))
        fields.map { |field| { expr: { kind: :field, expr: parent, name: field[:name] }, alias: field[:name] } }
      end
      super(projections, columns)
    end

    def dispatch(statement)
      if statement[:kind] == :create_type
        statement = statement.merge(fields: statement[:fields].map { |field| field.merge(type: Types.normalize(field[:type])) })
      end
      super
    end
  end

  Engine.prepend(CreateTableAs)
end
