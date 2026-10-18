# frozen_string_literal: true

module JedNative
  module PagedDeltas
    def refresh_primary_indexes
      @tables.each do |name, table|
        @tables[name] = publish_pending_rows(table) if table[:rows].is_a?(PendingRows)
      end
      super
    end

    private

    def publish_pending_rows(table)
      pending = table[:rows]
      base = pending.base_table
      codec = statistics_codec
      capacity = storage_page_size(table) - FileFormat::PAGE_HEADER
      cost, lifetime, maximum, lifetime_maximum = @cost, @lifetime_cost, @max_cost, @lifetime_max_cost
      @max_cost = @lifetime_max_cost = 0
      changes = pending.each_change.map do |before, after, rowid|
        [before, after, before&.storage_key, after && query_memory_storage_key(table, after, rowid, codec)]
      end
      tree = base[:native_primary_tree]
      changes.each { |_, _, old_key, key| tree = codec.delete_tree_key(tree, old_key, capacity) if old_key && old_key != key }
      shape = { columns: table[:columns] }
      changes.each do |_, after, _, key|
        next unless after
        values = after.each_with_index.map { |value, index| statistics_value(table[:columns][index][:type], value) }
        forms, comps, size = codec.plan_record(shape, key, values, capacity)
        record = { key: key, row: values, table: shape, forms: forms, comps: comps, size: size }
        tree = pending_tree_put(codec, tree, key, record, capacity)
      end
      raise Error.new('XX001', 'persisted row count does not match empty tree') unless tree.nil? == pending.length.zero?
      raw = PagedRows.new(base[:rows].pool, 0, table[:columns], pending.length, tree: tree)
      rows = raw.converted { |type, value| from_storage_value(type, value) }
      indexes = table[:indexes].map do |index|
        name = index[:name]
        metadata = base[:native_secondary_metadata]&.[](name) || base[:native_auxiliary_indexes]&.[](name)
        next index unless metadata
        if metadata[:kind] == 'gist'
          updated = pending_gist_tree(base, metadata, changes, codec)
          index.merge(storage_gist_profile: updated)
        else
          updated = metadata[:physical_tree] || metadata[:tree]
          if metadata[:kind] == 'gin'
            updated = pending_gin_tree(base, name, metadata, changes, codec, capacity)
          else
            changed_keys = changes.map do |before, after, old_key, new_key|
              [pending_secondary_key(base, name, metadata, before, old_key), pending_secondary_key(base, name, metadata, after, new_key)]
            end
            changed_keys.each { |old, current| updated = codec.delete_tree_key(updated, old, capacity) if old && old != current }
            changed_keys.each do |old, current|
              next unless current && old != current
              record = { key: current, row: [], table: { columns: [] }, forms: [], comps: [], size: current.bytesize }
              updated = pending_tree_put(codec, updated, current, record, capacity)
            end
          end
          old_rows = metadata[:paged_rows]
          raw_index = PagedRows.new(old_rows.pool, 0, [], nil, tree: updated)
          index.merge(storage_tree_profile: raw_index.profile)
        end
      end
      rowids = base[:native_primary_metadata][:columns].empty? ? rows.rowids(codec) : PageSequence.new(rows.length, ->(index) { index })
      table.merge(rows: rows, rowids: rowids, indexes: indexes,
        native_primary_tree: rows.tree, storage_tree_rows: rows, storage_rows: rows,
        storage_tree_profile: rows.profile, storage_keys: rows.keys)
    ensure
      @cost, @lifetime_cost, @max_cost, @lifetime_max_cost = cost, lifetime, maximum, lifetime_maximum
    end

    def pending_tree_put(codec, tree, key, record, capacity)
      singleton = { keys: [key], recs: [record], children: [] }
      raise Error.new('0A000', 'record exceeds page capacity after spilling') if codec.node_payload(singleton) > capacity
      return singleton unless tree
      result = codec.put_tree_key(tree, key, record, capacity)
      result[0] == :split ? { keys: [result[2]], recs: [], children: [result[1], result[3]] } : result[1]
    end

    def pending_secondary_key(table, name, metadata, row, key)
      return nil unless row
      return nil if metadata[:predicate] && evaluate(metadata[:predicate], row) != true
      values = metadata[:expressions].map { |expression| evaluate(expression, row) }
      secondary_prefix_key(table, name, values) + key
    end

    def pending_gin_tree(table, name, metadata, changes, codec, capacity)
      tree = metadata[:physical_tree] || metadata[:tree]
      runs = {}
      edits = changes.map do |before, after, old_key, key|
        terms = [before, after].map do |row|
          row ? metadata[:columns].each_with_index.flat_map { |column, position| gin_terms(table, name, position, row[column]) }.uniq.sort : []
        end
        [old_key, key, *terms]
      end
      load_runs = lambda do |term|
        runs[term] ||= metadata[:paged_rows].each_entry(lower: term, upper: prefix_successor(term)).map(&:first)
      end
      edits.sort_by { |old_key, _, _, _| old_key || ''.b }.each do |old_key, key, before, after|
        (old_key == key ? before - after : before).each do |term|
          tree = codec.gin_edit_posting(tree, load_runs.call(term), term, old_key, false, capacity)
        end
      end
      edits.each do |old_key, key, before, after|
        (old_key == key ? after - before : after).each do |term|
          tree = codec.gin_edit_posting(tree, load_runs.call(term), term, key, true, capacity)
        end
      end
      tree
    end

    def pending_gist_tree(table, metadata, changes, codec)
      # The canonical GiST shape is rebuilt from sorted bounds. Reuse its
      # resident bound inventory; do not fault the table's unchanged values.
      removed = changes.filter_map { |_, _, old, _| old }.to_h { |key| [key, true] }
      entries = []
      visit = lambda do |node|
        next unless node
        if node[:leaf]
          node[:entries].each { |entry| entries << entry unless removed[entry[1]] }
        else
          node[:children].each { |_, child| visit.call(child) }
        end
      end
      visit.call(metadata[:tree])
      operations = metadata[:opclasses]
      changes.each do |_, row, _, key|
        next unless row && metadata[:columns].none? { |column| row[column].nil? }
        next if metadata[:predicate] && evaluate(metadata[:predicate], row) != true
        values = row.each_with_index.map { |value, column| statistics_value(table[:columns][column][:type], value) }
        entries << [codec.gist_leaf_bound(operations, values, metadata[:columns]), key]
      end
      tree = entries.empty? ? nil : codec.gist_build(operations, entries)
      count = height = 0
      walk = lambda do |node, depth|
        next unless node
        count += 1
        height = [height, depth].max
        node[:children].each { |_, child| walk.call(child, depth + 1) } unless node[:leaf]
      end
      walk.call(tree, 1)
      { tree: tree, node_count: count, height: height, opclasses: operations }
    end
  end
  Engine.prepend(PagedDeltas)
end
