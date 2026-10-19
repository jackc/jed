# frozen_string_literal: true

module JedNative
  # All relation versions share the statement/transaction snapshot, while their
  # catalog keys include the owning attachment. SQL identifiers cannot contain
  # NUL, so this internal key cannot collide with an authored identifier.
  module Domains
    def initialize(**options)
      @attachments = {}
      super
    end

    def attach(name, source = :memory, mode = :read_write, **options)
      name = String(name)
      raise Error.new('55000', 'database is closed') if @closed
      raise Error.new('42710', 'attachment name is reserved or already attached') if %w[main temp].include?(name) || @attachments.key?(name)
      raise Error.new('22023', 'attachment name is empty or contains NUL') if name.empty? || name.include?("\0")
      raise Error.new('25001', 'cannot attach during a transaction') if @transaction
      mode = options.delete(:mode) || mode
      raise Error.new('22023', 'invalid attachment mode') unless %i[read_only read_write].include?(mode)
      attachment = { read_only: mode == :read_only }
      unless source.nil? || source == :memory
        require_relative 'persistence'
        if @storage && !source.is_a?(Storage)
          inherited = { locking: @storage.instance_variable_get(:@locking),
                        file_lock_timeout_ms: @storage.instance_variable_get(:@service_timeout) || 5000,
                        fsync: @storage.instance_variable_get(:@sync),
                        cache_bytes: @storage.instance_variable_get(:@cache_bytes) }
          inherited.merge!(@storage.instance_variable_get(:@service_options) || {})
          options = inherited.merge(options)
        end
        storage = source.is_a?(Storage) ? source : Storage.new(path: source, create: false, read_only: attachment[:read_only], **options)
        attachment[:storage] = storage
        import_attachment_catalog(name, attachment, storage.load)
      end
      @attachments[name] = attachment
      remember_attachment_storage(name) if attachment[:storage] && respond_to?(:remember_attachment_storage, true)
      refresh_primary_indexes
      storage.service_finish_open if storage
      nil
    rescue StandardError
      storage&.close unless defined?(Storage) && source.is_a?(Storage) || @attachments.key?(name)
      raise
    end

    def import_attachment_catalog(name, attachment, catalog)
      attachment[:stored_catalog] = catalog
      storage = attachment[:storage]
      attachment[:service_publication] = [storage.service_generation, storage.meta[:txid]] if storage
      loader = Engine.new(environment: @environment)
      loader.send(:import_storage, catalog)
      @tables = @tables.reject { |_, table| table[:database] == name }
      loader.tables.each do |local_name, table|
        key = domain_key(name, local_name)
        constraints = table[:constraints].map do |constraint|
          constraint[:kind] == :foreign_key ? constraint.merge(table: domain_key(name, constraint[:table])) : constraint
        end
        indexes = table[:indexes].map { |index| index.merge(name: domain_key(name, index[:name]), local_name: index[:name]) }
        @tables[key] = table.merge(name: key, local_name: local_name, database: name, constraints: constraints, indexes: indexes)
      end
      attachment[:views] = loader.instance_variable_get(:@views)
      attachment[:types] = loader.instance_variable_get(:@types)
    end

    def detach(name)
      raise Error.new('55000', 'database is closed') if @closed
      attachment = @attachments[name] || raise(Error.new('42704', 'database is not attached'))
      raise Error.new('55006', 'attachment is in use by a transaction') if @transaction
      @tables = @tables.reject { |_, table| table[:database] == name }
      @attachments.delete(name)
      attachment[:storage]&.close
      refresh_primary_indexes
      nil
    end

    def close
      @attachments.each_value { |attachment| attachment[:storage]&.close }
      super
    end

    def run(statement, params = [])
      committing = statement[:kind] == :commit && @transaction
      @durable_writer = nil unless @transaction
      statement = route_domains(statement)
      super
    rescue StandardError => error
      if committing
        restore(committing)
        @transaction, @failed, @dirty, @read_only, @savepoints = nil, false, false, false, []
      else
        @failed = true if @transaction && !(statement[:kind] == :begin && error.is_a?(Error) && error.sqlstate == '25001')
      end
      raise
    ensure
      @durable_writer = nil unless @transaction
    end

    def snapshot = super + [{ native_domain_snapshot: true, durable_writer: @durable_writer }]

    def restore(state)
      if state.last.is_a?(Hash) && state.last[:native_domain_snapshot]
        @durable_writer = state.last[:durable_writer]
        super(state[0...-1])
      else
        super
      end
    end

    def dispatch(statement)
      if statement[:kind] == :drop_type
        names = statement[:names] || [statement[:name]]
        dependent = @tables.values.any? { |table| table[:columns].any? { |column| names.include?(column[:type].delete_suffix('[]')) } }
        dependent ||= @types.any? { |name, fields| !names.include?(name) && fields.any? { |field| names.include?(field[:type].delete_suffix('[]')) } }
        raise Error.new('2BP01', 'a relation or composite depends on the type') if dependent
      end
      if statement[:kind] == :select && statement[:into]
        target = statement[:into]
        query = statement.reject { |key, _| key == :into }
        return dispatch(target.merge(kind: :create_table, query: query, columns: [], constraints: []))
      end
      if statement[:kind] == :commit && @transaction && !@failed && !@read_only
        apply_temp_commit_actions
      end
      validate_domain_write(statement)
      super
    end

    def temporary_statement?(statement)
      return true if statement[:kind] == :create_table && statement[:temporary]
      target = statement[:table] || statement[:name]
      if statement[:kind] == :create_view
        return statement[:temporary] || statement[:replace] && @views[statement[:name]]&.dig(:temporary)
      end
      if %i[insert update delete alter_table alter_view create_index analyze].include?(statement[:kind])
        return !!(@tables[target] || @views[target])&.dig(:temporary)
      end
      if %i[drop_table drop_view truncate].include?(statement[:kind])
        names = statement[:names] || [target]
        return names.all? { |name| !!(@tables[name] || @views[name])&.dig(:temporary) }
      end
      if statement[:kind] == :drop_index
        return (statement[:names] || [target]).all? do |name|
          @tables.values.any? { |table| table[:temporary] && table[:indexes].any? { |index| index[:name] == name } }
        end
      end
      if %i[drop_sequence alter_sequence].include?(statement[:kind])
        return (statement[:names] || [target]).all? do |name|
          sequence = @sequences[name]
          sequence && (sequence[:temporary] || @tables[sequence[:owned_by]&.first]&.dig(:temporary))
        end
      end
      false
    end

    def sequence_value(name, operation, value = nil, called = true)
      previous_read_only = @read_only
      if @read_only && name
        sequence = @sequences[sequence_name(name)]
        owner = sequence && @tables[sequence[:owned_by]&.first]
        @read_only = false if sequence&.dig(:temporary) || owner&.dig(:temporary)
      end
      super
    ensure
      @read_only = previous_read_only
    end

    def flush_foreign_keys
      super
      apply_temp_commit_actions if !@transaction && !@read_only
    end

    def create_table(statement)
      name = statement[:name]
      return Result.new if statement[:if_not_exists] && relation_exists?(name)
      raise Error.new('42P16', 'ON COMMIT requires a temporary table') if statement[:on_commit] && !statement[:temporary]
      domain = statement[:native_database]
      if domain
        forbidden = (statement[:constraints] || []).any? { |constraint| constraint[:kind] == :foreign_key }
        forbidden ||= (statement[:columns] || []).any? do |column|
          column[:references] || column[:identity] || Sequences::SERIAL_TYPES.key?(column[:type]) || @types.key?(Types.normalize(column[:type]))
        end
        raise Error.new('0A000', 'attachment table uses an unsupported catalog dependency') if forbidden
      end
      result = super
      table = @tables[name]
      if statement[:query]
        metadata = view_query_columns(statement[:query])
        columns = table[:columns].each_with_index.map do |column, index|
          column.merge(type_mod: metadata[index]&.dig(:type_mod))
        end
        table = table.merge(columns: columns)
      end
      @tables[name] = table.merge(on_commit: statement[:on_commit], database: domain, local_name: statement[:native_local_name] || name)
      result
    end

    def create_index(statement)
      result = super
      if statement[:native_database] && (table = @tables[statement[:table]])
        indexes = table[:indexes].map do |index|
          index[:name] == statement[:name] ? index.merge(local_name: statement[:native_index_name]) : index
        end
        @tables[statement[:table]] = table.merge(indexes: indexes)
      end
      result
    end

    def create_view(statement)
      attached = statement[:native_database]
      visit_view_nodes(statement[:query]) { |node| attached ||= node[:native_database] }
      raise Error.new('0A000', 'views cannot reference attachments') if attached
      super
    end

    def table_columns(table, name = table[:name])
      name = table[:local_name] if name == table[:name] && table[:database]
      super(table, name)
    end

    def source_relation(source)
      if source[:kind] == :table && source[:native_database] && @attachments.dig(source[:native_database], :views, source[:native_local_name])
        raise Error.new('0A000', 'attached views are not supported')
      end
      super
    end

    private

    def domain_key(database, name) = "\u0001#{database}\0#{name}"

    def route_domains(node)
      return node if node.is_a?(Hash) && node[:native_routed]
      # Prepared statements retain their routed syntax. Recheck qualifications
      # against the current namespace on every execution, without rebuilding the
      # whole tree or making the binding/selection caches miss by identity.
      return route_domain_node(node, []) unless node.is_a?(Hash) && node[:kind] == :select && !node[:into]
      @native_domain_routes ||= {}.compare_by_identity
      if (entry = @native_domain_routes[node])
        entry[1].each { |database, name, relation| validate_domain_qualification(database, name, relation) }
        return entry[0]
      end
      checks = []
      routed = route_domain_node(node, checks)
      @native_domain_routes.shift if @native_domain_routes.length >= 64
      @native_domain_routes[node] = [routed, checks]
      routed
    end

    def validate_domain_qualification(database, name, relation)
      unless %w[main temp].include?(database) || @attachments.key?(database)
        raise Error.new('42P01', "database #{database} is not attached")
      end
      return unless relation
      object = @tables[name] || @views[name]
      if object && !!object[:temporary] != (database == 'temp')
        raise Error.new('42P01', "relation #{database}.#{name} does not exist")
      end
    end

    def route_domain_node(node, checks)
      case node
      when Array then node.map { |item| route_domain_node(item, checks) }
      when Hash
        return node if node[:native_routed]
        routed = node.transform_values { |value| route_domain_node(value, checks) }
        kind, database = routed[:kind], routed[:database]
        if database
          name_key = routed[:table].is_a?(String) ? :table : :name
          local_name = routed[name_key]
          relation = !%i[create_table create_view].include?(kind) && local_name &&
                     %w[main temp].include?(database) && !introspection_relation?(local_name)
          validate_domain_qualification(database, local_name, relation)
          checks << [database, local_name, relation]
          if kind == :create_table || kind == :create_view
            if routed[:temporary] && database != 'temp'
              raise Error.new('42601', 'TEMP conflicts with the database qualifier')
            end
            routed[:temporary] = true if database == 'temp'
          end
          unless %w[main temp].include?(database) || introspection_relation?(local_name)
            routed[:native_database], routed[:native_local_name] = database, local_name
            if local_name
              routed[name_key] = domain_key(database, local_name)
              routed[:name] = routed[name_key] if routed[:name] == local_name && name_key == :table
              routed[:alias] ||= local_name if %i[table insert update delete].include?(kind)
            end
            if kind == :create_index && routed[:name]
              routed[:native_index_name] = routed[:name]
              routed[:name] = domain_key(database, routed[:name])
            end
            routed[:new_name] = domain_key(database, routed[:new_name]) if routed[:action] == :rename
            routed[:database] = nil
          end
        end
        if routed[:targets]
          routed[:names] = routed[:targets].map { |target| target[:name] }
          routed[:name] = routed[:names].first
        end
        routed[:native_routed] = true if kind
        routed
      else node
      end
    end

    def validate_domain_write(statement)
      kind = statement[:kind]
      return unless kind.to_s.start_with?('create_', 'alter_', 'drop_') || %i[insert update delete truncate analyze reindex].include?(kind)
      domain = statement[:native_database]
      # Persistence checks the actual changed domains together at commit,
      # before any file is written. Merely visiting a domain (including zero-
      # row DML) must not reserve the durable writer slot.
      return unless domain
      attachment = @attachments.fetch(domain)
      raise Error.new('25006', 'attachment is read-only') if attachment[:read_only]
      if %i[create_view alter_view drop_view create_sequence alter_sequence drop_sequence create_type alter_type drop_type].include?(kind)
        raise Error.new('0A000', 'attachment catalog operation is not supported')
      end
      if kind == :insert && statement[:on_conflict]
        raise Error.new('0A000', 'ON CONFLICT on attachments is not supported')
      end
      if kind == :alter_table && attachment[:views]&.any? { |_, view| (view[:dependencies] || {}).key?(statement[:native_local_name]) }
        raise Error.new('0A000', 'an attached view depends on this table')
      end
    end

    def apply_temp_commit_actions
      actions = @tables.values.select { |table| table[:temporary] && %i[delete_rows drop].include?(table[:on_commit]) }
      return if actions.empty?
      truncated = actions.select { |table| table[:on_commit] == :delete_rows }.map { |table| table[:name] }
      @tables.each_value do |table|
        next if truncated.include?(table[:name])
        if table[:constraints].any? { |constraint| constraint[:kind] == :foreign_key && truncated.include?(constraint[:table]) }
          raise Error.new('0A000', 'unsupported ON COMMIT and foreign key combination')
        end
      end
      actions.each do |table|
        next unless table[:on_commit] == :delete_rows && !table[:rows].empty?
        statistics = (table[:storage_statistics] || []).map { |entry| entry.merge(stale: true) }
        @tables[table[:name]] = table.merge(rows: [], rowids: [], storage_statistics: statistics)
        @dirty = true
      end
      names = actions.select { |table| table[:on_commit] == :drop }.map { |table| table[:name] }
      drop_table(kind: :drop_table, names: names, name: names.first, cascade: true) unless names.empty?
    end

    def name_constraints(table)
      domain = table[:database] || domain_from_key(table[:name])
      previous = @tables
      local_name = table[:local_name] || local_domain_name(table[:name])
      original_name = table[:name]
      @tables = @tables.select { |_, stored| (stored[:database] || domain_from_key(stored[:name])) == domain && !!stored[:temporary] == !!table[:temporary] }
      table[:name] = local_name
      super
    ensure
      table[:name] = original_name
      @tables = previous
    end

    def name_schema_constraints(table) = name_constraints(table)

    def domain_from_key(name)
      name&.start_with?("\u0001") ? name[1...name.index("\0")] : nil
    end

    def local_domain_name(name) = name&.start_with?("\u0001") ? name.split("\0", 2).last : name

    def introspection_scope(database)
      return database if @attachments.key?(database)
      super
    end

    def catalog_tables(scope)
      @tables.values.select do |table|
        if %w[main temp].include?(scope)
          !table[:database] && !!table[:temporary] == (scope == 'temp')
        else
          table[:database] == scope
        end
      end.map { |table| table[:database] ? table.merge(name: table[:local_name]) : table }.sort_by { |table| table[:name].b }
    end
  end

  Engine.prepend(Domains)
end
