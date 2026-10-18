# frozen_string_literal: true

require_relative "storage"
require_relative "sql_render"

module JedNative
  # Converts idiomatic engine values/catalogs to the language-neutral file
  # representation. The file never contains Ruby object dumps or Ruby source.
  module Persistence
    FOREIGN_ACTIONS = %i[no_action restrict cascade set_null set_default].freeze

    def attach_storage(storage = nil, **options)
      storage ||= Storage.new(**options)
      @storage = storage
      import_storage(storage.load)
      @persisted_snapshot = storage_signature(export_storage)
      self
    rescue StandardError
      storage&.close
      @storage = nil
      raise
    end

    def persist
      if @storage
        state = export_storage
        signature = storage_signature(state)
        unless signature == @persisted_snapshot
          @storage.commit(state)
          @persisted_snapshot = signature
        end
      end
      @dirty = false
    end

    def to_image
      state = export_storage
      codec = FileFormat.new
      codec.build_image(state[:types], state[:sequences], state[:tables],
                        @storage&.page_size || 8192, state[:collations], views: state[:views])
    end

    def close
      @storage&.close
    ensure
      super
    end

    def export_storage
      {
        tables: @tables.values.reject { |table| table[:temporary] }.map { |table| export_table(table) },
        types: @types.map { |name, fields| { name: name, fields: fields.map { |field| export_column(field, composite: true) } } },
        sequences: @sequences.reject { |_, sequence| sequence[:temporary] || @tables[sequence[:owned_by]&.first]&.dig(:temporary) }.map do |name, sequence|
          { name: name, increment: sequence[:increment], min_value: sequence[:min_value] || 1,
            max_value: sequence[:max_value] || ((1 << 63) - 1), start: sequence[:start] || 1,
            cache: sequence[:cache] || 1, cycle: !!sequence[:cycle], last_value: sequence[:value],
            is_called: sequence[:called], owned_by: sequence[:owned_by] }
        end,
        collations: [],
        views: @views.reject { |_, view| view[:temporary] }.map { |name, view| export_view(name, view) }
      }
    end

    private

    def storage_signature(value)
      case value
      when Hash
        value.reject { |key, _| %i[evaluate where].include?(key) }.transform_values { |item| storage_signature(item) }
      when Array then value.map { |item| storage_signature(item) }
      else value
      end
    end

    def export_column(column, composite: false)
      type = column[:type]
      modifier = column[:type_mod] || column[:typmod]
      modifier = { precision: modifier[0], scale: modifier[1] } if modifier.is_a?(Array)
      result = { name: column[:name], type: type, pk: !!column[:primary_key],
                 not_null: column[:nullable] == false, precision: nil, scale: nil, varchar_len: nil,
                 default: :none, default_expr: nil, identity: nil, collation: nil, temporal: nil }
      if type == 'decimal'
        result[:precision], result[:scale] = modifier&.values_at(:precision, :scale)
      elsif type == 'text'
        result[:varchar_len] = modifier&.dig(:precision)
      elsif %w[timestamp timestamptz interval].include?(type) && modifier
        result[:temporal] = { precision: modifier[:precision], fields: modifier[:fields] || 0 }
      end
      if column[:collation] && column[:collation] != 'C'
        raise Error.new('0A000', 'persisting non-C collations is not implemented in the Ruby experiment')
      end
      if column[:default]
        if type.end_with?('[]') || FileFormat::RANGE_ELEM.key?(type)
          raise Error.new('0A000', 'container column defaults have no supported file representation')
        end
        result[:default_expr] = catalog_expression(column[:default])
      end
      identity = column[:identity]
      result[:identity] = identity.is_a?(Hash) ? identity[:mode] : identity if identity
      result.delete(:pk) if composite
      result
    end

    def export_table(table)
      columns = table[:columns].map { |column| export_column(column) }
      pk = table[:constraints].find { |constraint| constraint[:kind] == :primary_key }
      pk_order = pk ? pk[:columns].map { |name| column_ordinal(table, name) } : []
      checks, indexes, fks, exclusions = [], table[:indexes].map { |index| export_index(table, index) }, [], []
      table[:constraints].each_with_index do |constraint, position|
        case constraint[:kind]
        when :check
          name = constraint[:name] || "#{table[:name]}_check#{position.zero? ? '' : position}"
          checks << { name: name, expr: catalog_expression(constraint[:expr]), host_deps: [] }
        when :unique
          name = constraint[:name] || "#{table[:name]}_#{constraint[:columns].join('_')}_key"
          unless indexes.any? { |index| index[:name] == name }
            indexes << { name: name, cols: constraint[:columns].map { |column| column_ordinal(table, column) }, unique: true, kind: 'btree' }
          end
        when :foreign_key
          parent = table(constraint[:table] || constraint[:ref_table])
          remote = constraint[:ref_columns] || constraint[:reference_columns]
          remote ||= parent[:constraints].find { |entry| entry[:kind] == :primary_key }&.dig(:columns)
          raise Error.new('42830', 'foreign key has no referenced key') unless remote

          actions = FOREIGN_ACTIONS.index(constraint[:on_delete] || :no_action) || 0
          actions |= (FOREIGN_ACTIONS.index(constraint[:on_update] || :no_action) || 0) << 3
          actions |= 64 if constraint[:match] == :full
          fks << { name: constraint[:name] || "#{table[:name]}_#{constraint[:columns].join('_')}_fkey",
                   local: constraint[:columns].map { |column| column_ordinal(table, column) },
                   ref_table: parent[:name], ref: remote.map { |column| column_ordinal(parent, column) }, actions: actions }
        when :exclude
          elements = constraint[:elements] || []
          unless elements.all? { |element| element[:expr][:kind] == :column && %w[= &&].include?(element[:op]) }
            raise Error.new('0A000', 'file exclusion keys require columns and supported operator classes')
          end
          name = constraint[:name] || "#{table[:name]}_#{elements.map { |element| element[:expr][:name] }.join('_')}_excl"
          unless indexes.any? { |index| index[:name] == name }
            indexes << export_index(table, { name: name, method: 'gist', elements: elements, where: constraint[:where] })
          end
          exclusions << { name: name, index: name, elements: elements.map { |element| [column_ordinal(table, element[:expr][:name]), element[:op]] } }
        end
      end
      rows = table[:rows].map do |row|
        row.each_with_index.map { |value, index| storage_value(columns[index][:type], value) }
      end
      result = { name: table[:name], columns: columns, rows: rows, pk_order: pk_order,
                 checks: checks.sort_by { |check| check[:name].b }, indexes: indexes.sort_by { |index| index[:name].b },
                 fks: fks.sort_by { |fk| fk[:name].b }, exclusions: exclusions.sort_by { |exclusion| exclusion[:name].b } }
      if pk_order.empty? && table[:rowids]
        codec = FileFormat.new
        result[:storage_keys] = table[:rowids].map { |rowid| codec.encode_int(8, rowid) }
      end
      if table[:storage_statistics]
        stale = table[:rows] != table[:storage_rows]
        result[:statistics] = table[:storage_statistics].map { |item| stale ? item.merge(stale: true) : item }
      end
      result
    end

    def column_ordinal(table, name)
      table[:columns].index { |column| column[:name] == name } || raise(Error.new('42703', "unknown column #{name}"))
    end

    def export_index(table, index)
      elements = index[:elements] || (index[:columns] || []).map { |name| { expr: { kind: :column, name: name } } }
      scope = table_columns(table)
      cols = elements.map do |element|
        node = element[:expr]
        if node[:kind] == :column
          column_ordinal(table, node[:name])
        else
          reject_stateful_index(node)
          resolved = compile(node, scope)
          { expr: catalog_expression(node), type: resolved.type,
            evaluate: ->(row) { unmetered_storage_evaluate(resolved, table, row, resolved.type) } }
        end
      end
      orders = elements.map do |element|
        desc = element[:direction] == :desc
        first = element[:nulls] ? element[:nulls] == :first : desc
        FileFormat::ORDER_SYMBOLS.key([desc, first])
      end
      result = { name: index[:name], cols: cols, unique: !!index[:unique], kind: index[:method] || 'btree', orders: orders }
      if index[:where]
        reject_stateful_index(index[:where])
        predicate = compile(index[:where], scope)
        result[:predicate] = catalog_expression(index[:where])
        result[:where] = ->(row) { unmetered_storage_evaluate(predicate, table, row, 'boolean') == true }
      end
      result
    end

    def reject_stateful_index(node)
      return unless node.is_a?(Hash)

      if node[:kind] == :call && %w[nextval currval setval lastval now clock_timestamp current_date current_time localtimestamp uuidv4 uuidv7 random].include?(node[:name])
        raise Error.new('42P17', 'index expressions must be immutable')
      end
      node.each_value do |value|
        value.is_a?(Array) ? value.each { |item| reject_stateful_index(item) } : reject_stateful_index(value)
      end
    end

    def unmetered_storage_evaluate(expression, table, stored_row, type)
      cost, scalar_bytes = @cost, @scalar_bytes
      row = stored_row.each_with_index.map { |value, i| from_storage_value(table[:columns][i][:type], value) }
      storage_value(type, evaluate(expression, row))
    ensure
      @cost, @scalar_bytes = cost, scalar_bytes
    end

    def storage_value(type, value)
      return nil if value.nil?
      if @types.key?(type)
        return @types[type].each_with_index.map { |field, i| storage_value(field[:type], value[i]) }
      end
      Types.storage_value(type, value)
    end

    def from_storage_value(type, value)
      return nil if value.nil?
      if @types.key?(type)
        return @types[type].each_with_index.map { |field, i| from_storage_value(field[:type], value[i]) }
      end
      Types.from_storage_value(type, value)
    end

    def import_storage(state)
      if !state[:collations].to_a.empty?
        raise Error.new('0A000', 'non-C collation catalogs are not implemented in the Ruby experiment')
      end
      @types = state[:types].to_h do |type|
        [type[:name], type[:fields].map { |field| import_column(field) }]
      end
      @tables = state[:tables].to_h do |stored|
        columns = stored[:columns].map { |column| import_column(column) }
        constraints = []
        unless stored[:pk].empty?
          constraints << { kind: :primary_key, columns: stored[:pk].map { |index| columns[index][:name] } }
        end
        stored[:checks].each do |check|
          raise Error.new('XX002', 'CHECK requires unavailable host functions') unless check[:host_deps].to_a.empty?

          constraints << { kind: :check, name: check[:name], expr: Parser.new(check[:expr]).parse_expression }
        end
        stored[:fks].each do |fk|
          parent = state[:tables].find { |candidate| candidate[:name] == fk[:ref_table] }
          constraints << { kind: :foreign_key, name: fk[:name], columns: fk[:local].map { |i| columns[i][:name] },
                           table: fk[:ref_table], ref_columns: fk[:ref].map { |i| parent[:columns][i][:name] },
                           on_delete: FOREIGN_ACTIONS[fk[:actions] & 7], on_update: FOREIGN_ACTIONS[(fk[:actions] >> 3) & 7],
                           match: fk[:actions] & 64 == 64 ? :full : :simple }
        end
        indexes = stored[:indexes].map { |index| import_index(stored, index) }
        stored[:exclusions].each do |exclusion|
          backing = indexes.find { |index| index[:name] == exclusion[:index] }
          raise Error.new('XX001', 'exclusion constraint has no backing index') unless backing
          constraints << { kind: :exclude, name: exclusion[:name], method: 'gist', where: backing[:where],
                           elements: exclusion[:elements].map { |ordinal, operator| { expr: { kind: :column, name: columns[ordinal][:name] }, op: operator } } }
        end
        indexes.each do |index|
          next unless index[:unique] && !index[:where] && index[:elements].all? { |element| element[:expr][:kind] == :column }

          constraints << { kind: :unique, name: index[:name], columns: index[:columns] }
        end
        rows = stored[:rows].map { |row| row.each_with_index.map { |value, i| from_storage_value(columns[i][:type], value) } }
        rowids = if stored[:pk].empty?
                   codec = FileFormat.new
                   stored[:storage_keys].map { |key| codec.decode_int(8, key) }
                 else
                   (0...rows.length).to_a
                 end
        [stored[:name], { name: stored[:name], columns: columns, constraints: constraints, indexes: indexes,
                         rows: rows, rowids: rowids, next_rowid: (rowids.max || -1) + 1, temporary: false,
                         storage_rows: rows, storage_statistics: stored[:statistics] }]
      end
      @sequences = state[:sequences].to_h do |sequence|
        [sequence[:name], sequence.merge(value: sequence[:last_value], called: sequence[:is_called])]
      end
      @views = state[:views].to_h do |view|
        query = Parser.new(view[:definition]).parse.first
        dependencies = view[:deps].select { |dependency| dependency[:kind] < 2 }.to_h { |dependency| [dependency[:name], dependency[:columns]] }
        [view[:name], { kind: :create_view, name: view[:name], columns: view[:columns], query: query,
                       storage_view: view, storage_query: Marshal.load(Marshal.dump(query)),
                       dependencies: dependencies,
                       sequence_dependencies: view[:deps].select { |dependency| dependency[:kind] == 2 }.map { |dependency| dependency[:name] },
                       type_dependencies: view[:deps].select { |dependency| dependency[:kind] == 3 }.map { |dependency| dependency[:name] },
                       defaults: (view[:defaults] || {}).transform_values { |sql| Parser.new(sql).parse_expression },
                       check_option: [nil, :local, :cascaded][view[:check_option] || 0],
                       options: { 'security_barrier' => !!view[:security_barrier], 'security_invoker' => !!view[:security_invoker] } }]
      end
      refresh_primary_indexes
    end

    def import_column(column)
      modifier = if column[:precision]
                   { precision: column[:precision], scale: column[:scale] }
                 elsif column[:varchar_len]
                   { precision: column[:varchar_len] }
                 elsif column[:temporal]
                   column[:temporal]
                 end
      default = if column[:default_expr]
                  Parser.new(column[:default_expr]).parse_expression
                elsif column.key?(:default) && column[:default] != :none
                  { kind: :literal, type: column[:type], value: from_storage_value(column[:type], column[:default]) }
                end
      { name: column[:name], type: column[:type], nullable: !column[:not_null], primary_key: !!column[:pk],
        type_mod: modifier, default: default, identity: column[:identity], collation: column[:collation], checks: [] }
    end

    def import_index(table, index)
      raise Error.new('XX002', 'index requires unavailable host functions') unless index[:host_deps].to_a.empty?
      raise Error.new('XX002', 'index requires unavailable pinned timezone data') if index[:timezone_deps]

      elements = index[:cols].each_with_index.map do |column, position|
        expression = column.is_a?(Hash) ? Parser.new(column[:expr]).parse_expression : { kind: :column, name: table[:columns][column][:name] }
        desc, first = FileFormat::ORDER_SYMBOLS.fetch(index[:orders]&.at(position) || :asc)
        { expr: expression, direction: desc ? :desc : :asc, nulls: first ? :first : :last }
      end
      { kind: :create_index, name: index[:name], table: table[:name], method: index[:kind], unique: index[:unique],
        elements: elements, columns: elements.map { |element| element[:expr][:name] },
        where: index[:predicate] ? Parser.new(index[:predicate]).parse_expression : nil }
    end

    def export_view(name, view)
      columns = view[:columns] || view_schema(view).map { |column| column[:name] }
      options = view[:options] || {}
      dependencies = view_dependencies(view).map do |relation, columns|
        { name: relation, kind: @views.key?(relation) ? 1 : 0,
          columns: @views.key?(relation) ? [] : columns.sort_by(&:b) }
      end
      if view[:sequence_dependencies] && view[:type_dependencies]
        sequence_names, type_names = view.values_at(:sequence_dependencies, :type_dependencies)
      else
        sequence_names, type_names = view_catalog_dependencies(view[:query], view_schema(view))
      end
      sequence_names.each { |sequence| dependencies << { name: sequence, kind: 2, columns: [] } }
      type_names.each { |type| dependencies << { name: type, kind: 3, columns: [] } }
      defaults = (view[:defaults] || {}).transform_values { |node| catalog_expression(node) }
      { name: name, columns: columns, definition: catalog_view_definition(view),
        deps: dependencies.sort_by { |dependency| [dependency[:name].b, dependency[:kind]] },
        check_option: { local: 1, cascaded: 2 }.fetch(view[:check_option], 0), defaults: defaults,
        security_barrier: !!options['security_barrier'], security_invoker: !!options['security_invoker'] }
    end
  end

  Engine.prepend(Persistence)
end
