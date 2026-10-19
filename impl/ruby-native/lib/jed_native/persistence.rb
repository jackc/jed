# frozen_string_literal: true

require_relative "storage"
require_relative "sql_render"

module JedNative
  # Converts idiomatic engine values/catalogs to the language-neutral file
  # representation. The file never contains Ruby object dumps or Ruby source.
  module Persistence
    FOREIGN_ACTIONS = %i[no_action restrict cascade set_null set_default].freeze

    def primary_scan(stored, predicate)
      return nil unless stored[:collation_skew].to_a.empty?
      super
    end

    def dispatch(statement)
      if %i[insert update delete create_index alter_table].include?(statement[:kind])
        stored = @tables[statement[:table] || statement[:name]]
        if stored && !stored[:collation_skew].to_a.empty?
          raise Error.new('XX002', "table requires collation upgrade: #{stored[:collation_skew].join(', ')}")
        end
      end
      super
    end

    def attach_storage(storage = nil, **options)
      options[:collation_key] ||= ->(name, value) { resolve_collation(name).sort_key(value) }
      storage ||= Storage.new(**options)
      storage.collation_key = options[:collation_key]
      @storage = storage
      import_storage(storage.load)
      @persisted_snapshot = storage_signature(export_storage)
      @storage_publication = [storage.service_generation, storage.meta[:txid]]
      storage.service_finish_open
      self
    rescue StandardError
      storage&.close
      @storage = nil
      raise
    end

    def persist
      publications = []
      if @storage
        state = export_storage
        signature = storage_signature(state)
        publications << [nil, @storage, state, signature] unless signature == @persisted_snapshot
      end
      (@attachments || {}).each do |domain, attachment|
        next unless attachment[:storage]
        state = export_attachment_storage(domain)
        signature = storage_signature(state)
        next if signature == attachment[:storage_signature]
        raise Error.new('25006', 'attachment is read-only') if attachment[:read_only]
        publications << [domain, attachment[:storage], state, signature]
      end
      if publications.length > 1
        raise Error.new('0A000', 'a transaction may write at most one durable database')
      end
      publications.each do |domain, storage, state, signature|
        storage.commit(state)
        adopt_storage_pages(storage, domain)
        if domain
          @attachments[domain][:storage_signature] = storage_signature(export_attachment_storage(domain))
        else
          @persisted_snapshot = storage_signature(export_storage)
        end
      end
      refresh_primary_indexes unless publications.empty?
      @dirty = false
    end

    def to_image
      state = export_storage
      codec = statistics_codec
      codec.build_image(state[:types], state[:sequences], state[:tables],
                        storage_page_size, state[:collations], views: state[:views])
    end

    def close
      @storage&.close
      (@attachments || {}).each_value { |attachment| attachment[:storage]&.close }
    ensure
      super
    end

    def export_storage
      {
        tables: @tables.values.reject { |table| table[:temporary] || table[:database] }.map { |table| export_table(table) },
        types: @types.map { |name, fields| { name: name, fields: fields.map { |field| export_column(field, composite: true) } } },
        sequences: @sequences.reject { |_, sequence| sequence[:database] || sequence[:temporary] || @tables[sequence[:owned_by]&.first]&.dig(:temporary) }.map do |name, sequence|
          { name: name, increment: sequence[:increment], min_value: sequence[:min_value] || 1,
            max_value: sequence[:max_value] || ((1 << 63) - 1), start: sequence[:start] || 1,
            cache: sequence[:cache] || 1, cycle: !!sequence[:cycle], last_value: sequence[:value],
            is_called: sequence[:called], owned_by: sequence[:owned_by] }
        end,
        collations: export_collations,
        views: @views.reject { |_, view| view[:temporary] || view[:database] }.map { |name, view| export_view(name, view) }
      }
    end

    def export_attachment_storage(domain)
      attachment = @attachments.fetch(domain)
      base = attachment[:stored_catalog] || { types: [], sequences: [], views: [], collations: [] }
      stored_tables = @tables.values.select { |table| table[:database] == domain }
      names = stored_tables.to_h { |table| [table[:name], table[:local_name] || table[:name]] }
      tables = stored_tables.map do |table|
        state = export_table(table)
        index_names = table[:indexes].to_h { |index| [index[:name], index[:local_name] || index[:name]] }
        local = ->(name) { names[name] || index_names[name] || name.to_s.split("\0").last }
        state[:name] = names.fetch(table[:name])
        state[:indexes] = state[:indexes].map { |index| index.merge(name: local.call(index[:name])) }
        state[:fks] = state[:fks].map { |fk| fk.merge(name: local.call(fk[:name]), ref_table: local.call(fk[:ref_table])) }
        state[:checks] = state[:checks].map { |check| check.merge(name: local.call(check[:name])) }
        state[:exclusions] = state[:exclusions].map { |exclusion| exclusion.merge(name: local.call(exclusion[:name]), index: local.call(exclusion[:index])) }
        state
      end
      base.merge(tables: tables)
    end

    def remember_attachment_storage(domain)
      @attachments.fetch(domain)[:storage].collation_key = ->(name, value) { resolve_collation(name).sort_key(value) }
      @attachments.fetch(domain)[:storage_signature] = storage_signature(export_attachment_storage(domain))
    end

    private

    def adopt_storage_pages(storage, domain)
      catalog = storage.load[:tables].to_h { |entry| [entry[:name], entry] }
      catalog_types = @types
      @tables.transform_values! do |table|
        next table if table[:temporary] || table[:database] != domain
        entry = catalog.fetch(table[:local_name] || table[:name])
        raw = entry[:rows]
        rows = raw.converted { |type, value| Types.from_storage_value(type, value, catalog_types) }
        raw_indexes = entry[:indexes].to_h { |index| [index[:name], index] }
        indexes = table[:indexes].map do |index|
          stored = raw_indexes.delete(index[:local_name] || index[:name])
          next index unless stored
          index.merge(storage_tree_profile: stored[:tree_profile], storage_gist_profile: stored[:gist_profile], storage_index: stored)
        end
        # Unique/exclusion backing indexes also become regular paged indexes.
        raw_indexes.each_value { |index| indexes << import_index(entry, index) }
        rowids = entry[:pk].empty? ? rows.rowids(FileFormat.new) : PageSequence.new(rows.length, ->(index) { index })
        clean = table.reject { |key, _| key.to_s.start_with?('native_') }
        clean.merge(rows: rows, rowids: rowids, indexes: indexes,
                    storage_rows: rows, storage_statistics: entry[:statistics], storage_tree_profile: rows.profile,
                    storage_tree_rows: rows, storage_keys: rows.keys)
      end
      refresh_primary_indexes
    end

    def export_collations
      references = (@storage_collations || []).to_h { |entry| [entry[:name], entry] }
      default = respond_to?(:default_collation) ? default_collation : 'C'
      references.transform_values! { |entry| entry.merge(default: entry[:name] == default) }
      if default != 'C' && !references.key?(default)
        loaded = resolve_collation(default)
        references[default] = { name: default, default: true, unicode: loaded.unicode_version, cldr: loaded.cldr_version, desc: loaded.description }
      end
      @tables.each_value do |table|
        next if table[:temporary] || table[:database]
        table[:columns].each do |column|
          name = column[:collation]
          next unless name && name != 'C' && !references.key?(name)
          loaded = resolve_collation(name)
          references[name] = { name: name, default: false, unicode: loaded.unicode_version, cldr: loaded.cldr_version, desc: loaded.description }
        end
      end
      references.values.sort_by { |entry| entry[:name].b }
    end

    def invalidate_storage_profiles(domain)
      # Imported scan profiles have served their purpose; the immutable native
      # tree now carries the actual shape across subsequent edits and commits.
      @tables.transform_values! do |table|
        next table unless table[:database] == domain && table[:storage_tree_profile]
        table.reject { |key, _| %i[storage_tree_profile storage_tree_rows native_page_signature native_secondary_signature native_auxiliary_signature].include?(key) }
             .merge(indexes: table[:indexes].map { |index| index.reject { |key, _| key == :storage_tree_profile } })
      end
    end

    def storage_signature(value)
      case value
      when Hash
        value.reject { |key, _| %i[evaluate where physical_tree physical_gist tree_profile gist_profile].include?(key) }.transform_values { |item| storage_signature(item) }
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
        fields = modifier[:fields]
        fields = DateTimeUnits::INTERVAL_QUALIFIER_CODES.fetch(fields) if fields.is_a?(String)
        result[:temporal] = { precision: modifier[:precision], fields: fields || 0 }
      end
      result[:collation] = column[:collation] if column[:collation] && column[:collation] != 'C'
      if column[:default]
        container = type.end_with?('[]') || FileFormat::RANGE_ELEM.key?(type) || @types.key?(type)
        null_default = container && column[:default][:kind] == :literal && column[:default][:value].nil?
        if !null_default && (type.end_with?('[]') || FileFormat::RANGE_ELEM.key?(type))
          raise Error.new('0A000', 'container column defaults have no supported file representation')
        end
        result[:default_expr] = catalog_expression(column[:default]) unless null_default
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
          checks << { name: name, expr: catalog_expression(constraint[:expr]), host_deps: constraint[:host_deps] || constraint.dig(:storage_check, :host_deps) || [] }
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
            indexes << export_index(table, { name: name, method: constraint[:method] || 'gist', elements: elements, where: constraint[:where] })
          end
          exclusions << { name: name, index: name, elements: elements.map { |element| [column_ordinal(table, element[:expr][:name]), element[:op]] } }
        end
      end
      rows = if table[:rows].is_a?(PagedRows)
        table[:rows].raw || table[:rows]
      else
        table[:rows].map do |row|
        row.each_with_index.map { |value, index| storage_value(columns[index][:type], value) }
        end
      end
      result = { name: table[:name], columns: columns, rows: rows, pk_order: pk_order,
                 checks: checks.sort_by { |check| check[:name].b }, indexes: indexes.sort_by { |index| index[:name].b },
                 fks: fks.sort_by { |fk| fk[:name].b }, exclusions: exclusions.sort_by { |exclusion| exclusion[:name].b } }
      result[:physical_tree] = table[:native_primary_tree] if table.key?(:native_primary_tree)
      result[:indexes].each do |index|
        metadata = table[:native_secondary_metadata]&.[](index[:name]) || table[:native_auxiliary_indexes]&.[](index[:name])
        if index[:kind] == 'gist'
          index[:physical_gist] = metadata[:tree] if metadata
          index[:physical_gist] = index[:gist_profile][:tree] if !metadata && index[:gist_profile]
          index[:physical_gist] = nil if !metadata && index[:root_page] == 0
        elsif metadata && metadata.key?(:physical_tree)
          index[:physical_tree] = metadata[:physical_tree]
        elsif index[:tree_profile]
          index[:physical_tree] = index[:tree_profile][:paged_rows] ? index[:tree_profile][:tree] : statistics_codec.tree_from_profile(index[:tree_profile], [])
        end
      end
      result[:rebuild_generation] = table[:index_rebuild_generation] if table[:index_rebuild_generation]
      if table[:rows].is_a?(PagedRows)
        result[:storage_keys] = table[:rows].keys
      elsif pk_order.empty? && table[:rowids]
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
      if index[:storage_index] && (index[:unparsed] || respond_to?(:host_index_usable?, true) && !host_index_usable?(index))
        return index[:storage_index].dup
      end
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
      result[:host_deps] = index[:host_deps] if index[:host_deps]
      if index[:where]
        reject_stateful_index(index[:where])
        predicate = compile_filter(index[:where], scope)
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
      cost, lifetime = @cost, @lifetime_cost
      maximum, lifetime_maximum = @max_cost, @lifetime_max_cost
      @max_cost, @lifetime_max_cost = 0, 0
      row = stored_row.each_with_index.map { |value, i| from_storage_value(table[:columns][i][:type], value) }
      storage_value(type, evaluate(expression, row))
    ensure
      @cost, @lifetime_cost = cost, lifetime
      @max_cost, @lifetime_max_cost = maximum, lifetime_maximum
    end

    def storage_value(type, value)
      Types.storage_value(type, value, @types)
    end

    def from_storage_value(type, value)
      Types.from_storage_value(type, value, @types)
    end

    def import_storage(state)
      @storage_collations = state[:collations] || []
      @default_collation = @storage_collations.find { |entry| entry[:default] }&.dig(:name) || 'C'
      skewed_collations = @storage_collations.filter_map do |entry|
        begin
          loaded = resolve_collation(entry[:name])
        rescue Error
          raise Error.new('XX002', "required collation #{entry[:name]} is unavailable")
        end
        entry[:name] if [entry[:unicode], entry[:cldr]] != [loaded.unicode_version, loaded.cldr_version]
      end
      @types = state[:types].to_h do |type|
        [type[:name], type[:fields].map { |field| import_column(field) }]
      end
      @tables = state[:tables].to_h do |stored|
        columns = stored[:columns].map do |column|
          imported = import_column(column)
          imported[:default][:detail] = "default of column #{column[:name].inspect} of table #{stored[:name].inspect}" if opaque_definition?(imported[:default])
          imported
        end
        constraints = []
        unless stored[:pk].empty?
          constraints << { kind: :primary_key, name: object_name(stored[:name], '', 'pkey'),
                           columns: stored[:pk].map { |index| columns[index][:name] } }
        end
        stored[:checks].each do |check|
          constraints << { kind: :check, name: check[:name], storage_check: check,
                           host_deps: check[:host_deps],
                           expr: parse_stored_expression(check[:expr], "check constraint #{check[:name].inspect} of table #{stored[:name].inspect}") }
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
          constraints << { kind: :exclude, name: exclusion[:name], method: backing[:method], where: backing[:where],
                           elements: exclusion[:elements].map { |ordinal, operator| { expr: { kind: :column, name: columns[ordinal][:name] }, op: operator } } }
        end
        indexes.each do |index|
          next unless index[:unique] && !index[:where] && index[:elements].all? { |element| element[:expr][:kind] == :column }

          constraints << { kind: :unique, name: index[:name], columns: index[:columns] }
        end
        rows = if stored[:rows].is_a?(PagedRows)
          catalog_types = @types
          stored[:rows].converted { |type, value| Types.from_storage_value(type, value, catalog_types) }
        else
          stored[:rows].map { |row| row.each_with_index.map { |value, i| from_storage_value(columns[i][:type], value) } }
        end
        rowids = if stored[:pk].empty?
                   codec = FileFormat.new
                   rows.is_a?(PagedRows) ? rows.rowids(codec) : stored[:storage_keys].map { |key| codec.decode_int(8, key) }
                 else
                   rows.is_a?(PagedRows) ? PageSequence.new(rows.length, ->(index) { index }) : (0...rows.length).to_a
                 end
        [stored[:name], { name: stored[:name], columns: columns, constraints: constraints, indexes: indexes,
                         rows: rows, rowids: rowids, next_rowid: (rows.is_a?(PagedRows) ? rowids.last || -1 : rowids.max || -1) + 1, temporary: false,
                         storage_rows: rows, storage_statistics: stored[:statistics], storage_tree_profile: stored[:tree_profile], storage_tree_rows: rows,
                         storage_keys: stored[:storage_keys],
                         collation_skew: columns.filter_map { |column| column[:collation] if skewed_collations.include?(column[:collation]) }.uniq }]
      end
      @sequences = state[:sequences].to_h do |sequence|
        [sequence[:name], sequence.merge(value: sequence[:last_value], called: sequence[:is_called])]
      end
      @views = state[:views].to_h do |view|
        query = parse_stored_query(view[:definition], "view #{view[:name].inspect}")
        dependencies = view[:deps].select { |dependency| dependency[:kind] < 2 }.to_h { |dependency| [dependency[:name], dependency[:columns]] }
        [view[:name], { kind: :create_view, name: view[:name], columns: view[:columns], query: query,
                       storage_view: view, storage_query: Marshal.load(Marshal.dump(query)),
                       dependencies: dependencies,
                       sequence_dependencies: view[:deps].select { |dependency| dependency[:kind] == 2 }.map { |dependency| dependency[:name] },
                       type_dependencies: view[:deps].select { |dependency| dependency[:kind] == 3 }.map { |dependency| dependency[:name] },
                       defaults: (view[:defaults] || {}).transform_values { |sql| parse_stored_expression(sql, "default of view #{view[:name].inspect}") },
                       check_option: [nil, :local, :cascaded][view[:check_option] || 0],
                       options: { 'security_barrier' => !!view[:security_barrier], 'security_invoker' => !!view[:security_invoker] } }]
      end
      refresh_primary_indexes
    end

    def import_column(column)
      modifier = if column[:precision]
                   { precision: column[:precision], scale: column[:scale] }
                 elsif column[:varchar_len]
                   { precision: column[:varchar_len], scale: nil }
                 elsif column[:temporal]
                   temporal = column[:temporal]
                   fields = temporal[:fields]
                   temporal.merge(fields: fields.to_i.zero? ? nil : DateTimeUnits::INTERVAL_QUALIFIER_CODES.key(fields))
                 end
      default = if column[:default_expr]
                  parse_stored_expression(column[:default_expr], "default of column #{column[:name].inspect}")
                elsif column.key?(:default) && column[:default] != :none
                  { kind: :literal, type: column[:type], value: from_storage_value(column[:type], column[:default]) }
                end
      { name: column[:name], type: column[:type], nullable: !column[:not_null], primary_key: !!column[:pk],
        type_mod: modifier, default: default, identity: column[:identity], collation: column[:collation], checks: [] }
    end

    def import_index(table, index)
      raise Error.new('XX002', 'index requires unavailable pinned timezone data') if index[:timezone_deps]

      elements = index[:cols].each_with_index.map do |column, position|
        expression = column.is_a?(Hash) ? parse_stored_expression(column[:expr], "index #{index[:name].inspect} of table #{table[:name].inspect}") : { kind: :column, name: table[:columns][column][:name] }
        desc, first = FileFormat::ORDER_SYMBOLS.fetch(index[:orders]&.at(position) || :asc)
        { expr: expression, direction: desc ? :desc : :asc, nulls: first ? :first : :last }
      end
      predicate = index[:predicate] ? parse_stored_expression(index[:predicate], "index #{index[:name].inspect} of table #{table[:name].inspect}") : nil
      unparsed = opaque_definition?(predicate) || elements.any? { |element| opaque_definition?(element[:expr]) }
      { kind: :create_index, name: index[:name], table: table[:name], method: index[:kind], unique: index[:unique], storage_index: index, unparsed: unparsed,
        host_deps: index[:host_deps],
        storage_gist_profile: index[:gist_profile],
        elements: elements, columns: elements.map { |element| element[:expr][:name] }, storage_tree_profile: index[:tree_profile],
        where: predicate }
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
