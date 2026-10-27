# frozen_string_literal: true

module JedNative
  # Committed rows already satisfy their constraints. A sparse write compares
  # its pending keys with those immutable rows, without retaining all old keys.
  # Expression constraints keep their existing evaluation order in a bounded
  # spool; FK checks probe a master key or stream the referenced relation.
  module PagedConstraints
    def validate_rows(table, rows)
      return super unless rows.is_a?(PagedRows) || rows.is_a?(PendingRows)
      columns = table_columns(table)
      table[:constraints].each do |constraint|
        case constraint[:kind]
        when :primary_key, :unique
          paged_validate_unique(table, rows, constraint, columns)
        when :foreign_key
          if @checking_foreign_keys
            paged_validate_foreign(table, rows, constraint, columns)
          else
            @foreign_key_checks[table[:name]] = true
          end
        when :exclude
          paged_validate_exclusion(rows, constraint, columns)
        else
          super(table.merge(constraints: [constraint], indexes: []), rows)
        end
      end
      table[:indexes].each do |index|
        verify_host_dependencies(index[:host_deps], "index #{index[:name]}")
        next unless index[:unique]
        elements = index[:elements] || index[:columns].map { |name| { expr: { kind: :column, name: name } } }
        expressions = elements.map { |element| compile(element[:expr], columns) }
        predicate = compile_source_order_filter(index[:where], columns)
        paged_unique_state(rows, primary: false) do |row|
          next if predicate && evaluate(predicate, row) != true
          expressions.map { |expression| evaluate(expression, row) }
        end
      end
    end

    def paged_validate_unique(table, rows, constraint, columns)
      ordinals = constraint[:columns].map { |name| columns.index { |column| column[:name] == name } }
      primary = constraint[:kind] == :primary_key
      unless rows.is_a?(PendingRows)
        return paged_unique_state(rows, primary: primary) { |row| ordinals.map { |ordinal| row[ordinal] } }
      end
      seen = {}
      rows.changed_rows.each do |row|
        key = ordinals.map { |ordinal| row[ordinal] }
        if key.any?(&:nil?)
          raise Error.new('23502', 'NULL primary key') if primary
          next
        end
        encoded = paged_constraint_key(table, ordinals, key)
        raise Error.new('23505', 'duplicate key') if seen.key?(encoded)
        seen[encoded] = true
        if paged_key_exists?(rows.base_table, rows, ordinals, key, unchanged: true, index_name: primary ? nil : constraint[:name])
          raise Error.new('23505', 'duplicate key')
        end
      end
    end

    def paged_constraint_key(table, ordinals, key)
      codec = table[:native_primary_codec] || statistics_codec
      ordinals.each_with_index.map do |ordinal, index|
        column = table[:columns][ordinal]
        collation = column[:collation]
        codec.key_body(column[:type], statistics_value(column[:type], key[index]), collation == 'C' ? nil : collation)
      end.join.b
    end

    def paged_key_equal?(table, ordinals, row, key)
      values = ordinals.map { |ordinal| row[ordinal] }
      return false if values.any?(&:nil?)
      ordinals.each_with_index do |ordinal, index|
        type = table[:columns][ordinal][:type]
        next unless %w[i16 i32 i64 decimal f32 f64].include?(type)
        return false unless Types.compare(values[index], key[index]).zero?
      end
      paged_constraint_key(table, ordinals, values) == paged_constraint_key(table, ordinals, key)
    end

    def paged_key_exists?(table, rows, ordinals, key, unchanged: false, index_name: nil)
      base = rows.is_a?(PendingRows) ? rows.base_table : table
      pending = rows.is_a?(PendingRows) ? rows : nil
      if pending && !unchanged && pending.changed_rows.any? { |row| paged_key_equal?(table, ordinals, row, key) }
        return true
      end
      primary = base.dig(:native_primary_metadata, :columns)
      if primary == ordinals && base[:rows].is_a?(PagedRows)
        encoded = primary_prefix_key(base, key)
        return false if pending && pending.changes.key?(encoded)
        candidate = base[:rows].point(encoded)
        return candidate && paged_key_equal?(table, ordinals, candidate, key)
      end
      if index_name && base.dig(:native_secondary_metadata, index_name) && base[:rows].is_a?(PagedRows)
        prefix = secondary_prefix_key(base, index_name, key)
        storage_scan_entries(base, index_name, lower: prefix).each do |encoded, ordinal|
          break unless encoded.start_with?(prefix)
          row = ordinal.is_a?(Array) ? ordinal : storage_row(base, ordinal)
          next if pending && pending.changes.key?(row.storage_key)
          return true if paged_key_equal?(table, ordinals, row, key)
        end
        return false
      end
      candidates = pending ? pending.each_unchanged : rows
      candidates.any? { |row| paged_key_equal?(table, ordinals, row, key) }
    end

    def paged_unique_state(rows, primary:)
      state = SpillStateMap.new(self)
      rows.each do |row|
        key = yield row
        next unless key
        if key.any?(&:nil?)
          raise Error.new('23502', 'NULL primary key') if primary
          next
        end
        raise Error.new('23505', 'duplicate key') unless state.insert(sql_set_key(key))
      end
    ensure
      state&.close
    end

    def paged_validate_foreign(table, rows, constraint, columns)
      name = constraint[:table] || constraint[:ref_table] || constraint.dig(:references, :table)
      parent = name == table[:name] ? table.merge(rows: rows) : table(name)
      names = constraint[:ref_columns] || constraint[:reference_columns] || constraint.dig(:references, :columns)
      names ||= parent[:constraints].find { |item| item[:kind] == :primary_key }&.dig(:columns)
      raise Error.new('42830', 'no referenced key') unless names
      local = constraint[:columns].map { |name| columns.index { |column| column[:name] == name } }
      remote = names.map { |name| parent[:columns].index { |column| column[:name] == name } }
      index_name = parent[:constraints].find { |item| item[:kind] == :unique && item[:columns] == names }&.dig(:name)
      rows.each do |row|
        key = local.map { |ordinal| row[ordinal] }
        if key.any?(&:nil?)
          raise Error.new('23503', 'MATCH FULL does not allow mixed nulls') if constraint[:match] == :full && !key.all?(&:nil?)
          next
        end
        begin
          found = paged_key_exists?(parent, parent[:rows], remote, key, index_name: index_name)
        rescue Error => error
          raise unless %w[22003 22P02].include?(error.sqlstate)
          found = false
        end
        raise Error.new('23503', 'foreign key violation') unless found
      end
    end

    def paged_validate_exclusion(rows, constraint, columns)
      predicate = compile_source_order_filter(constraint[:where], columns)
      elements = constraint[:elements].map { |element| [compile(element[:expr], columns), element[:op]] }
      keys = RowSpool.new(self)
      rows.each do |row|
        next if predicate && evaluate(predicate, row) != true
        keys << elements.map { |expression, _| evaluate(expression, row) }
      end
      keys.each_with_index do |left, position|
        keys.each_with_index do |right, ordinal|
          break if ordinal >= position
          if elements.each_index.all? { |index| Types.binary(elements[index][1], left[index], right[index], type: 'boolean') == true }
            raise Error.new('23P01', 'exclusion constraint violation')
          end
        end
      end
    ensure
      keys&.close
    end
  end
  Engine.prepend(PagedConstraints)
end
