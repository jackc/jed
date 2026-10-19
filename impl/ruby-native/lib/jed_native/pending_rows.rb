# frozen_string_literal: true

module JedNative
  # SQL sees ordinary values on demand. The separate stored references survive
  # demand faults and are available only to an eligible in-place UPDATE.
  class CarriedRow < Array
    def initialize(source, excluded)
      super(source.length)
      @source, @loaded, @references = source, {}, {}
      source.length.times do |column|
        next if excluded.include?(column)
        value = source.storage_reference(column)
        @references[column] = value if value&.large?
      end
    end
    def [](index, count = nil)
      return to_a[index, count] if count
      return to_a[index] if index.is_a?(Range)
      index += length if index.negative?
      return nil unless index.between?(0, length - 1)
      unless @loaded[index]
        Array.instance_method(:[]=).bind_call(self, index, @source[index])
        @loaded[index] = true
      end
      super(index)
    end
    def []=(index, value)
      @references.delete(index)
      @loaded[index] = true
      super
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
    def storage_reference(column) = @references[column]
    def discard_storage_reference(column) = @references.delete(column)
    def null_at?(column) = @loaded[column] ? self[column].nil? : @source.null_at?(column)
  end

  class MutationFrame < Array
    def initialize(rows)
      @segments = rows
      super(rows.sum(&:length))
    end
    def [](index)
      @segments.each do |row|
        return row[index] if index < row.length
        index -= row.length
      end
      nil
    end
  end

  # A statement owns only its changed row versions. The committed tree remains
  # immutable and can still serve pinned readers while this overlay is staged.
  class PendingRows
    include Enumerable
    attr_reader :base_table, :changes, :additions

    def initialize(table)
      @base_table, @base = table, table[:rows]
      @changes, @additions = {}, []
      @replacement_keys = {}.compare_by_identity
    end

    def dup
      copy = super
      copy.instance_variable_set(:@changes, @changes.dup)
      copy.instance_variable_set(:@additions, @additions.map(&:dup))
      copy.instance_variable_set(:@replacement_keys, @replacement_keys.dup)
      copy
    end

    def length = @base.length - @changes.count { |_, row| row.nil? } + @additions.length
    alias size length
    def empty? = length.zero?
    def each
      return enum_for(:each) unless block_given?
      @base.each do |row|
        key = row.storage_key
        row = @changes.fetch(key, row)
        yield row if row
      end
      @additions.each { |row, _| yield row }
      self
    end
    def each_unchanged
      return enum_for(:each_unchanged) unless block_given?
      @base.each { |row| yield row unless @changes.key?(row.storage_key) }
    end
    def changed_rows = @changes.values.compact + @additions.map(&:first)
    def [](index)
      each_with_index { |row, position| return row if position == index }
      nil
    end
    def []=(index, replacement)
      replace(self[index], replacement)
    end
    def replace(row, replacement)
      key = row.is_a?(PagedRow) ? row.storage_key : @replacement_keys[row]
      if key
        @changes[key] = replacement
        @replacement_keys[replacement] = key if replacement
      else
        index = @additions.index { |item, _| item.equal?(row) }
        raise IndexError, 'pending row is absent' unless index
        replacement ? @additions[index][0] = replacement : @additions.delete_at(index)
      end
      replacement
    end
    def append(row, rowid)
      @additions << [row, rowid]
      self
    end
    def each_change
      return enum_for(:each_change) unless block_given?
      codec = @base.pool.codec
      keyed = @base_table[:constraints].any? { |constraint| constraint[:kind] == :primary_key }
      @changes.each { |key, after| yield @base.point(key), after, keyed ? 0 : codec.decode_int(8, key) }
      @additions.each { |row, rowid| yield nil, row, rowid }
    end
    def rowids
      PageSequence.new(-> { length }, ->(index) { rowid_for(self[index]) },
        ->(&block) { each { |row| block.call(rowid_for(row)) } })
    end
    def rowid_for(row)
      return 0 if @base_table[:constraints].any? { |constraint| constraint[:kind] == :primary_key }
      key = row.is_a?(PagedRow) ? row.storage_key : @replacement_keys[row]
      return @base.pool.codec.decode_int(8, key) if key
      @additions.find { |item, _| item.equal?(row) }&.last
    end
    def +(rows) = to_a + rows.to_a
  end

  # New leaves use the same cursor interface as decoded file leaves, but their
  # records are the pending versions already charged to the transaction.
  class RecordLeaf
    attr_reader :columns
    def initialize(node, columns, codec, capacity)
      @node, @columns, @codec, @capacity = node, columns, codec, capacity
    end
    def [](key) = @node[key]
    def count = @node[:keys].length
    def data = { count: count }
    def value(slot, column)
      value = @node[:recs][slot][:row][column]
      value.is_a?(StoredValue) ? value.resolve : value
    end
    def storage_reference(slot, column)
      value = @node[:recs][slot][:row][column]
      value if value.is_a?(StoredValue)
    end
    def null_at?(slot, column) = @node[:recs][slot][:row][column].nil?
    def units(slot, selected = nil)
      record = @node[:recs][slot]
      pages = slabs = 0
      record[:forms].each_with_index do |form, column|
        next if form == :inline || selected && !selected.include?(column)
        value = record[:row][column]
        length = value.is_a?(StoredValue) ? value.payload_length : @codec.value_payload(@columns[column][:type], value).bytesize
        stored = form == :external_comp ? record[:comps][column].bytesize : length
        pages += (stored + @capacity - 1) / @capacity if %i[external external_comp].include?(form)
        slabs += (length + @capacity - 1) / @capacity if %i[inline_comp external_comp].include?(form)
      end
      [pages, slabs]
    end
  end

  # The one-leaf root retains newly written resident values, matching B4. A
  # value already carried from disk keeps its reference through this root too.
  class ResidentLeaf < PagedLeaf
    def initialize(pool, page, columns, records)
      super(pool, page, columns)
      @records = records
      @resident = RecordLeaf.new({ keys: records.map { |record| record[:key] }, recs: records, children: [] }, columns, pool.codec, pool.page_size - FileFormat::PAGE_HEADER)
    end
    def [](key) = %i[keys recs].include?(key) ? @resident[key] : super
    def records = @records
    def count = @records.length
    def value(slot, column) = @resident.value(slot, column)
    def storage_reference(slot, column) = @resident.storage_reference(slot, column)
    def null_at?(slot, column) = @resident.null_at?(slot, column)
    def units(slot, selected = nil) = @resident.units(slot, selected)
  end
end
