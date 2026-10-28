# frozen_string_literal: true

module JedNative
  class DeferredLivePages
    include Enumerable
    def initialize(known, tables)
      @known, @tables = known, tables
    end
    def each
      return enum_for(:each) unless block_given?
      reached = @known.dup
      walk = lambda do |node|
        next unless node
        reached.add(node[:page])
        reached.merge(node[:overflow_pages] || [])
        node[:children].each { |child| walk.call(child) }
      end
      walk_gist = lambda do |node|
        next unless node
        reached.add(node[:page])
        node[:children].each { |_, child| walk_gist.call(child) } unless node[:leaf]
      end
      @tables.each do |table|
        walk.call(table[:rows].tree) if table[:rows].is_a?(PagedRows)
        table[:indexes].each do |index|
          walk.call(index[:tree_profile][:tree]) if index.dig(:tree_profile, :paged_rows)
          walk_gist.call(index[:gist_profile][:tree]) if index[:gist_profile]
        end
      end
      reached.each { |page| yield page }
    end
  end

  # Decoded leaves are refaultable. Routing nodes stay resident, while neither
  # row values nor leaf key arrays escape this bounded cache unless a query or
  # pending write actually retains them.
  class LeafPool
    attr_reader :codec, :image, :page_size, :fetch_page
    def initialize(codec, image, page_size, limit)
      @codec, @image, @page_size = codec, image, page_size
      @fetch_page = method(:page)
      @fallback_cache = DecodedLeafCache.new(limit) unless image.respond_to?(:decoded_leaf_cache)
    end

    def page(index)
      @codec.read_page(@image, @page_size, index)
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new('XX001', error.message)
    end

    def leaf(index, columns, key = nil)
      # Hash the schema's identity, rather than recursively hashing every
      # column descriptor for every field read. The cached value retains that
      # exact object, so its identity cannot be reused while this key is live.
      key ||= [index, columns.object_id]
      decoded_cache.fetch(key) do
        raw = page(index)
        raise Error.new('XX001', 'expected a B-tree leaf') unless raw[:type] == FileFormat::PAGE_LEAF
        keys, regions = @codec.parse_pax_leaf(raw[:payload], raw[:item_count], columns)
        raise Error.new('XX001', 'leaf keys are not strictly ascending') unless keys.each_cons(2).all? { |a, b| a < b }
        { keys: keys, regions: regions, count: raw[:item_count], schema: columns }
      end
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new('XX001', error.message)
    end

    def decoded_cache = @fallback_cache || @image.decoded_leaf_cache
    def cached_leaves = decoded_cache.size
  end

  class PagedLeaf
    attr_reader :page, :pool, :columns
    EMPTY = [].freeze
    def initialize(pool, page, columns)
      @pool, @page, @columns = pool, page, columns
      @cache_key = [page, columns.object_id].freeze
    end

    def [](key)
      case key
      when :page then @page
      when :children then EMPTY
      when :keys then data[:keys]
      when :recs then records
      when :overflow_pages then overflow_pages
      end
    end

    def data = @pool.leaf(@page, @columns, @cache_key)
    def count
      raw = @pool.page(@page)
      raise Error.new('XX001', 'expected a B-tree leaf') unless raw[:type] == FileFormat::PAGE_LEAF
      raw[:item_count]
    end

    def value(slot, column)
      region = data[:regions].fetch(column)
      @pool.codec.leaf_value(region, @columns[column][:type], slot, @pool.fetch_page)
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new('XX001', error.message)
    end

    def units(slot, selected = nil)
      pages = slabs = 0
      data[:regions].each_with_index do |region, column|
        next if region[:width] || selected && !selected.include?(column)
        low = slot.zero? ? 0 : region[:ends][slot - 1]
        length = region[:ends][slot] - low
        next if length.zero?
        body = region[:body]
        # Overflow pages hold a chain slab (C less a successor tag when encrypted); decompression
        # units count C-sized slabs (cost.md §3).
        capacity = @pool.codec.payload_capacity(@pool.page_size)
        slab = @pool.codec.chain_capacity(capacity)
        case body.getbyte(low)
        when FileFormat::TAG_EXTERNAL
          raise Error.new('XX001', 'short external value span') if length < 9
          pages += (body.unpack1('N', offset: low + 5) + slab - 1) / slab
        when FileFormat::TAG_INLINE_COMP
          raise Error.new('XX001', 'short compressed value span') if length < 5
          slabs += (body.unpack1('N', offset: low + 1) + capacity - 1) / capacity
        when FileFormat::TAG_EXTERNAL_COMP
          raise Error.new('XX001', 'short compressed external value span') if length < 13
          pages += (body.unpack1('N', offset: low + 5) + slab - 1) / slab
          slabs += (body.unpack1('N', offset: low + 9) + capacity - 1) / capacity
        end
      end
      [pages, slabs]
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new('XX001', error.message)
    end

    def overflow_pages
      pages = Set.new
      data[:regions].each do |region|
        next if region[:width]
        previous = 0
        region[:ends].each do |finish|
          bytes = region[:body].byteslice(previous, finish - previous)
          previous = finish
          next unless [FileFormat::TAG_EXTERNAL, FileFormat::TAG_EXTERNAL_COMP].include?(bytes.getbyte(0))
          @pool.codec.note_external(bytes)
          page = bytes.byteslice(1, 4).unpack1('N')
          until page.zero?
            raise Error.new('XX001', 'overflow page reached twice') unless pages.add?(page)
            raw = @pool.page(page)
            raise Error.new('XX001', 'expected an overflow page') unless raw[:type] == FileFormat::PAGE_OVERFLOW
            page = raw[:next_page]
          end
        end
      end
      pages.to_a
    end

    def records
      codec = @pool.codec
      shape = { columns: @columns }
      self[:keys].each_with_index.map do |key, slot|
        row = @columns.each_index.map { |column| storage_reference(slot, column) || value(slot, column) }
        forms = row.map { |value| value.is_a?(StoredValue) ? value.form : :inline }
        comps = row.map { |value| value.is_a?(StoredValue) ? value.compressed_block : nil }
        size = key.bytesize + row.each_with_index.sum do |value, column|
          codec.fixed_width(@columns[column][:type]) || (value.is_a?(StoredValue) ? value.stored_bytes.bytesize : 0)
        end
        { key: key, row: row, table: shape, forms: forms, comps: comps, size: size }
      end
    end

    def storage_reference(slot, column) = StoredValue.from_leaf(self, slot, column)
    def null_at?(slot, column)
      region = data[:regions].fetch(column)
      if region[:width]
        value(slot, column).nil?
      else
        region[:ends][slot] == (slot.zero? ? 0 : region[:ends][slot - 1])
      end
    end
  end

  # A virtual sequence keeps the existing executor's ordinal interface without
  # retaining a Ruby array for every committed row or index entry.
  class PageSequence
    include Enumerable
    def initialize(length, getter, iterator = nil)
      @length, @getter, @iterator = length, getter, iterator
    end
    def length = @length.respond_to?(:call) ? @length.call : @length
    alias size length
    def empty? = length.zero?
    def [](index, count = nil)
      return Array.new(count) { |i| self[index + i] }.compact if count
      return index.map { |i| self[i] }.compact if index.is_a?(Range)
      index += length if index.negative?
      return nil unless index.between?(0, length - 1)
      @getter.call(index)
    end
    def fetch(index) = self[index] || raise(IndexError, 'row ordinal out of bounds')
    def at(index) = self[index]
    def dig(index, *rest)
      value = self[index]
      rest.empty? || value.nil? ? value : value.dig(*rest)
    end
    def first = self[0]
    def last = self[-1]
    def each
      return enum_for(:each) unless block_given?
      if @iterator
        @iterator.call { |value| yield value }
      else
        length.times { |index| yield self[index] }
      end
      self
    end
    def each_index = block_given? ? length.times { |index| yield index } : (0...length).each
    def reverse_each = block_given? ? (length - 1).downto(0) { |index| yield self[index] } : enum_for(:reverse_each)
    def bsearch_index
      low, high = 0, length
      while low < high
        middle = (low + high) / 2
        yield(self[middle]) ? high = middle : low = middle + 1
      end
      low == length ? nil : low
    end
    def +(other) = to_a + other.to_a
    def dup = to_a
    def ==(other) = equal?(other) || to_a == other
  end

  class PagedRow < Array
    attr_reader :storage_leaf, :storage_slot
    def initialize(leaf, slot, converter = nil)
      super(leaf.columns.length)
      @storage_leaf, @storage_slot, @converter = leaf, slot, converter
      @loaded = 0
    end
    def [](index, count = nil)
      return to_a[index, count] if count
      return to_a[index] if index.is_a?(Range)
      index += length if index.negative?
      return nil unless index.between?(0, length - 1)
      mask = 1 << index
      if (@loaded & mask).zero?
        value = @storage_leaf.value(@storage_slot, index)
        value = @converter.call(@storage_leaf.columns[index][:type], value) if @converter
        self[index] = value
        @loaded |= mask
      end
      super(index)
    end
    def each
      return enum_for(:each) unless block_given?
      length.times { |index| yield self[index] }
      self
    end
    def each_with_index
      return enum_for(:each_with_index) unless block_given?
      length.times { |index| yield self[index], index }
      self
    end
    def map = block_given? ? each.map { |value| yield value } : enum_for(:map)
    def to_a = each.to_a
    alias to_ary to_a
    def dup = to_a
    def +(other) = to_a + other.to_a
    def zip(*others, &block) = to_a.zip(*others, &block)
    def values_at(*indices) = indices.map { |index| self[index] }
    def storage_key = @storage_leaf[:keys][@storage_slot]
    def storage_reference(column) = @storage_leaf.storage_reference(@storage_slot, column)
    def null_at?(column) = @storage_leaf.null_at?(@storage_slot, column)
    def ==(other) = to_a == other.to_a
  end

  class PagedRows < PageSequence
    attr_reader :tree, :pool, :columns, :raw, :profile, :entries
    def initialize(pool, root, columns, row_count, converter: nil, raw: nil, tree: :load)
      @pool, @columns, @converter, @raw = pool, columns, converter, raw
      @leaves, seen = [], Set.new
      @tree, height = tree == :load ? load_skeleton(root, seen) : adopt_tree(tree, seen)
      @profile = { tree: @tree, height: height, node_count: seen.length, paged_rows: self }
      length = row_count || -> { actual_count }
      super(length, ->(index) { leaf, slot = locate(index); leaf && PagedRow.new(leaf, slot, @converter) },
            ->(&block) { each_leaf_row { |leaf, slot, _| block.call(PagedRow.new(leaf, slot, @converter)) } })
      @entries = PageSequence.new(length, ->(index) { leaf, slot = locate(index); leaf && [leaf[:keys][slot], row_at(leaf, slot)] },
                                  ->(&block) { each_leaf_row { |leaf, slot, _| block.call([leaf[:keys][slot], row_at(leaf, slot)]) } })
    end

    def converted(&converter)
      result = dup_view
      result.instance_variable_set(:@raw, self)
      result.instance_variable_set(:@converter, converter)
      result
    end

    # A physical slot remains stable for the lifetime of this immutable tree.
    # Passing it through a cursor avoids reading every leaf merely to compute
    # its position in a hypothetical flat row array.
    def row_at(leaf, slot) = PagedRow.new(leaf, slot, @converter)
    def [](index, count = nil)
      return index if index.is_a?(PagedRow) && count.nil?
      super
    end

    def each_entry(lower: nil, upper: nil, reverse: false, &block)
      return enum_for(:each_entry, lower: lower, upper: upper, reverse: reverse) unless block
      walk = lambda do |node|
        next unless node
        if node[:children].empty?
          keys = node[:keys]
          first = lower ? keys.bsearch_index { |key| key >= lower } || keys.length : 0
          last = upper ? keys.bsearch_index { |key| key >= upper } || keys.length : keys.length
          slots = reverse ? (last - 1).downto(first) : (first...last).each
          slots.each { |slot| block.call([keys[slot], row_at(node, slot)]) }
        else
          first = lower ? node[:keys].bsearch_index { |key| key > lower } || node[:keys].length : 0
          last = upper ? node[:keys].bsearch_index { |key| key >= upper } || node[:keys].length : node[:children].length - 1
          children = reverse ? last.downto(first) : (first..last).each
          children.each { |slot| walk.call(node[:children][slot]) }
        end
      end
      walk.call(@tree)
    end

    def dup_view
      # Reuse the resident routing skeleton and bounded pool across raw and
      # typed views. Rebuilding the closures binds them to the correct view.
      self.class.new(@pool, 0, @columns, length, raw: @raw, tree: @tree)
    end

    def keys = @keys ||= PageSequence.new(@length, ->(index) { @entries[index]&.first }, ->(&block) { @entries.each { |key, _| block.call(key) } })
    def rowids(codec) = PageSequence.new(length, ->(index) { codec.decode_int(8, keys[index]) })
    def actual_count
      ensure_offsets
      @offsets.last || 0
    end

    def point(key)
      leaf, slot = point_slot(key)
      leaf && PagedRow.new(leaf, slot, @converter)
    end

    def ordinal_for_key(key)
      leaf, slot = point_slot(key)
      raise Error.new('XX001', 'index entry references absent row') unless leaf
      ordinal_for_leaf_slot(leaf, slot)
    end

    def ordinal_for_leaf_slot(leaf, slot)
      ensure_offsets
      @leaf_numbers ||= @leaves.each_with_index.to_h
      @offsets[@leaf_numbers.fetch(leaf)] + slot
    end

    def point_slot(key)
      node = @tree
      while node && !node[:children].empty?
        index = node[:keys].bsearch_index { |boundary| boundary > key } || node[:keys].length
        node = node[:children][index]
      end
      return [nil, nil] unless node
      keys = node[:keys]
      slot = keys.bsearch_index { |stored| stored >= key }
      slot && keys[slot] == key ? [node, slot] : [nil, nil]
    end

    def scan_units(ordinals = nil, columns = nil)
      return [0, 0] if columns&.empty?
      # Fixed-width columns cannot spill or compress. Their zero payload block
      # must not force later leaves merely to price a short LIMIT scan.
      selected = columns || @columns.each_index
      return [0, 0] if selected.all? { |column| @pool.codec.fixed_width(@columns[column][:type]) }
      pages = slabs = 0
      if ordinals
        ordinals.each do |ordinal|
          leaf, slot = ordinal.is_a?(PagedRow) ? [ordinal.storage_leaf, ordinal.storage_slot] : locate(ordinal)
          a, b = leaf.units(slot, columns)
          pages += a; slabs += b
        end
      else
        each_leaf_row do |leaf, slot, _|
          a, b = leaf.units(slot, columns)
          pages += a; slabs += b
        end
      end
      [pages, slabs]
    end

    private

    def adopt_tree(node, seen)
      return [nil, 0] unless node
      seen.add(node.object_id)
      if node[:children].empty?
        leaf = node.is_a?(PagedLeaf) || node.is_a?(RecordLeaf) ? node : RecordLeaf.new(node, @columns, @pool.codec, @pool.codec.payload_capacity(@pool.page_size))
        @leaves << leaf
        return [leaf, 1]
      end
      height = 0
      children = node[:children].map do |child|
        adopted, height = adopt_tree(child, seen)
        adopted
      end
      [node.merge(children: children), height + 1]
    end

    def load_skeleton(page, seen)
      return [nil, 0] if page.zero?
      raise Error.new('XX001', 'B-tree page reached twice') unless seen.add?(page)
      raw = @pool.page(page)
      if raw[:type] == FileFormat::PAGE_LEAF
        leaf = PagedLeaf.new(@pool, page, @columns)
        @leaves << leaf
        return [leaf, 1]
      end
      raise Error.new('XX001', 'expected a B-tree interior') unless raw[:type] == FileFormat::PAGE_INTERIOR
      pointers = Array.new(raw[:item_count] + 1) { |index| raw[:payload].byteslice(index * 4, 4).unpack1('N') }
      first, height = load_skeleton(pointers.shift, seen)
      children = [first]
      pointers.each do |child|
        if height == 1
          raise Error.new('XX001', 'B-tree page reached twice') unless seen.add?(child)
          leaf = PagedLeaf.new(@pool, child, @columns)
          children << leaf
          @leaves << leaf
        else
          node, child_height = load_skeleton(child, seen)
          raise Error.new('XX001', 'unbalanced B-tree') unless child_height == height
          children << node
        end
      end
      [{ page: page, keys: @pool.codec.interior_keys(raw), recs: [], children: children }, height + 1]
    end

    def each_leaf_row
      ordinal = 0
      @leaves.each do |leaf|
        leaf.data[:count].times do |slot|
          yield leaf, slot, ordinal
          ordinal += 1
        end
      end
    end

    def ensure_offsets
      @offsets ||= @leaves.each_with_object([0]) { |leaf, offsets| offsets << offsets.last + leaf.count }
    end

    def locate(index)
      ensure_offsets
      return [nil, nil] unless index.between?(0, @offsets.last - 1)
      leaf = @offsets.bsearch_index { |offset| offset > index } - 1
      [@leaves[leaf], index - @offsets[leaf]]
    end
  end
end
