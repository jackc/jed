# frozen_string_literal: true

module JedNative
  module SchemaResolution
    def dispatch(statement)
      if statement[:kind] == :create_type
        fields = statement[:fields].map { |field| field[:name] }
        raise Error.new('42701', 'duplicate composite field') unless fields.uniq.length == fields.length
      elsif statement[:kind] == :drop_type && statement[:cascade]
        names = statement[:names] || [statement[:name]]
        stored = @tables.values.any? { |table| table[:columns].any? { |column| names.include?(column[:type].delete_suffix('[]')) } }
        stored ||= @types.any? { |name, fields| !names.include?(name) && fields.any? { |field| names.include?(field[:type].delete_suffix('[]')) } }
        raise Error.new('0A000', 'DROP TYPE CASCADE cannot rewrite stored columns') if stored
      end
      super
    end
    def relation_exists?(name)
      super || @tables.values.any? do |table|
        table[:constraints].any? { |constraint| %i[unique exclude].include?(constraint[:kind]) && constraint[:name] == name }
      end
    end

    # PostgreSQL's makeObjectName: shorten the longer part first and retain the
    # complete suffix; clipping to a UTF-8 boundary happens after byte budgeting.
    def object_name(table, part, label)
      part = nil if part == ''
      left, right = table.bytesize, part&.bytesize || 0
      budget = 63 - label.bytesize - (part ? 2 : 1)
      while left + right > budget
        left > right ? left -= 1 : right -= 1
      end
      [identifier_prefix(table, left), part && identifier_prefix(part, right), label].compact.join('_')
    end

    def object_name_part(names)
      result = []
      names.each do |name|
        result << name
        break if result.join('_').bytesize >= 64
      end
      result.join('_')
    end

    def name_constraints(table)
      # Constraint names belong to one database, while attached tables share
      # the executor catalog under internal qualified keys.
      previous_tables, previous_views, previous_sequences = @tables, @views, @sequences
      domain = table[:database] || domain_from_key(table[:name])
      @tables = @tables.filter_map do |key, stored|
        next unless (stored[:database] || domain_from_key(key)) == domain && !!stored[:temporary] == !!table[:temporary]
        local = local_domain_name(key)
        [local, stored.merge(name: local)]
      end.to_h
      @views, @sequences = {}, {} if domain
      name_local_constraints(table.merge(name: local_domain_name(table[:name])))
    ensure
      @tables, @views, @sequences = previous_tables, previous_views, previous_sequences
    end

    def name_local_constraints(table)
      existing = @tables[table[:name]]&.dig(:constraints) || []
      explicit = {}
      table[:constraints].each do |constraint|
        next unless constraint[:name]
        relation = %i[unique exclude].include?(constraint[:kind])
        name = constraint[:name]
        if explicit.key?(name)
          raise Error.new(relation && %i[unique exclude].include?(explicit[name]) ? '42P07' : '42710', 'duplicate constraint name')
        end
        if relation && (name == table[:name] || relation_exists?(name) && existing.none? { |item| item[:name] == name })
          raise Error.new('42P07', 'constraint index relation already exists')
        end
        explicit[name] = constraint[:kind]
      end
      used = @tables.values.reject { |other| other[:name] == table[:name] || !!other[:temporary] != !!table[:temporary] }.flat_map { |other| other[:constraints].filter_map { |constraint| constraint[:name] } }
      named = []
      table[:constraints].sort_by { |constraint| constraint[:definition_order] || -1 }.each do |constraint|
        if constraint[:name]
          raise Error.new('42710', 'duplicate constraint name') if named.include?(constraint[:name])
          named << constraint[:name]
          used << constraint[:name]
          next
        end
        kind = constraint[:kind]
        label = { primary_key: 'pkey', unique: 'key', check: 'check', foreign_key: 'fkey', exclude: 'excl' }.fetch(kind)
        columns = constraint[:columns] || schema_column_names(constraint[:elements] || constraint[:expr])
        columns = [] if kind == :primary_key || kind == :check && columns.length != 1
        part = object_name_part(columns)
        suffix = 0
        name = object_name(table[:name], part, label)
        unless kind == :primary_key
          while used.include?(name) || %i[unique exclude].include?(kind) && (explicit.key?(name) || name == table[:name] || relation_exists?(name))
            suffix += 1
            name = object_name(table[:name], part, "#{label}#{suffix}")
          end
        end
        constraint[:name] = name
        used << name
        named << name
      end
    end

    def name_schema_constraints(table) = name_constraints(table)

    def create_table(statement)
      unless relation_exists?(statement[:name])
        (statement[:columns] || []).each { |column| validate_column_default(column) }
      end
      super
    end

    def validate_column_default(column)
      node = column[:default]
      return unless node && node[:kind] != :opaque_definition
      schema_expression_structure(node, :default)
      raise Error.new('0A000', 'column reference in DEFAULT') unless schema_column_names(node).empty?
      expression = compile(node, [])
      target = Types.normalize(Sequences::SERIAL_TYPES[column[:type]] || column[:type])
      raise Error.new('42804', 'DEFAULT has incompatible type') unless context_assignable?(expression, target) || %w[unknown null].include?(expression.type)
    end

    def validate_definition(table)
      primary = table[:constraints].find { |constraint| constraint[:kind] == :primary_key }
      groups = @tables.key?(table[:name]) ? {} : table[:constraints].select { |constraint| constraint[:kind] == :unique }.group_by { |constraint| constraint[:columns] }
      groups.each do |columns, constraints|
        survivor = constraints.find { |constraint| constraint[:name] } || constraints.first
        survivor = nil if primary && primary[:columns] == columns
        table[:constraints] -= constraints.reject { |constraint| constraint.equal?(survivor) }
      end
      table[:constraints].each do |constraint|
        next unless constraint[:kind] == :exclude
        method = constraint[:method] || 'btree'
        raise Error.new('0A000', 'temporary exclusion constraints are not supported') if table[:temporary]
        raise Error.new('0A000', 'GIN cannot enforce exclusion constraints') if method == 'gin'
        raise Error.new('42704', 'unknown exclusion access method') unless %w[btree gist].include?(method)
        names = constraint[:elements].map { |element| element[:expr][:name] }
        raise Error.new('42701', 'duplicate exclusion column') if method == 'gist' && names.uniq.length != names.length
        constraint[:elements].each do |element|
          node = element[:expr]
          raise Error.new('0A000', 'exclusion expressions are not supported') unless node[:kind] == :column
          type = compile(node, table_columns(table)).type
          if method == 'btree'
            raise Error.new('42809', 'btree exclusions require equality') unless element[:op] == '='
            raise Error.new('0A000', 'type has no ordered keys') if %w[json jsonpath].include?(type)
          else
            auxiliary_type_gate('gist', type)
            raise Error.new('42809', 'unsupported GiST exclusion operator') unless Types::RANGES.key?(type) ? element[:op] == '&&' : element[:op] == '='
          end
        end
        if constraint[:where]
          validate_index_expression(constraint[:where], table_columns(table))
          compile_boolean_value(constraint[:where], table_columns(table))
        end
      end
      super
    end

    def create_index(statement)
      table = table(statement[:table])
      if (statement[:method] || 'btree') == 'btree'
        scope = table_columns(table)
        (statement[:elements] || []).each { |element| validate_index_expression(element[:expr], scope) }
        if statement[:where]
          validate_index_expression(statement[:where], scope)
          compile_boolean_value(statement[:where], scope)
        end
      end
      unless statement[:name]
        parts = []
        (statement[:elements] || []).each do |element|
          node = element[:expr]
          base = %i[column call].include?(node[:kind]) ? node[:name] : 'expr'
          part, count = base, 0
          while parts.include?(part)
            count += 1
            part = identifier_prefix(base, 63 - count.to_s.bytesize) + count.to_s
          end
          parts << part
        end
        part = object_name_part(parts)
        name, count = object_name(table[:name], part, 'idx'), 0
        while relation_exists?(name)
          count += 1
          name = object_name(table[:name], part, "idx#{count}")
        end
        statement = statement.merge(name: name)
      end
      super(statement)
    end

    def drop_index(statement)
      owner = @tables.values.find { |table| table[:constraints].any? { |constraint| constraint[:kind] == :unique && constraint[:name] == statement[:name] } }
      return super unless owner
      constraint = owner[:constraints].find { |item| item[:name] == statement[:name] }
      drop_incoming_foreign_keys(owner, constraint[:columns], statement[:cascade])
      @tables[owner[:name]] = owner.merge(constraints: owner[:constraints] - [constraint], indexes: owner[:indexes].reject { |index| index[:name] == statement[:name] })
      @dirty = true
      Result.new
    end

    def reindex_targets(statement)
      if statement[:object] == :database && statement[:name] && @attachments.key?(statement[:name])
        return @tables.values.select { |table| table[:database] == statement[:name] }.sort_by { |table| table[:name].b }.map { |table| [table, catalog_indexes(table)] }
      end
      super
    end

    def drop_table(statement)
      names = (statement[:names] || [statement[:name]]).uniq
      names.each do |name|
        raise Error.new('42809', 'relation is not a table') if !@tables.key?(name) && relation_exists?(name)
      end
      super(statement.merge(names: names))
    end

    def alter_table(statement)
      table = @tables[statement[:name]]
      raise Error.new('42809', 'relation is not a table') if !table && !@views.key?(statement[:name]) && relation_exists?(statement[:name])
      constraint = table && table[:constraints].find { |item| item[:name] == statement[:column] }
      if table && table[:database] && !%w[main temp].include?(table[:database])
        forbidden = (statement[:actions] || []).any? { |action| action[:kind] == :add_constraint && %i[foreign_key exclude].include?(action.dig(:constraint, :kind)) }
        raise Error.new('0A000', 'attached constraints are not supported') if forbidden
      end
      if statement[:action] == :rename_constraint && constraint && %i[unique exclude].include?(constraint[:kind]) && statement[:new_name].start_with?('jed_')
        raise Error.new('42939', 'reserved constraint name')
      end
      if table && (statement[:actions] || []).any? { |action| action[:kind] == :set_type }
        columns = table[:columns].map(&:dup)
        statement[:actions].each do |action|
          if action[:kind] == :add_column
            columns << action[:column].merge(type: Types.normalize(Sequences::SERIAL_TYPES[action[:column][:type]] || action[:column][:type]))
            next
          elsif action[:kind] == :drop_column
            columns.reject! { |column| column[:name] == action[:column] }
            next
          end
          next unless action[:kind] == :set_type
          column = columns.find { |candidate| candidate[:name] == action[:column] }
          raise Error.new('42703', 'column does not exist') unless column
          validate_type(Types.normalize(action[:type]), action[:type_mod])
          compile(action[:using], table_columns(table)) if action[:using]
          column[:type] = Types.normalize(action[:type])
        end
        table[:indexes].each do |index|
          (index[:elements] || []).each do |element|
            node = element[:expr]
            type = compile(node, table_columns(table.merge(columns: columns))).type
            if %w[gin gist].include?(index[:method])
              auxiliary_type_gate(index[:method], type)
            elsif %w[json jsonpath].include?(type)
              raise Error.new('0A000', 'type has no ordered keys')
            end
          end
        end
      end
      super
    end
  end
  Engine.prepend(SchemaResolution)
end
