# frozen_string_literal: true

module JedNative
  module SchemaChanges
    def dispatch(stmt)
      case stmt[:kind]
      when :alter_table then alter_table(stmt)
      else super
      end
    end

    def alter_table(stmt)
      return Result.new if stmt[:if_exists] && !@tables.key?(stmt[:name])
      original = table(stmt[:name])
      t = original.merge(columns: original[:columns].map(&:dup), constraints: Marshal.load(Marshal.dump(original[:constraints])), indexes: Marshal.load(Marshal.dump(original[:indexes] || [])), rows: original[:rows].map(&:dup))
      name_schema_constraints(t)
      prior_constraints = t[:constraints].dup
      scanned = false
      scan = lambda do
        unless scanned
          charge(1 + original[:rows].length) unless original[:rows].empty?
          scanned = true
        end
      end
      retype = false
      rekey = false
      case stmt[:action]
      when :rename
        raise Error.new('42P07', 'relation already exists') if relation_exists?(stmt[:new_name])
        @tables.delete(t[:name])
        @sequences.transform_values! do |sequence|
          sequence[:owned_by]&.first == t[:name] ? sequence.merge(owned_by: [stmt[:new_name], sequence[:owned_by].last]) : sequence
        end
        @tables.transform_values! do |other|
          constraints = other[:constraints].map do |constraint|
            constraint[:kind] == :foreign_key && constraint[:table] == t[:name] ? constraint.merge(table: stmt[:new_name]) : constraint
          end
          other.merge(constraints: constraints)
        end
        t[:constraints].each { |constraint| constraint[:table] = stmt[:new_name] if constraint[:kind] == :foreign_key && constraint[:table] == t[:name] }
        t[:name] = stmt[:new_name]
      when :rename_column
        index = t[:columns].index { |c| c[:name] == stmt[:column] }
        raise Error.new('42703', 'column does not exist') unless index
        raise Error.new('42701', 'column already exists') if t[:columns].any? { |c| c[:name] == stmt[:new_name] }
        t[:columns][index][:name] = stmt[:new_name]
        t[:constraints].each { |c| c[:columns] = c[:columns].map { |n| n == stmt[:column] ? stmt[:new_name] : n } if c[:columns] }
        t[:constraints] = schema_rename_expression(t[:constraints], stmt[:column], stmt[:new_name])
        t[:indexes] = schema_rename_expression(t[:indexes], stmt[:column], stmt[:new_name])
        t[:indexes].each { |index| index[:columns]&.map! { |name| name == stmt[:column] ? stmt[:new_name] : name } }
        @tables.transform_values! do |other|
          constraints = other[:constraints].map do |constraint|
            if constraint[:kind] == :foreign_key && constraint[:table] == t[:name] && constraint[:ref_columns]
              constraint.merge(ref_columns: constraint[:ref_columns].map { |name| name == stmt[:column] ? stmt[:new_name] : name })
            else
              constraint
            end
          end
          other.merge(constraints: constraints)
        end
      when :rename_constraint
        constraint = t[:constraints].find { |c| c[:name] == stmt[:column] && c[:kind] != :primary_key }
        raise Error.new('42704', 'constraint does not exist') unless constraint
        raise Error.new('42710', 'constraint already exists') if t[:constraints].any? { |item| item[:name] == stmt[:new_name] }
        constraint[:name] = stmt[:new_name]
      else
        (stmt[:actions] || []).each do |action|
          case action[:kind]
          when :add_column
            column = action[:column].dup
            if t[:columns].any? { |c| c[:name] == column[:name] }
              next if action[:if_not_exists]
              raise Error.new('42701', 'duplicate column')
            end
            scan.call
            serial = Sequences::SERIAL_TYPES[column[:type]]
            identity = column[:identity]
            column[:type] = Types.normalize(serial || column[:type])
            validate_type(column[:type], column[:type_mod])
            if serial || identity
              options = identity ? identity[:options] : {}
              name = create_owned_sequence(t, column, t[:columns].length, options, reserved: [column[:unique_name]].compact)
              column[:default] = identity_default(name)
              column[:nullable] = false
              column[:identity] = identity[:mode] if identity
            end
            expression = column[:default] && compile(column[:default], [])
            t[:rows].each { |row| row << assign(expression && evaluate(expression, []), column, expression) }
            t[:columns] << column
            if column[:primary_key]
              raise Error.new('42P16', 'multiple primary keys') if t[:constraints].any? { |constraint| constraint[:kind] == :primary_key }
              column[:nullable] = false
              t[:constraints] << { kind: :primary_key, columns: [column[:name]], name: "#{t[:name]}_pkey" }
            end
            t[:constraints] << { kind: :unique, columns: [column[:name]], name: column[:unique_name] || "#{t[:name]}_#{column[:name]}_key" } if column[:unique]
            t[:constraints].concat(column[:checks] || [])
            t[:constraints] << column[:references].merge(kind: :foreign_key, columns: [column[:name]]) if column[:references]
          when :drop_column
            index = t[:columns].index { |c| c[:name] == action[:column] }
            unless index
              next if action[:if_exists]
              raise Error.new('42703', 'column does not exist')
            end
            raise Error.new('0A000', 'dropping a primary-key column is not supported') if t[:constraints].any? { |constraint| constraint[:kind] == :primary_key && constraint[:columns].include?(action[:column]) }
            dependent = t[:constraints].select { |constraint| constraint[:kind] == :exclude && schema_uses_column?(constraint[:where], action[:column]) }
            raise Error.new('2BP01', 'exclusion predicate depends on column') if !dependent.empty? && !action[:cascade]
            t[:constraints] -= dependent
            scan.call
            drop_incoming_foreign_keys(t, [action[:column]], action[:cascade])
            t[:columns].delete_at(index)
            t[:rows].each { |row| row.delete_at(index) }
            t[:constraints].reject! { |c| c[:columns]&.include?(action[:column]) }
            t[:constraints].reject! { |constraint| schema_uses_column?(constraint[:expr], action[:column]) || schema_uses_column?(constraint[:elements], action[:column]) }
            t[:indexes].reject! { |entry| schema_uses_column?(entry, action[:column]) }
            @sequences = @sequences.filter_map do |name, sequence|
              owner = sequence[:owned_by]
              next if owner == [t[:name], index]
              sequence = sequence.merge(owned_by: [t[:name], owner.last - 1]) if owner && owner.first == t[:name] && owner.last > index
              [name, sequence]
            end.to_h
          when :add_constraint
            scan.call
            constraint = action[:constraint].dup
            rekey = true if constraint[:kind] == :primary_key
            raise Error.new('42710', 'duplicate constraint') if constraint[:name] && t[:constraints].any? { |c| c[:name] == constraint[:name] }
            raise Error.new('42P07', 'relation already exists') if %i[unique exclude].include?(constraint[:kind]) && constraint[:name] && relation_exists?(constraint[:name])
            raise Error.new('42P16', 'multiple primary keys') if constraint[:kind] == :primary_key && t[:constraints].any? { |c| c[:kind] == :primary_key }
            (constraint[:columns] || []).each do |name|
              column = t[:columns].find { |item| item[:name] == name }
              raise Error.new('42703', 'constraint column does not exist') unless column
              column[:nullable] = false if constraint[:kind] == :primary_key
            end
            if constraint[:kind] == :foreign_key && constraint[:ref_columns] && constraint[:ref_columns].uniq.length != constraint[:ref_columns].length
              raise Error.new('42701', 'referenced column appears more than once')
            end
            if constraint[:kind] == :exclude
              names = constraint[:elements].map { |element| element[:expr][:name] }
              raise Error.new('42701', 'exclusion column appears more than once') unless names.uniq.length == names.length
              constraint[:elements].each do |element|
                raise Error.new('0A000', 'unsupported exclusion operator') unless %w[= && @> <@].include?(element[:op])
                type = compile(element[:expr], table_columns(t)).type
                raise Error.new('42704', 'no exclusion operator class') if element[:op] == '=' && Types::RANGES.key?(type)
              end
            end
            t[:constraints] << constraint
            name_schema_constraints(t)
          when :drop_constraint, :drop_primary_key
            found = t[:constraints].find { |c| action[:kind] == :drop_primary_key ? c[:kind] == :primary_key : c[:name] == action[:name] }
            raise Error.new('42704', 'constraint does not exist') unless found || action[:if_exists]
            drop_incoming_foreign_keys(t, found[:columns], action[:cascade]) if found && %i[primary_key unique].include?(found[:kind])
            scan.call if found && found[:kind] == :primary_key
            t[:constraints].delete(found)
            t[:indexes].reject! { |index| index[:name] == found[:name] } if found && %i[unique exclude].include?(found[:kind])
          when :add_identity, :drop_identity, :set_generated, :set_identity_options, :alter_identity
            apply_identity_action(t, action)
          when :set_default, :drop_default, :set_not_null, :drop_not_null, :set_type
            index = t[:columns].index { |c| c[:name] == action[:column] }
            raise Error.new('42703', 'column does not exist') unless index
            column = t[:columns][index]
            if column[:identity] && %i[set_default drop_default drop_not_null].include?(action[:kind])
              raise Error.new('42601', 'identity column requires DROP IDENTITY first')
            end
            case action[:kind]
            when :set_default
              expression = compile(action[:expr], [])
              assign(expression.value, column.merge(nullable: true), expression) if expression.kind == :literal
              column[:default] = action[:expr]
            when :drop_default then column.delete(:default)
            when :set_not_null
              scan.call
              raise Error.new('23502', 'NULL in column') if t[:rows].any? { |r| r[index].nil? }
              column[:nullable] = false
            when :drop_not_null
              raise Error.new('42P16', 'primary key cannot be nullable') if t[:constraints].any? { |c| c[:kind] == :primary_key && c[:columns].include?(column[:name]) }
              column[:nullable] = true
            when :set_type
              scan.call
              retype = true
              expr = action[:using] ? compile(action[:using], table_columns(t)) : nil
              old_type = column[:type]
              column[:type] = Types.normalize(action[:type])
              column[:type_mod] = action[:type_mod]
              validate_type(column[:type], column[:type_mod])
              if %w[json jsonpath].include?(column[:type]) && t[:constraints].any? { |constraint| %i[primary_key unique].include?(constraint[:kind]) && constraint[:columns].include?(column[:name]) }
                raise Error.new('0A000', 'type is not keyable')
              end
              validate_schema_foreign_types(t)
              retype_identity_sequence(column, old_type) if column[:identity]
              t[:rows].each { |row| row[index] = assign(expr ? evaluate(expr, row) : row[index], column, expr) }
            end
          else raise Error.new('0A000', "ALTER action #{action[:kind]} is not implemented")
          end
        end
      end
      validate_definition(t)
      constraints = if stmt[:action]
                      []
                    elsif retype
                      t[:constraints]
                    else
                      t[:constraints].reject { |constraint| prior_constraints.any? { |prior| prior.equal?(constraint) } }
                    end
      validate_alter_constraints(t, constraints)
      if retype || rekey
        t[:indexes].each do |index|
          charge(1 + t[:rows].length) unless t[:rows].empty?
          columns = table_columns(t)
          expressions = (index[:elements] || []).map { |element| compile(element[:expr], columns) }
          predicate = compile_filter(index[:where], columns)
          t[:rows].each do |row|
            next if predicate && evaluate(predicate, row) != true
            expressions.each { |expression| evaluate(expression, row) }
          end
        end
      end
      @tables[t[:name]] = t
      @dirty = true
      Result.new
    end

    private

    def validate_alter_constraints(table, constraints)
      columns = table_columns(table)
      constraints.each do |constraint|
        if constraint[:kind] == :exclude
          predicate = compile_filter(constraint[:where], columns)
          elements = constraint[:elements].map { |element| [compile(element[:expr], columns), element[:op]] }
          keys = []
          table[:rows].each do |row|
            next if predicate && evaluate(predicate, row) != true
            key = elements.map { |expression, _| evaluate(expression, row) }
            keys.each do |other|
              charge
              conflict = elements.each_index.all? { |index| Types.binary(elements[index][1], key[index], other[index], type: 'boolean') == true }
              raise Error.new('23P01', 'exclusion constraint violation') if conflict
            end
            keys << key
          end
        else
          if constraint[:kind] == :foreign_key && constraint[:table] == table[:name]
            local = constraint[:columns].map { |name| columns.index { |column| column[:name] == name } }
            remote = constraint[:ref_columns].map { |name| columns.index { |column| column[:name] == name } }
            table[:rows].each do |row|
              key = local.map { |index| row[index] }
              next if key.any?(&:nil?)
              table[:rows].each do |candidate|
                charge
                break if remote.each_with_index.all? { |index, position| !candidate[index].nil? && Types.compare(key[position], candidate[index]).zero? }
              end
            end
          end
          validate_rows(table.merge(constraints: [constraint], indexes: []), table[:rows])
        end
      end
    end

    def schema_rename_expression(node, old_name, new_name)
      case node
      when Array then node.map { |child| schema_rename_expression(child, old_name, new_name) }
      when Hash
        return node.merge(name: new_name, quoted: true) if node[:kind] == :column && node[:name] == old_name
        node.transform_values { |child| schema_rename_expression(child, old_name, new_name) }
      else node
      end
    end

    def name_schema_constraints(table)
      used = table[:constraints].filter_map { |constraint| constraint[:name] }
      table[:constraints].each do |constraint|
        next if constraint[:name]
        suffix = { primary_key: 'pkey', unique: 'key', check: 'check', foreign_key: 'fkey', exclude: 'excl' }.fetch(constraint[:kind])
        columns = constraint[:columns] || (constraint[:elements] || []).map { |element| element[:expr][:name] }
        columns = [] if constraint[:kind] == :primary_key
        base = ([table[:name]] + columns + [suffix]).join('_')
        name, n = base, 0
        while used.include?(name)
          n += 1
          name = "#{base}#{n}"
        end
        constraint[:name] = name
        used << name
      end
    end

    def drop_incoming_foreign_keys(table, columns, cascade)
      @tables.transform_values! do |other|
        constraints = other[:name] == table[:name] ? table[:constraints] : other[:constraints]
        dependencies = constraints.select do |constraint|
          next false unless constraint[:kind] == :foreign_key && constraint[:table] == table[:name]
          refs = constraint[:ref_columns] || table[:constraints].find { |item| item[:kind] == :primary_key }&.dig(:columns) || []
          (refs & columns).any?
        end
        raise Error.new('2BP01', 'foreign key depends on column or key') unless dependencies.empty? || cascade
        if other[:name] == table[:name]
          table[:constraints] -= dependencies
          other
        else
          other.merge(constraints: constraints - dependencies)
        end
      end
    end

    def validate_schema_foreign_types(table)
      (@tables.values.reject { |item| item[:name] == table[:name] } + [table]).each do |child|
        child[:constraints].each do |constraint|
          next unless constraint[:kind] == :foreign_key && (child[:name] == table[:name] || constraint[:table] == table[:name])
          parent = constraint[:table] == table[:name] ? table : @tables[constraint[:table]]
          next unless parent
          names = constraint[:ref_columns] || parent[:constraints].find { |item| item[:kind] == :primary_key }&.dig(:columns) || []
          constraint[:columns].zip(names).each do |local, remote|
            a = child[:columns].find { |column| column[:name] == local }
            b = parent[:columns].find { |column| column[:name] == remote }
            compatible = a && b && (a[:type] == b[:type] || Types::INTEGER_BITS.key?(a[:type]) && (Types::INTEGER_BITS.key?(b[:type]) || b[:type] == 'decimal') || %w[f32 f64].include?(a[:type]) && %w[f32 f64].include?(b[:type]))
            raise Error.new('42804', 'foreign key column types do not match') unless compatible
          end
        end
      end
    end

    def schema_uses_column?(node, name)
      case node
      when Array then node.any? { |child| schema_uses_column?(child, name) }
      when Hash
        node[:kind] == :column && node[:name] == name || node.values.any? { |child| schema_uses_column?(child, name) }
      else false
      end
    end

    def identity_default(name)
      { kind: :call, name: 'nextval', args: [{ kind: :literal, type: 'text', value: name }] }
    end

    def create_owned_sequence(table, column, ordinal, options, reserved: [])
      raise Error.new('22023', 'identity requires an integer column') unless Types::INTEGER_BITS.key?(column[:type])
      raise Error.new('42601', 'identity sequence options do not accept AS') if options.key?(:type)
      base = object_name(table[:name], column[:name], 'seq')
      name, suffix = options[:name] || base, 0
      if options[:name]
        raise Error.new('42939', 'reserved sequence name') if name.start_with?('jed_')
        raise Error.new('42P07', 'relation already exists') if relation_exists?(name)
      else
        while relation_exists?(name) || reserved.include?(name)
          suffix += 1
          name = object_name(table[:name], column[:name], "seq#{suffix}")
        end
      end
      definition = sequence_definition(options.reject { |key, _| key == :name }.merge(type: column[:type]))
      @sequences[name] = definition.merge(owned_by: [table[:name], ordinal])
      name
    end

    def apply_identity_action(table, action)
      index = table[:columns].index { |column| column[:name] == action[:column] }
      raise Error.new('42703', 'column does not exist') unless index
      column = table[:columns][index]
      if action[:kind] == :add_identity
        definition = action[:identity]
        name = create_owned_sequence(table, column, index, definition[:options])
        if column[:nullable] != false || column[:identity] || column[:default]
          raise Error.new('55000', 'column must be NOT NULL, without a default or identity')
        end
        column[:identity] = definition[:mode]
        column[:default] = identity_default(name)
      elsif action[:kind] == :drop_identity
        unless column[:identity]
          return if action[:if_exists]
          raise Error.new('55000', 'column is not an identity column')
        end
        name = column[:default][:args].first[:value]
        @sequences.delete(name)
        @sequence_session&.delete(name)
        column.delete(:default)
        column.delete(:identity)
      else
        raise Error.new('55000', 'column is not an identity column') unless column[:identity]
        actions = action[:kind] == :alter_identity ? action[:actions] : [action]
        options = {}
        actions.each do |item|
          if item[:kind] == :set_generated
            column[:identity] = item[:mode]
          elsif item[:kind] == :set_identity_options
            options.merge!(item[:options])
          else
            raise Error.new('42601', 'invalid identity action')
          end
        end
        unless options.empty?
          name = column[:default][:args].first[:value]
          @sequences[name] = sequence_definition(options.merge(type: column[:type]), @sequences.fetch(name))
        end
      end
    end

    def retype_identity_sequence(column, old_type)
      bits = Types::INTEGER_BITS[column[:type]]
      raise Error.new('22023', 'identity requires an integer column') unless bits
      old_bits = Types::INTEGER_BITS[old_type]
      name = column[:default][:args].first[:value]
      sequence = @sequences.fetch(name)
      options = { type: column[:type] }
      options[:maxvalue] = (1 << (bits - 1)) - 1 if sequence[:max_value] == (1 << (old_bits - 1)) - 1
      options[:minvalue] = -(1 << (bits - 1)) if sequence[:min_value] == -(1 << (old_bits - 1))
      @sequences[name] = sequence_definition(options, sequence)
    end
  end
  Engine.prepend(SchemaChanges)
end
