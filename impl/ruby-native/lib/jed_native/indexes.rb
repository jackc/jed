# frozen_string_literal: true

module JedNative
  # Native B-tree search inventory. Keys are the shared byte-order contract,
  # including nullable slots, direction, and the row storage-key suffix.
  module SecondaryIndexes
    def storage_page_size(table = nil)
      return 8192 if table && table[:temporary]
      return @attachments&.dig(table[:database], :storage)&.page_size || 8192 if table && table[:database]
      @storage&.page_size || @native_page_size || 8192
    end

    def refresh_primary_indexes
      super
      refresh_primary_pages
      refresh_secondary_indexes
      refresh_auxiliary_indexes
    end

    GIN_ELEMENT_TYPES = %w[i16 i32 i64 boolean uuid date timestamp timestamptz f32 f64 interval text bytea decimal].freeze
    GIST_SCALAR_TYPES = %w[i16 i32 i64 boolean uuid date timestamp timestamptz].freeze

    def create_index(statement)
      method = statement[:method] || 'btree'
      return super if method == 'btree'
      stored = table(statement[:table])
      raise Error.new('42704', "unknown access method #{method}") unless %w[gin gist].include?(method)
      elements = statement[:elements] || []
      elements.each do |element|
        node = element[:expr]
        raise Error.new('0A000', "#{method} expression keys are not supported") unless node[:kind] == :column
        column = stored[:columns].find { |item| item[:name] == node[:name] }
        raise Error.new('42703', "unknown column #{node[:name]}") unless column
        auxiliary_type_gate(method, column[:type])
        if element[:direction_explicit] || element[:direction] == :desc || element[:nulls]
          raise Error.new('0A000', "#{method} does not accept key ordering options")
        end
      end
      raise Error.new('0A000', "#{method} does not support unique indexes") if statement[:unique]
      if method == 'gin'
        raise Error.new('54011', 'an index may contain at most 32 keys') if elements.length > 32
      elsif elements.length != 1 || stored[:temporary]
        raise Error.new('0A000', 'standalone GiST requires one column and a persistent table')
      end
      raise Error.new('0A000', "partial #{method} indexes are not supported") if statement[:where]
      if statement[:database] && !%w[main temp].include?(statement[:database])
        raise Error.new('0A000', "#{method} indexes on attached databases are not supported")
      end
      unless statement[:name]
        base = "#{stored[:name]}_#{elements.map { |element| element[:expr][:name] }.join('_')}_idx"
        name, suffix = base, 0
        while relation_exists?(name)
          suffix += 1
          name = "#{base}#{suffix}"
        end
        statement = statement.merge(name: name)
      end
      if relation_exists?(statement[:name])
        return Result.new if statement[:if_not_exists]
        raise Error.new('42P07', 'index exists')
      end
      charge(full_scan_pages(stored) + stored[:rows].length) unless stored[:rows].empty?
      @tables[stored[:name]] = stored.merge(indexes: stored[:indexes] + [statement])
      @dirty = true
      Result.new
    end

    def refresh_primary_pages
      @tables.each do |name, stored|
        page_size = storage_page_size(stored)
        signature = [stored[:rows].object_id, stored[:columns].object_id, stored[:constraints].object_id, page_size]
        next if stored[:native_page_signature] == signature
        codec = statistics_codec
        names = stored[:constraints].find { |constraint| constraint[:kind] == :primary_key }&.dig(:columns) || []
        ordinals = names.map { |column| stored[:columns].index { |item| item[:name] == column } }
        types = ordinals.map { |ordinal| stored[:columns][ordinal][:type] }
        row_positions = stored[:rows].each_with_index.to_h { |row, index| [row.object_id, index] }
        rows = if stored[:storage_keys] && stored[:storage_tree_rows].equal?(stored[:rows])
          stored[:storage_keys].zip(stored[:rows])
        else
          statistics_storage_rows(stored, codec)
        end
        entries = rows.map { |key, row| [key.freeze, row_positions.fetch(row.object_id)].freeze }.freeze
        shape = { columns: stored[:columns] }
        cap = page_size - FileFormat::PAGE_HEADER
        scan_units = {}
        pairs = rows.map do |key, row|
            values = row.each_with_index.map { |value, index| statistics_value(stored[:columns][index][:type], value) }
            forms, comps, size = codec.plan_record(shape, key, values, cap)
            units = {}
            forms.each_with_index do |form, index|
              next if form == :inline
              payload = codec.value_payload(stored[:columns][index][:type], values[index])
              pages = case form
                      when :external then (payload.bytesize + cap - 1) / cap
                      when :external_comp then (comps[index].bytesize + cap - 1) / cap
                      else 0
                      end
              slabs = %i[inline_comp external_comp].include?(form) ? (payload.bytesize + cap - 1) / cap : 0
              units[index] = [pages, slabs].freeze unless pages.zero? && slabs.zero?
            end
            scan_units[row_positions.fetch(row.object_id)] = units.freeze unless units.empty?
            [key, { key: key, row: values, table: shape, forms: forms, comps: comps, size: size }]
        end
        tree = if stored[:storage_tree_profile] && stored[:storage_tree_rows].equal?(stored[:rows])
          codec.tree_from_profile(stored[:storage_tree_profile], pairs)
        else
          codec.patch_tree(stored[:native_primary_tree], pairs, cap)
        end
        profile = compact_tree_profile(tree).merge(scan_units: scan_units.freeze)
        metadata = profile.merge(columns: ordinals.freeze, types: types.freeze,
                                 collations: ordinals.map { |ordinal| stored[:columns][ordinal][:collation] == 'C' ? nil : stored[:columns][ordinal][:collation] }.freeze).freeze
        @tables[name] = stored.merge(native_page_signature: signature, native_primary_metadata: metadata,
                                    native_primary_entries: entries, native_primary_codec: codec, native_row_positions: row_positions.freeze,
                                    native_primary_tree: tree)
      end
    rescue RuntimeError, KeyError => error
      raise Error.new('54000', error.message)
    end

    def primary_prefix_key(table, values)
      metadata = table.fetch(:native_primary_metadata)
      codec = table.fetch(:native_primary_codec)
      raise Error.new('42601', 'too many primary-key values') if values.length > metadata[:types].length
      values.each_with_index.map do |value, index|
        raise Error.new('23502', 'NULL primary key') if value.nil?
        type = metadata[:types][index]
        codec.key_body(type, statistics_value(type, value), metadata[:collations][index])
      end.join.b
    end

    def primary_page_count(table, lower = nil, upper = nil)
      tree_page_count(table.fetch(:native_primary_metadata)[:tree], lower, upper)
    end

    # Overflow pages and decompression slabs for the selected rows/columns,
    # separate from B-tree traversal pages. Nil selects all rows or all columns.
    def primary_scan_units(table, row_ordinals = nil, columns = nil)
      units = table.fetch(:native_primary_metadata)[:scan_units] || {}
      return [0, 0] if units.empty?
      pages, slabs = 0, 0
      selected = row_ordinals ? row_ordinals.filter_map { |ordinal| units[ordinal] } : units.values
      selected.each do |row|
        row.each do |column, (extra_pages, extra_slabs)|
          next if columns && !columns.include?(column)
          pages += extra_pages
          slabs += extra_slabs
        end
      end
      [pages, slabs]
    end

    def refresh_secondary_indexes
      @tables.each do |name, stored|
        next if !stored.key?(:native_secondary_indexes) && stored[:indexes].empty? && stored[:constraints].none? { |constraint| constraint[:kind] == :unique }
        signature = [stored[:rows].object_id, stored[:columns].object_id, stored[:indexes].object_id,
                     stored[:constraints].object_id, @types.object_id]
        next if stored[:native_secondary_signature] == signature
        codec = statistics_codec
        metadata, indexes = build_secondary_indexes(stored, codec)
        @tables[name] = stored.merge(native_secondary_signature: signature, native_secondary_metadata: metadata.freeze,
                                    native_secondary_indexes: indexes.freeze, native_secondary_codec: codec)
      end
    rescue RuntimeError, KeyError => error
      raise Error.new('54000', error.message)
    end

    def secondary_prefix_key(table, name, values)
      metadata = table.fetch(:native_secondary_metadata).fetch(name)
      raise Error.new('42601', 'too many index key values') if values.length > metadata[:types].length
      codec = table.fetch(:native_secondary_codec)
      values.each_with_index.map do |value, index|
        type = metadata[:types][index]
        desc, first = metadata[:orders][index]
        present, absent = codec.ordered_slot_tags(desc, first)
        if value.nil?
          [absent].pack('C')
        else
          body = codec.key_body(type, statistics_value(type, value), metadata[:collations][index])
          body = body.bytes.map { |byte| byte ^ 255 }.pack('C*') if desc
          [present].pack('C') + body
        end
      end.join.b
    end

    # Logical page visits in this concrete rebuilt B-tree. Bounds use the same
    # encoded order as entries; the upper end is exclusive.
    def secondary_page_count(table, name, lower = nil, upper = nil)
      tree_page_count(table.fetch(:native_secondary_metadata).fetch(name)[:tree], lower, upper)
    end

    def refresh_auxiliary_indexes
      @tables.each do |name, stored|
        definitions = catalog_indexes(stored).select { |index| %w[gin gist].include?(index[:method]) }
        next if definitions.empty? && !stored.key?(:native_auxiliary_indexes)
        signature = [stored[:rows].object_id, stored[:columns].object_id, stored[:indexes].object_id,
                     stored[:constraints].object_id, @types.object_id]
        next if stored[:native_auxiliary_signature] == signature
        codec = statistics_codec
        indexes = build_auxiliary_indexes(stored, codec, definitions)
        @tables[name] = stored.merge(native_auxiliary_signature: signature, native_auxiliary_indexes: indexes.freeze,
                                    native_auxiliary_codec: codec)
      end
    rescue RuntimeError, KeyError => error
      raise Error.new('54000', error.message)
    end

    # A GIN posting dictionary maps canonical terms to row ordinals. Multi-key
    # indexes prefix each term with its key position, matching the stored runs.
    def gin_terms(table, name, key_position, value)
      metadata = table.fetch(:native_auxiliary_indexes).fetch(name)
      return [] if value.nil?
      type = metadata[:types].fetch(key_position)
      codec = table.fetch(:native_auxiliary_codec)
      stored = statistics_value(type, value)
      terms = if type == 'jsonb'
                codec.gin_jsonb_terms(stored)
              else
                elements = stored.is_a?(Hash) ? stored[:elements] : stored
                elements.compact.map { |element| codec.gin_element_term(type[0...-2], element) }.uniq.sort
              end
      prefix = metadata[:columns].length > 1 ? [key_position].pack('C') : ''.b
      terms.map { |term| (prefix + term).b }
    end

    def gin_page_count(table, name, lower = nil, upper = nil)
      tree_page_count(table.fetch(:native_auxiliary_indexes).fetch(name)[:tree], lower, upper)
    end

    # Queries have one value/strategy per key component. A nil value is an
    # unbound component; results still pass through the SQL residual filter.
    # Empty containment cannot use the overlap descent and requests a fallback.
    def gist_candidates(table, name, values, strategies)
      metadata = table.fetch(:native_auxiliary_indexes).fetch(name)
      codec = table.fetch(:native_auxiliary_codec)
      queries = values.each_with_index.map do |value, position|
        next nil if value.nil?
        op = metadata[:opclasses][position]
        if op[:kind] == :range
          stored = statistics_value(metadata[:types][position], value)
          return nil if strategies[position] == '@>' && stored == :empty
          stored
        else
          codec.key_body(metadata[:types][position], statistics_value(metadata[:types][position], value))
        end
      end
      keys, pages, descents = [], 0, 0
      matches = lambda do |bounds, leaf|
        bounds.each_with_index.all? do |bound, i|
          query = queries[i]
          next true if query.nil?
          op = metadata[:opclasses][i]
          if op[:kind] == :scalar
            leaf ? bound[0] == query : bound[0] <= query && query <= bound[1]
          elsif leaf && strategies[i] == '@>'
            gist_range_contains?(codec, op[:elem], bound, query)
          else
            gist_range_overlaps?(codec, op[:elem], bound, query)
          end
        end
      end
      visit = lambda do |node|
        pages += 1
        if node[:leaf]
          node[:entries].each { |bound, key| keys << key if matches.call(bound, true) }
        else
          descents += 1
          node[:children].each { |bound, child| visit.call(child) if matches.call(bound, false) }
        end
      end
      visit.call(metadata[:tree]) if metadata[:tree]
      { ordinals: keys.sort.map { |key| metadata[:row_positions].fetch(key) }, pages: pages, descents: descents }
    end

    def tree_page_count(root, lower = nil, upper = nil)
      return 0 unless root
      count = 0
      visit = lambda do |node|
        count += 1
        return if node[:children].empty?
        first = lower ? (node[:keys].bsearch_index { |key| key > lower } || node[:keys].length) : 0
        last = upper ? (node[:keys].bsearch_index { |key| key >= upper } || node[:keys].length) : node[:children].length - 1
        (first..last).each { |index| visit.call(node[:children][index]) } if first <= last
      end
      visit.call(root)
      count
    end

    private

    def gist_range_overlaps?(codec, element, left, right)
      return false if left == :empty || right == :empty
      [[left, right], [right, left]].none? do |before, after|
        next false if before[:upper].nil? || after[:lower].nil?
        comparison = codec.key_body(element, before[:upper]) <=> codec.key_body(element, after[:lower])
        comparison.negative? || comparison.zero? && !(before[:upper_inc] && after[:lower_inc])
      end
    end

    def gist_range_contains?(codec, element, left, right)
      return true if right == :empty
      return false if left == :empty
      %i[lower upper].all? do |side|
        next true if left[side].nil?
        next false if right[side].nil?
        comparison = codec.key_body(element, right[side]) <=> codec.key_body(element, left[side])
        comparison = -comparison if side == :upper
        inclusive = side == :lower ? :lower_inc : :upper_inc
        comparison.positive? || comparison.zero? && (!right[inclusive] || left[inclusive])
      end
    end

    def auxiliary_type_gate(method, type)
      if method == 'gin'
        return if type == 'jsonb'
        raise Error.new('42704', "#{type} has no GIN operator class") unless type.end_with?('[]')
        raise Error.new('0A000', "#{type} has no supported GIN element class") unless GIN_ELEMENT_TYPES.include?(type[0...-2])
      elsif !Types::RANGES.key?(type) && !GIST_SCALAR_TYPES.include?(type)
        deferred = %w[text bytea decimal interval].include?(type)
        raise Error.new(deferred ? '0A000' : '42704', "#{type} has no supported GiST operator class")
      end
    end

    def build_auxiliary_indexes(table, codec, definitions, fresh: false)
      cost, lifetime = @cost, @lifetime_cost
      @scalar_bytes ||= 0
      maximum, lifetime_maximum = @max_cost, @lifetime_max_cost
      @max_cost, @lifetime_max_cost = 0, 0
      result = {}
      return result if definitions.empty?
      row_keys = statistics_storage_rows(table, codec).to_h { |key, row| [row.object_id, key] }
      positions = table[:rows].each_with_index.to_h { |row, ordinal| [row_keys.fetch(row.object_id), ordinal] }.freeze
      definitions.each do |definition|
        details = secondary_metadata(table, definition)
        if details[:columns].any?(&:nil?)
          raise Error.new('0A000', 'GIN/GiST expression keys are not supported')
        end
        details[:types].each { |type| auxiliary_type_gate(details[:kind], type) }
        entries, postings = [], {}
        if details[:kind] == 'gin'
          shape = table.merge(native_auxiliary_indexes: { definition[:name] => details }, native_auxiliary_codec: codec)
          table[:rows].each_with_index do |row, ordinal|
            next if details[:predicate] && evaluate(details[:predicate], row) != true
            details[:columns].each_with_index do |column, position|
              gin_terms(shape, definition[:name], position, row[column]).each do |term|
                (postings[term] ||= []) << ordinal
              end
            end
          end
          postings.each do |term, ordinals|
            ordinals.sort_by! { |ordinal| row_keys.fetch(table[:rows][ordinal].object_id) }
            keys = ordinals.map { |ordinal| row_keys.fetch(table[:rows][ordinal].object_id) }
            codec.gin_pack_runs(term, keys).each { |key| entries << [key.freeze, nil].freeze }
            ordinals.freeze
          end
          entries.sort_by!(&:first)
          imported = definition[:storage_tree_profile] if !fresh && table[:storage_tree_rows].equal?(table[:rows])
          previous = table[:native_auxiliary_indexes]&.dig(definition[:name], :physical_tree) unless fresh
          profile = secondary_tree_profile(codec, entries, table, previous: previous, imported: imported)
          details.merge!(profile, entries: entries.freeze, postings: postings.freeze)
        else
          shape = { columns: table[:columns] }
          ops = codec.gist_opclasses_for(shape, cols: details[:columns])
          table[:rows].each do |row|
            next if details[:columns].any? { |column| row[column].nil? }
            next if details[:predicate] && evaluate(details[:predicate], row) != true
            values = row.each_with_index.map { |value, i| statistics_value(table[:columns][i][:type], value) }
            entries << [codec.gist_leaf_bound(ops, values, details[:columns]), row_keys.fetch(row.object_id)]
          end
          tree = entries.empty? ? nil : codec.gist_build(ops, entries)
          count, height = 0, 0
          freeze_node = lambda do |node, depth|
            count += 1
            height = [height, depth].max
            if node[:leaf]
              node[:entries].each(&:freeze)
              node[:entries].freeze
            else
              node[:children].each { |entry| freeze_node.call(entry[1], depth + 1); entry.freeze }
              node[:children].freeze
            end
            node.freeze
          end
          freeze_node.call(tree, 1) if tree
          details.merge!(tree: tree, node_count: count, height: height, entries: entries.freeze, opclasses: ops.freeze)
        end
        details[:row_positions] = positions
        result[definition[:name]] = details.freeze
      end
      result
    ensure
      @cost, @lifetime_cost = cost, lifetime
      @max_cost, @lifetime_max_cost = maximum, lifetime_maximum
    end

    def secondary_metadata(table, index)
      scope = table_columns(table)
      elements = index[:elements] || index[:columns].map { |name| { expr: { kind: :column, name: name } } }
      expressions = elements.map do |element|
        validate_index_expression(element[:expr], scope)
        compile(element[:expr], scope)
      end
      predicate = index[:where] && (validate_index_expression(index[:where], scope); compile_filter(index[:where], scope))
      { definition: index, kind: index[:method] || 'btree', elements: elements,
        types: expressions.map(&:type), expressions: expressions, predicate: predicate,
        columns: elements.map { |element| table[:columns].index { |column| column[:name] == element[:expr][:name] } if element[:expr][:kind] == :column },
        collations: elements.map do |element|
          column = element[:expr][:kind] == :column && table[:columns].find { |item| item[:name] == element[:expr][:name] }
          column && column[:collation] != 'C' ? column[:collation] : nil
        end,
        orders: elements.map do |element|
          desc = element[:direction] == :desc
          [desc, element[:nulls] ? element[:nulls] == :first : desc]
        end }
    end

    def secondary_tree_profile(codec, entries, table = nil, previous: nil, imported: nil)
      columns = { columns: [] }
      pairs = entries.map do |key, _|
        [key, { key: key, row: [], table: columns, forms: [], comps: [], size: key.bytesize }]
      end
      page_size = storage_page_size(table)
      tree = imported ? codec.tree_from_profile(imported, pairs) : codec.patch_tree(previous, pairs, page_size - FileFormat::PAGE_HEADER)
      compact_tree_profile(tree).merge(physical_tree: tree)
    end

    def compact_tree_profile(root)
      count, height = 0, 0
      compact = lambda do |node, depth|
        return nil unless node
        count += 1
        height = [height, depth].max
        { keys: node[:keys].freeze, children: node[:children].map { |child| compact.call(child, depth + 1) }.freeze }.freeze
      end
      tree = compact.call(root, 1)
      { tree: tree, node_count: count, height: height }
    end

    def build_secondary_indexes(table, codec, definitions = nil, fresh: false)
      cost, lifetime = @cost, @lifetime_cost
      @scalar_bytes ||= 0
      maximum, lifetime_maximum = @max_cost, @lifetime_max_cost
      @max_cost, @lifetime_max_cost = 0, 0
      metadata, indexes = {}, {}
      definitions ||= catalog_indexes(table).select { |index| !index[:method] || index[:method] == 'btree' }
      return [metadata, indexes] if definitions.empty?
      row_keys = statistics_storage_rows(table, codec).to_h { |key, row| [row.object_id, key] }
      definitions.each do |index|
        details = secondary_metadata(table, index)
        metadata[index[:name]] = details
        shape = table.merge(native_secondary_metadata: metadata, native_secondary_codec: codec)
        seen = {}
        entries = []
        table[:rows].each_with_index do |row, ordinal|
          next if details[:predicate] && evaluate(details[:predicate], row) != true
          values = details[:expressions].map { |expression| evaluate(expression, row) }
          prefix = secondary_prefix_key(shape, index[:name], values)
          if index[:unique] && values.none?(&:nil?)
            raise Error.new('23505', 'duplicate unique index key') if seen[prefix]
            seen[prefix] = true
          end
          entries << [(prefix + row_keys.fetch(row.object_id)).freeze, ordinal].freeze
        end
        indexes[index[:name]] = entries.sort_by(&:first).freeze
        imported = index[:storage_tree_profile] if !fresh && table[:storage_tree_rows].equal?(table[:rows])
        previous = table[:native_secondary_metadata]&.dig(index[:name], :physical_tree) unless fresh
        profile = secondary_tree_profile(codec, indexes[index[:name]], table, previous: previous, imported: imported)
        details.merge!(profile)
        details.freeze
      end
      [metadata, indexes]
    ensure
      @cost, @lifetime_cost = cost, lifetime
      @max_cost, @lifetime_max_cost = maximum, lifetime_maximum
    end
  end

  Engine.prepend(SecondaryIndexes)
end
