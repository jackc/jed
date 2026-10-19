# frozen_string_literal: true

module JedNative
  # A valid catalog can contain SQL authored by a newer parser. Preserve those
  # bytes and reject only operations which need to interpret the definition.
  module OpaqueDefinitions
    def opaque_definition?(node) = node.is_a?(Hash) && node[:kind] == :opaque_definition

    def parse_stored_expression(text, detail)
      Parser.new(text).parse_expression
    rescue Error
      { kind: :opaque_definition, text: text, detail: detail }
    end

    def parse_stored_query(text, detail)
      Parser.new(text).parse.first
    rescue Error
      { kind: :opaque_definition, text: text, detail: detail }
    end

    def require_parsed_definition(node)
      raise Error.new('XX003', "stored definition does not parse in this version of jed: #{node[:detail]}") if opaque_definition?(node)
    end

    def compile(node, columns)
      require_parsed_definition(node)
      super
    end

    def catalog_expression(node, parent_precedence = 0)
      opaque_definition?(node) ? node[:text] : super
    end

    def view_schema(view)
      return @opaque_replacement_schema if @opaque_replacement_name == view[:name]
      require_parsed_view(view)
      super
    end

    def dispatch(statement)
      validate_opaque_statement(statement)
      super
    end

    def drop_index(statement)
      owned = @tables.values.any? { |table| table[:constraints].any? { |constraint| constraint[:kind] == :exclude && constraint[:name] == statement[:name] } }
      raise Error.new('2BP01', 'constraint depends on index') if owned
      super
    end

    def insert(statement)
      require_parsed_write(@tables[statement[:table]], statement) if @tables[statement[:table]]
      super
    end

    def update(statement)
      require_parsed_write(@tables[statement[:table]], statement) if @tables[statement[:table]]
      super
    end

    def delete(statement)
      require_parsed_write(@tables[statement[:table]], statement) if @tables[statement[:table]]
      super
    end

    def rewrite_view_mutation(statement, operation)
      super do |rewritten|
        require_parsed_write(@tables[rewritten[:table]], rewritten)
        yield rewritten
      end
    end

    def build_secondary_indexes(table, codec, definitions = nil, fresh: false)
      definitions ||= catalog_indexes(table).select { |index| !index[:method] || index[:method] == 'btree' }
      super(table, codec, definitions.reject { |index| index[:unparsed] }, fresh: fresh)
    end

    def build_auxiliary_indexes(table, codec, definitions, fresh: false)
      super(table, codec, definitions.reject { |index| index[:unparsed] }, fresh: fresh)
    end

    def create_view(statement)
      old = @views[statement[:name]]
      return super unless old && statement[:replace] && opaque_definition?(old[:query])
      schema = view_query_columns(statement[:query])
      names = statement[:columns] || []
      schema = schema.each_with_index.map { |column, index| column.merge(name: names[index] || column[:name]) }
      unless schema.length >= old[:columns].length && old[:columns].each_with_index.all? { |name, index| schema[index][:name] == name }
        raise Error.new('42P16', 'cannot change existing view columns')
      end
      @opaque_replacement_name = old[:name]
      @opaque_replacement_schema = schema.first(old[:columns].length)
      super
    ensure
      @opaque_replacement_name = @opaque_replacement_schema = nil
    end

    def alter_view(statement)
      view = @views[statement[:name]]
      return super unless view && opaque_definition?(view[:query]) && %i[rename rename_column].include?(statement[:action])
      dependents = @views.values.select { |candidate| view_dependencies(candidate).key?(view[:name]) }
      require_parsed_view(view) unless dependents.empty?
      changed = view.merge(columns: view[:columns].dup)
      if statement[:action] == :rename
        raise Error.new('42P07', 'relation already exists') if relation_exists?(statement[:new_name])
        @views.delete(view[:name])
        changed[:name] = statement[:new_name]
      else
        index = changed[:columns].index(statement[:column])
        raise Error.new('42703', 'view column does not exist') unless index
        raise Error.new('42701', 'view column already exists') if changed[:columns].include?(statement[:new_name])
        changed[:columns][index] = statement[:new_name]
      end
      @views[changed[:name]] = changed
      @dirty = true
      Result.new
    end

    private

    def introspection_rows(name, scope)
      return super unless name == 'jed_views'
      catalog_views(scope).map do |view_name, view|
        options = view[:options] || {}
        names = view[:columns] || view_schema(view).map { |column| column[:name] }
        [view_name, catalog_view_definition(view), catalog_array(names), (view[:check_option] || 'NONE').to_s.upcase,
         !!options['security_barrier'], !!options['security_invoker']]
      end
    end

    def require_parsed_view(view, seen = {})
      return if seen[view[:name]]
      seen[view[:name]] = true
      require_parsed_definition(view[:query])
      view_dependencies(view).each_key { |name| require_parsed_view(@views[name], seen) if @views[name] }
    end

    def validate_opaque_statement(statement)
      # A cursor contains only a read statement. Opaque executable definitions
      # on that path can only come from views; table checks/defaults/indexes are
      # handled by mutation admission and are not interpreted by a plain read.
      return if @native_cursor && @views.empty?
      case statement[:kind]
      when :select, :values, :set_operation, :setop, :insert, :update, :delete, :explain
        validate_opaque_query_tree(statement)
      when :alter_table
        return if @views.key?(statement[:name])
        table = @tables[statement[:name]]
        return unless table
        actions = statement[:actions] || []
        cascade_drop = actions.any? { |action| action[:kind] == :drop_column && action[:cascade] }
        unless cascade_drop
          @views.each_value { |view| require_parsed_view(view) if view_dependencies(view).key?(table[:name]) }
        end
        edits = %i[rename rename_column].include?(statement[:action]) || actions.any? { |action| %i[set_type drop_column].include?(action[:kind]) }
        if edits
          table[:constraints].select { |constraint| constraint[:kind] == :check }.sort_by { |constraint| constraint[:name].to_s.b }.each { |constraint| require_parsed_definition(constraint[:expr]) }
          table[:columns].each do |column|
            needed = %i[rename rename_column].include?(statement[:action]) || actions.any? { |action| action[:kind] == :set_type && action[:column] == column[:name] }
            require_parsed_definition(column[:default]) if needed
          end
          require_parsed_indexes(table)
        end
      when :alter_view, :create_view
        name = statement[:name]
        replacing = statement[:kind] == :create_view && statement[:replace] && opaque_definition?(@views[name]&.dig(:query))
        @views.each_value { |view| require_parsed_view(view, replacing ? { name => true } : {}) if view_dependencies(view).key?(name) }
      when :reindex
        reindex_targets(statement).each { |table, indexes| require_parsed_indexes(table, indexes) }
      end
    end

    def validate_opaque_query_tree(node, ctes = {})
      case node
      when Array then node.each { |child| validate_opaque_query_tree(child, ctes) }
      when Hash
        local = ctes.merge((node[:with] || []).to_h { |entry| [entry[:name], true] })
        if node[:kind] == :table && !local[node[:name]]
          require_parsed_view(@views[node[:name]]) if @views[node[:name]]
          if node[:name] == 'jed_columns'
            @views.each_value { |view| require_parsed_view(view) }
          end
        elsif %i[insert update delete].include?(node[:kind])
          if (view = @views[node[:table]])
            require_parsed_view(view)
          elsif (table = @tables[node[:table]])
            require_parsed_write(table, node)
          end
        end
        node.each_value { |child| validate_opaque_query_tree(child, local) }
      end
    end

    def require_parsed_indexes(table, indexes = table[:indexes])
      index = indexes.find { |entry| entry[:unparsed] }
      return unless index
      raise Error.new('XX003', "stored definition does not parse in this version of jed: index #{index[:name].inspect} of table #{table[:name].inspect}")
    end

    def require_parsed_write(table, statement, seen = {})
      token = [table[:name], statement[:kind]]
      return if seen[token]
      seen[token] = true
      unless statement[:kind] == :delete
        table[:constraints].select { |constraint| constraint[:kind] == :check }.sort_by { |constraint| constraint[:name].to_s.b }.each { |constraint| require_parsed_definition(constraint[:expr]) }
        names = statement[:columns] || table[:columns].map { |column| column[:name] }
        table[:columns].each do |column|
          needed = if statement[:kind] == :insert
            position = names.index(column[:name])
            position.nil? || statement[:default_values] || (statement[:rows] || []).any? { |row| row[position]&.dig(:kind) == :default }
          else
            (statement[:assignments] || []).any? { |assignment| assignment[:column] == column[:name] && assignment.dig(:expr, :kind) == :default }
          end
          needed ||= (statement.dig(:on_conflict, :assignments) || []).any? { |assignment| assignment[:column] == column[:name] && assignment.dig(:expr, :kind) == :default }
          require_parsed_definition(column[:default]) if needed
        end
      end
      require_parsed_indexes(table)
      return unless %i[update delete].include?(statement[:kind])
      @tables.each_value do |child|
        child[:constraints].each do |constraint|
          next unless constraint[:kind] == :foreign_key && constraint[:table] == table[:name]
          action = constraint[statement[:kind] == :delete ? :on_delete : :on_update]
          next unless %i[cascade set_null set_default].include?(action)
          if statement[:kind] == :update && statement[:assignments]
            referenced = constraint[:ref_columns] || table[:constraints].find { |item| item[:kind] == :primary_key }&.dig(:columns) || []
            next if (referenced & statement[:assignments].map { |assignment| assignment[:column] }).empty?
          end
          operation = action == :cascade && statement[:kind] == :delete ? :delete : :update
          assignments = constraint[:columns].map { |name| { column: name, expr: { kind: action == :set_default ? :default : :literal } } }
          require_parsed_write(child, { kind: operation, assignments: assignments }, seen)
        end
      end
    end
  end
  Engine.prepend(OpaqueDefinitions)

  module OpaqueBindingDefinitions
    def statement(node, outer = [])
      @engine.require_parsed_definition(node)
      super
    end
  end
  ParameterBindings.prepend(OpaqueBindingDefinitions)
end
