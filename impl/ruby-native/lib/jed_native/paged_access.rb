# frozen_string_literal: true

require_relative 'key_reader'

module JedNative
  class PagedIntegerLookup
    def initialize(rows, codec, type)
      @rows, @codec, @bytes = rows, codec, Types::INTEGER_BITS.fetch(type) / 8
    end
    def [](key)
      return nil unless key.is_a?(Integer) && key.between?(-(1 << (@bytes * 8 - 1)), (1 << (@bytes * 8 - 1)) - 1)
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
        signature = [table[:rows], table[:columns].object_id, table[:indexes].object_id, table[:constraints].object_id]
        if table[:native_paged_signature] == signature
          @tables[name] = table
          next
        end
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
          getter = lambda do |position|
            key = raw.entries[position][0]
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
            [key, rows.ordinal_for_key(key.byteslice(offset..))]
          end
          entries[index[:name]] = PageSequence.new(-> { raw.entries.length }, getter)
        end
        @tables[name] = table.merge(native_paged_signature: signature, native_paged_rows: rows, native_primary_ordinal: ordinal,
          native_primary_index: ordinal && PagedIntegerLookup.new(rows, codec, types.first),
          native_primary_sorted: nil, native_primary_metadata: metadata, native_primary_entries: rows.entries,
          native_primary_codec: codec, native_primary_tree: rows.tree, native_row_positions: nil,
          native_secondary_metadata: secondary, native_secondary_indexes: entries, native_secondary_codec: codec,
          native_auxiliary_indexes: auxiliary, native_auxiliary_codec: codec)
      end
      @native_select_plans&.delete_if { |_, plan| plan.source && !@tables[plan.source[:name]].equal?(plan.table) }
    rescue StandardError
      @tables = resident.merge(paged) if defined?(resident) && resident
      raise
    end

    def primary_scan_units(table, row_ordinals = nil, columns = nil)
      return super unless table[:rows].is_a?(PagedRows)
      table[:rows].scan_units(row_ordinals, columns)
    end

    def storage_scan_entries(table, index_name = nil, lower: nil, upper: nil, reverse: false, charge_pages: false, prepaid_pages: 0)
      entries = index_name ? table[:native_secondary_indexes].fetch(index_name) : table[:native_primary_entries]
      metadata = index_name ? table[:native_secondary_metadata].fetch(index_name) : table[:native_primary_metadata]
      Enumerator.new do |output|
        walk = lambda do |node|
          next unless node
          if charge_pages
            prepaid_pages.positive? ? prepaid_pages -= 1 : charge
          end
          if node[:children].empty?
            keys = node[:keys]
            first = lower ? keys.bsearch_index { |key| key >= lower } || keys.length : 0
            last = upper ? keys.bsearch_index { |key| key >= upper } || keys.length : keys.length
            slots = reverse ? (last - 1).downto(first) : (first...last).each
            if node.is_a?(PagedLeaf)
              rows = metadata[:paged_rows]
              slots.each do |slot|
                ordinal = rows.ordinal_for_leaf_slot(node, slot)
                output << (index_name ? entries[ordinal] : [keys[slot], ordinal])
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
      initial = table[:native_primary_metadata][:height]
      charge(initial)
      Enumerator.new do |output|
        storage_scan_entries(table, lower: lower, upper: upper, reverse: reverse, charge_pages: true, prepaid_pages: initial).each do |_, ordinal|
          charge_scan_payload(table, [ordinal])
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
      if %i[insert update delete truncate alter_table create_index reindex analyze].include?(statement[:kind])
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
  end
  Engine.prepend(PagedAccess)
end
