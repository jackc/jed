# frozen_string_literal: true

module JedNative
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
    def value(slot, column) = @node[:recs][slot][:row][column]
    def units(slot, selected = nil)
      record = @node[:recs][slot]
      pages = slabs = 0
      record[:forms].each_with_index do |form, column|
        next if form == :inline || selected && !selected.include?(column)
        payload = @codec.value_payload(@columns[column][:type], record[:row][column])
        stored = form == :external_comp ? record[:comps][column] : payload
        pages += (stored.bytesize + @capacity - 1) / @capacity if %i[external external_comp].include?(form)
        slabs += (payload.bytesize + @capacity - 1) / @capacity if %i[inline_comp external_comp].include?(form)
      end
      [pages, slabs]
    end
  end
end
