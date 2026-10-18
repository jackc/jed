# frozen_string_literal: true

module JedNative
  # Reverse FK searches are work, even when no child changes. Deferred checks
  # inspect the statement end state; generated actions gather before rewriting.
  module ForeignKeyWork
    def enforce_references(parent, changes, operation, visited = {})
      return super if changes.empty?
      if @reverse_check_statement != @foreign_key_checks.object_id
        @reverse_check_statement = @foreign_key_checks.object_id
        @reverse_checks = []
      end
      @tables.values.sort_by { |table| table[:name].b }.each do |child|
        child[:constraints].select { |constraint| constraint[:kind] == :foreign_key && constraint[:table] == parent[:name] }.sort_by { |constraint| constraint[:name].b }.each do |foreign|
          remote = foreign[:ref_columns] || parent[:constraints].find { |constraint| constraint[:kind] == :primary_key }[:columns]
          ordinals = remote.map { |name| parent[:columns].index { |column| column[:name] == name } }
          keys = changes.filter_map do |before, after|
            previous = ordinals.map { |index| before[index] }
            current = after && ordinals.map { |index| after[index] }
            previous unless current && fk_tuple_equal?(previous, current)
          end.uniq
          next if keys.empty?
          action = foreign[operation == :delete ? :on_delete : :on_update] || :no_action
          if %i[no_action restrict].include?(action)
            keys.each { |key| @reverse_checks << [child[:name], foreign[:name], parent[:name], remote, key] }
          else
            fk_gather_work(child, parent, foreign, remote, keys)
          end
        end
      end
      super
    end

    def flush_foreign_keys
      if @reverse_check_statement == @foreign_key_checks.object_id && @reverse_checks && !@reverse_checks.empty?
        refresh_primary_indexes
        @reverse_checks.each_with_index.sort_by { |(entry, index)| [entry[0].b, entry[1].b, index] }.each do |(child_name, constraint_name, parent_name, remote, key), _|
          child, parent = @tables[child_name], @tables[parent_name]
          next unless child && parent
          foreign = child[:constraints].find { |constraint| constraint[:name] == constraint_name }
          next unless foreign
          indices = remote.map { |name| parent[:columns].index { |column| column[:name] == name } }
          next if parent[:rows].any? { |row| fk_tuple_equal?(indices.map { |index| row[index] }, key) }
          fk_reverse_check(child, parent, foreign, remote, key)
        end
        @reverse_checks.clear
      end
      super
    end

    private

    def fk_tuple_equal?(left, right)
      !left.any?(&:nil?) && !right.any?(&:nil?) && left.zip(right).all? { |a, b| Types.compare(a, b).zero? }
    end

    def fk_access_path(child, parent, foreign, remote)
      locals = foreign[:columns].map { |name| child[:columns].index { |column| column[:name] == name } }
      width = locals.length
      collations_match = locals.each_with_index.all? do |ordinal, index|
        column = child[:columns][ordinal]
        next true unless collatable_type?(column[:type])
        other = parent[:columns].find { |entry| entry[:name] == remote[index] }
        collation = column[:collation] || 'C'
        collation == (other[:collation] || 'C') && !(child[:collation_skew] || []).include?(collation)
      end
      return [nil, locals] unless collations_match
      primary = child[:native_primary_metadata]
      if primary && primary[:columns].first(width).sort == locals.sort
        return [{ kind: :primary, columns: primary[:columns].first(width) }, locals]
      end
      (child[:native_secondary_metadata] || {}).sort_by { |name, _| name.b }.each do |name, metadata|
        next if metadata[:predicate]
        columns = metadata[:columns].first(width)
        next if columns.length != width || columns.any?(&:nil?) || columns.sort != locals.sort
        next unless metadata[:types].drop(width).all? { |type| statistics_codec.fixed_width(type) }
        return [{ kind: :secondary, name: name, columns: columns }, locals]
      end
      [nil, locals]
    end

    def fk_probe(child, path, locals, key)
      values = path[:columns].map do |ordinal|
        value = key[locals.index(ordinal)]
        return [0, []] if value.nil?
        begin
          converted = cast_value(value, child[:columns][ordinal][:type])
          return [0, []] unless Types.compare(value, converted).zero?
          converted
        rescue Error
          return [0, []]
        end
      end
      if path[:kind] == :primary
        prefix = primary_prefix_key(child, values)
        pages = primary_page_count(child, prefix, prefix_successor(prefix))
        entries = child[:native_primary_entries]
      else
        prefix = secondary_prefix_key(child, path[:name], values)
        pages = secondary_page_count(child, path[:name], prefix, prefix_successor(prefix))
        entries = child[:native_secondary_indexes][path[:name]]
      end
      upper = prefix_successor(prefix)
      if child[:rows].is_a?(PagedRows)
        name = path[:kind] == :secondary ? path[:name] : nil
        return [pages, storage_scan_entries(child, name, lower: prefix, upper: upper).map(&:last)]
      end
      first = entries.bsearch_index { |entry, _| entry >= prefix } || entries.length
      last = upper ? entries.bsearch_index { |entry, _| entry >= upper } || entries.length : entries.length
      [pages, entries[first...last].map(&:last)]
    end

    def fk_reverse_check(child, parent, foreign, remote, key)
      path, locals = fk_access_path(child, parent, foreign, remote)
      if path
        pages, ordinals = fk_probe(child, path, locals, key)
        charge(pages)
        raise Error.new('23503', 'referencing row still exists') unless ordinals.empty?
      else
        charge(full_scan_pages(child))
        charge_scan_payload(child, nil, locals)
        entries = child[:native_primary_entries] || child[:rows].each_index.map { |index| [nil, index] }
        entries.each do |_, ordinal|
          charge
          raise Error.new('23503', 'referencing row still exists') if fk_tuple_equal?(locals.map { |index| child[:rows][ordinal][index] }, key)
        end
      end
    end

    def fk_gather_work(child, parent, foreign, remote, keys)
      path, locals = fk_access_path(child, parent, foreign, remote)
      if path
        gathered = []
        keys.each do |key|
          pages, ordinals = fk_probe(child, path, locals, key)
          pages += ordinals.length * (child.dig(:native_primary_metadata, :height) || 1) if path[:kind] == :secondary
          charge(pages)
          charge_scan_payload(child, ordinals, locals)
          gathered.concat(ordinals)
        end
        gathered.uniq { |row| auxiliary_row_identity(row) }.each { charge }
      else
        charge(full_scan_pages(child))
        charge_scan_payload(child, nil, locals)
        child[:rows].each { charge }
      end
    end
  end
  Engine.prepend(ForeignKeyWork)
end
