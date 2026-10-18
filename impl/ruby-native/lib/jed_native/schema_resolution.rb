# frozen_string_literal: true

module JedNative
  module SchemaResolution
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
      existing = @tables[table[:name]]&.dig(:constraints) || []
      explicit = []
      table[:constraints].each do |constraint|
        next unless constraint[:name]
        relation = %i[unique exclude].include?(constraint[:kind])
        name = constraint[:name]
        if explicit.include?(name)
          raise Error.new(relation ? '42P07' : '42710', 'duplicate constraint name')
        end
        if relation && (name == table[:name] || relation_exists?(name) && existing.none? { |item| item[:name] == name })
          raise Error.new('42P07', 'constraint index relation already exists')
        end
        explicit << name
      end
      used = explicit + @tables.values.reject { |other| other[:name] == table[:name] }.flat_map { |other| other[:constraints].filter_map { |constraint| constraint[:name] } }
      table[:constraints].each do |constraint|
        next if constraint[:name]
        kind = constraint[:kind]
        label = { primary_key: 'pkey', unique: 'key', check: 'check', foreign_key: 'fkey', exclude: 'excl' }.fetch(kind)
        columns = constraint[:columns] || schema_column_names(constraint[:elements] || constraint[:expr])
        columns = [] if kind == :primary_key || kind == :check && columns.length != 1
        part = object_name_part(columns)
        suffix = 0
        name = object_name(table[:name], part, label)
        unless kind == :primary_key
          while used.include?(name) || %i[unique exclude].include?(kind) && (name == table[:name] || relation_exists?(name))
            suffix += 1
            name = object_name(table[:name], part, "#{label}#{suffix}")
          end
        end
        constraint[:name] = name
        used << name
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

    def drop_table(statement)
      names = (statement[:names] || [statement[:name]]).uniq
      names.each do |name|
        raise Error.new('42809', 'relation is not a table') if !@tables.key?(name) && relation_exists?(name)
      end
      super(statement.merge(names: names))
    end

    def alter_table(statement)
      if statement[:action] == :rename_constraint && statement[:new_name].start_with?('jed_')
        raise Error.new('42939', 'reserved constraint name')
      end
      super
    end
  end
  Engine.prepend(SchemaResolution)
end
