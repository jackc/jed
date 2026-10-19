# frozen_string_literal: true

require_relative 'key_reader'
require_relative 'pending_rows'

module JedNative
  # Repeatable forward/reverse cursors avoid retaining one token per matched row.
  class PagedScanOrdinals
    include Enumerable
    def initialize(&source) = (@source = source)
    def each(&block)
      return enum_for(:each) unless block
      @source.call(false).each { |_, row| block.call(row) }
    end
    def reverse_each(&block)
      return enum_for(:reverse_each) unless block
      @source.call(true).each { |_, row| block.call(row) }
    end
    def length = count
  end

  class PagedIntegerLookup
    def initialize(rows, codec, type)
      @rows, @codec, @bytes = rows, codec, Types::INTEGER_BITS.fetch(type) / 8
      @minimum, upper = Types::INTEGER_BOUNDS.fetch(type)
      @maximum = upper - 1
    end
    def [](key)
      return nil unless key.is_a?(Integer) && key.between?(@minimum, @maximum)
      @rows.point(@codec.encode_int(@bytes, key))
    end
  end

  # Storage cursors retain ordinal compatibility with the in-memory executor.
  # File rows are views; a pending mutation still owns its ordinary Ruby arrays.
  module PagedAccess
    def refresh_primary_indexes
      paged, resident = @tables.partition { |_, table| table[:rows].is_a?(PagedRows) }.map(&:to_h)
      @tables = resident
      super
      paged.each do |name, table|
        signature = [table[:rows].object_id, table[:columns].object_id, table[:indexes].object_id, table[:constraints].object_id]
        if table[:native_paged_signature] == signature
          @tables[name] = table
          next
        end
        table = build_missing_paged_indexes(table)
        signature[2] = table[:indexes].object_id
        rows = table[:rows]
        primary = table[:constraints].find { |constraint| constraint[:kind] == :primary_key }
        ordinals = primary ? primary[:columns].map { |column| table[:columns].index { |item| item[:name] == column } } : []
        types = ordinals.map { |ordinal| table[:columns][ordinal][:type] }
        codec = statistics_codec
        metadata = rows.profile.merge(columns: ordinals, types: types,
          collations: ordinals.map { |ordinal| table[:columns][ordinal][:collation] == 'C' ? nil : table[:columns][ordinal][:collation] })
        ordinal = ordinals.first if ordinals.length == 1 && Types::INTEGER_BITS.key?(types.first)
        secondary, entries, auxiliary = {}, {}, {}
        table[:indexes].each do |index|
          next if index[:unparsed]
          if index[:method] == 'gist'
            details = secondary_metadata(table, index).merge(index[:storage_gist_profile])
            auxiliary[index[:name]] = details.merge(row_positions: PagedRowPositions.new(rows))
            next
          end
          raw = index.dig(:storage_tree_profile, :paged_rows)
          next unless raw
          details = secondary_metadata(table, index).merge(raw.profile, physical_tree: raw.tree)
          if index[:method] == 'gin'
            auxiliary[index[:name]] = details.merge(entries: raw.entries, postings: PagedPostings.new(raw, rows),
                                                     row_positions: PagedRowPositions.new(rows))
            next
          end
          secondary[index[:name]] = details
          lookup = lambda do |key|
            offset = 0
            details[:types].each_with_index do |type, slot|
              desc, first = details[:orders][slot]
              present, absent = codec.ordered_slot_tags(desc, first)
              tag = key.getbyte(offset)
              offset += 1
              if tag == present
                offset = codec.skip_key_body(type, key, offset, details[:collations][slot], desc)
              elsif tag != absent
                raise Error.new('XX001', 'invalid index slot tag')
              end
            end
            rows.point(key.byteslice(offset..)) || raise(Error.new('XX001', 'index entry references absent row'))
          end
          details[:row_lookup] = lookup
          getter = ->(position) { key = raw.entries[position][0]; [key, lookup.call(key)] }
          entries[index[:name]] = PageSequence.new(-> { raw.entries.length }, getter,
            ->(&block) { raw.each_entry { |key, _| block.call([key, lookup.call(key)]) } })
        end
        @tables[name] = table.merge(native_paged_signature: signature, native_paged_rows: rows, native_primary_ordinal: ordinal,
          native_primary_index: ordinal && PagedIntegerLookup.new(rows, codec, types.first),
          native_primary_sorted: nil, native_primary_metadata: metadata, native_primary_entries: rows.entries,
          native_primary_codec: codec, native_primary_tree: rows.tree, native_row_positions: nil,
          native_secondary_metadata: secondary, native_secondary_indexes: entries, native_secondary_codec: codec,
          native_auxiliary_indexes: auxiliary, native_auxiliary_codec: codec)
      end
      @native_select_plans&.delete_if { |_, plan| plan.source && !@tables[plan.source[:name]].equal?(plan.table) }
      @native_read_plans&.delete_if { |_, plan| !@tables[plan.table[:name]].equal?(plan.table) }
    rescue StandardError
      @tables = resident.merge(paged) if defined?(resident) && resident
      raise
    end

    def primary_scan_units(table, row_ordinals = nil, columns = nil)
      return super unless table[:rows].is_a?(PagedRows)
      table[:rows].scan_units(row_ordinals, columns)
    end

    def paged_scan_ordinals(table, index_name = nil, lower: nil, upper: nil)
      PagedScanOrdinals.new do |reverse|
        storage_scan_entries(table, index_name, lower: lower, upper: upper, reverse: reverse)
      end
    end

    def storage_scan_entries(table, index_name = nil, lower: nil, upper: nil, reverse: false)
      entries = index_name ? table[:native_secondary_indexes].fetch(index_name) : table[:native_primary_entries]
      metadata = index_name ? table[:native_secondary_metadata].fetch(index_name) : table[:native_primary_metadata]
      Enumerator.new do |output|
        walk = lambda do |node|
          next unless node
          if node[:children].empty?
            keys = node[:keys]
            first = lower ? keys.bsearch_index { |key| key >= lower } || keys.length : 0
            last = upper ? keys.bsearch_index { |key| key >= upper } || keys.length : keys.length
            slots = reverse ? (last - 1).downto(first) : (first...last).each
            if metadata[:paged_rows]
              rows = metadata[:paged_rows]
              slots.each do |slot|
                row = index_name ? metadata[:row_lookup].call(keys[slot]) : rows.row_at(node, slot)
                output << [keys[slot], row]
              end
            else
              offset = entries.bsearch_index { |key, _| key >= keys.first } if first < last
              slots.each { |slot| output << entries[offset + slot] }
            end
          else
            first = lower ? node[:keys].bsearch_index { |key| key > lower } || node[:keys].length : 0
            last = upper ? node[:keys].bsearch_index { |key| key >= upper } || node[:keys].length : node[:children].length - 1
            children = reverse ? last.downto(first) : (first..last).each
            children.each { |index| walk.call(node[:children][index]) }
          end
        end
        walk.call(metadata[:tree])
      end
    end

    def storage_stream_rows(table, lower: nil, upper: nil, reverse: false)
      # Cost counts every structural page and touched payload in the bound,
      # even when LIMIT stops before reaching its last leaf. The page block is
      # paid before pulling rows; physical row/value reads remain lazy.
      charge(primary_page_count(table, lower, upper))
      ordinals = paged_scan_ordinals(table, lower: lower, upper: upper) if lower || upper
      charge_scan_payload(table, ordinals)
      Enumerator.new do |output|
        storage_scan_entries(table, lower: lower, upper: upper, reverse: reverse).each do |_, ordinal|
          # A suspended cursor shares its session's lifetime budget. Recheck
          # it before resolving a row when another query spent that budget.
          charge(0)
          output << storage_row(table, ordinal)
        end
      end
    end

    def storage_row(table, ordinal) = table[:rows][ordinal]
    def storage_tree_page_count(table, index_name = nil, lower: nil, upper: nil)
      index_name ? secondary_page_count(table, index_name, lower, upper) : primary_page_count(table, lower, upper)
    end

    def dispatch(statement)
      original = nil
      if statement[:kind] == :reindex
        reindex_targets(statement).each { |table, _| materialize_paged_table(table) }
      end
      if %i[truncate alter_table reindex].include?(statement[:kind])
        table = @tables[statement[:table] || statement[:name]]
        original = table if table && table[:rows].is_a?(PagedRows) && %i[insert update delete].include?(statement[:kind])
        materialize_paged_table(table) if table
      end
      result = super
      if original && result.rows_affected == 0
        current = @tables[original[:name]]
        @tables[original[:name]] = original if current && current[:rows].equal?(current[:storage_tree_rows])
      end
      result
    end

    def materialize_paged_table(table)
      return unless table[:rows].is_a?(PagedRows)
      raise Error.new('XX001', 'persisted row count does not match tree') if table[:rows].actual_count != table[:rows].length
      rows = table[:rows].map(&:to_a)
      changed = table.merge(rows: rows, rowids: table[:rowids].to_a, storage_tree_rows: rows)
      changed[:storage_rows] = rows if table[:storage_rows].equal?(table[:rows])
      @tables[table[:name]] = changed
      refresh_primary_indexes
    end

    # CREATE INDEX owns new index keys, not copies of every table row and its
    # primary record plan. The resulting tree also serves queries and later
    # writes in the same transaction before it receives physical page numbers.
    def build_missing_paged_indexes(table)
      missing = table[:indexes].any? do |index|
        !index[:unparsed] && !(index[:method] == 'gist' ? index[:storage_gist_profile] : index[:storage_tree_profile])
      end
      return table unless missing

      rows = table[:rows]
      raise Error.new('XX001', 'persisted row count does not match tree') unless rows.actual_count == rows.length
      codec = statistics_codec
      stored = { columns: table[:columns], rows: rows.raw || rows }
      capacity = storage_page_size(table) - FileFormat::PAGE_HEADER
      definitions = table[:indexes].to_h { |index| [index[:name], export_index(table, index)] }
      indexes = table[:indexes].map do |index|
        next index if index[:unparsed]
        definition = definitions.fetch(index[:name])
        if index[:method] == 'gist'
          next index if index[:storage_gist_profile]
          operations = codec.gist_opclasses_for(stored, definition)
          entries = codec.table_entries(stored).filter_map do |key, row|
            next if definition[:cols].any? { |column| row[column].nil? } || !codec.index_admits?(definition, row)
            [codec.gist_leaf_bound(operations, row, definition[:cols]), key]
          end
          tree = entries.empty? ? nil : codec.gist_build(operations, entries)
          count = height = 0
          visit = lambda do |node, depth|
            next unless node
            count += 1
            height = [height, depth].max
            node[:children].each { |_, child| visit.call(child, depth + 1) } unless node[:leaf]
          end
          visit.call(tree, 1)
          index.merge(storage_gist_profile: { tree: tree, node_count: count, height: height, opclasses: operations })
        else
          next index if index[:storage_tree_profile]
          entries = index[:method] == 'gin' ? codec.gin_index_entries(stored, definition) : codec.index_entries(stored, definition)
          tree = codec.build_tree(entries, capacity)
          raw = PagedRows.new(rows.pool, 0, [], nil, tree: tree)
          index.merge(storage_tree_profile: raw.profile)
        end
      end
      table.merge(indexes: indexes)
    end
  end
  Engine.prepend(PagedAccess)
end
