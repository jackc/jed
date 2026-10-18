# frozen_string_literal: true

module JedNative
  class PageAllocator
    attr_reader :high_water, :free
    def initialize(high_water, free = [])
      @high_water, @free = high_water, free.sort
      @position = 0
    end
    def allocate
      if @position < @free.length
        page = @free[@position]
        @position += 1
        page
      else
        raise Error.new('54000', 'database page count overflow') if @high_water >= 0xFFFF_FFFF
        page = @high_water
        @high_water += 1
        page
      end
    end
    def remaining = @free.drop(@position)
  end

  # Encoding is a preparation step: neither the file nor a node's committed page
  # identity changes until every page and manifest has been validated and synced.
  class CommitPlan
    attr_reader :pages, :root, :live, :allocator, :assignments, :live_pages, :overflow_ids

    def initialize(codec, snapshot, page_size, allocator, page_ids: {}.compare_by_identity, overflow_ids: {}.compare_by_identity)
      @codec, @page_size, @allocator = codec, page_size, allocator
      @page_ids, @next_page_ids = page_ids, {}.compare_by_identity
      @old_overflow_ids, @overflow_ids = overflow_ids, {}.compare_by_identity
      @live_pages = Set.new
      @capacity = page_size - FileFormat::PAGE_HEADER
      @body, @assignments, @live = {}, [], 0
      @codec.instance_variable_set(:@ctypes, snapshot.fetch(:types, []).to_h { |type| [type[:name], type[:fields]] })
      entries = []
      snapshot.fetch(:types, []).sort_by { |type| type[:name].b }.each { |type| entries << "\x01".b + @codec.composite_type_entry_bytes(type) }
      snapshot.fetch(:sequences, []).sort_by { |sequence| sequence[:name].b }.each { |sequence| entries << "\x02".b + @codec.sequence_entry_bytes(sequence) }
      snapshot.fetch(:collations, []).sort_by { |collation| collation[:name].b }.each { |collation| entries << "\x03".b + @codec.collation_entry_bytes(collation) }
      tables = snapshot.fetch(:tables, []).sort_by { |table| table[:name].b }
      tables.each do |table|
        tree = table.key?(:physical_tree) ? table[:physical_tree] : fresh_table_tree(table)
        data_root = write_tree(tree)
        roots = table.fetch(:indexes, []).map do |index|
          if index[:kind] == 'gist'
            operations = @codec.gist_opclasses_for(table, index)
            tree = index.key?(:physical_gist) ? index[:physical_gist] : fresh_gist_tree(table, index, operations)
            write_gist(tree, operations)
          else
            tree = index.key?(:physical_tree) ? index[:physical_tree] : fresh_index_tree(table, index)
            write_tree(tree)
          end
        end
        entries << "\x00".b + @codec.table_entry_bytes(table, data_root, roots, table[:row_count] || table[:rows].length)
      end
      tables.each { |table| entries.concat(@codec.statistics_entries(table)) }
      snapshot.fetch(:views, []).sort_by { |view| view[:name].b }.each { |view| entries << "\x05".b + @codec.view_entry_bytes(view) }
      catalog = @codec.catalog_layout(entries, @capacity)
      ids = catalog.map { @allocator.allocate }
      @root = ids.first
      catalog.each_with_index do |page, index|
        @body[ids[index]] = [FileFormat::PAGE_CATALOG, page[:count], ids[index + 1] || 0, page[:payload], page[:continued] ? 1 : 0]
        @live_pages.add(ids[index])
      end
      @live += catalog.length
      @pages = PageBuffer.new(page_size, @allocator.high_water)
      @body.each do |page, (kind, count, following, payload, flags)|
        @codec.write_page(@pages, @page_size, page, kind, count, following, payload, flags: flags || 0)
      end
    end

    def adopt
      @assignments.each { |node, page| @next_page_ids[node] = page }
      @next_page_ids
    end

    private

    def fresh_table_tree(table)
      pairs = @codec.table_entries(table).map do |key, row|
        forms, comps, size = @codec.plan_record(table, key, row, @capacity)
        [key, { key: key, row: row, table: table, forms: forms, comps: comps, size: size }]
      end
      @codec.build_tree(pairs, @capacity)
    end

    def fresh_index_tree(table, index)
      pairs = index[:kind] == 'gin' ? @codec.gin_index_entries(table, index) : @codec.index_entries(table, index)
      @codec.build_tree(pairs, @capacity)
    end

    def fresh_gist_tree(table, index, operations)
      entries = @codec.table_entries(table).filter_map do |key, row|
        next if index[:cols].any? { |column| row[column].nil? } || !@codec.index_admits?(index, row)
        [@codec.gist_leaf_bound(operations, row, index[:cols]), key]
      end
      entries.empty? ? nil : @codec.gist_build(operations, entries)
    end

    def tree_live_pages(node)
      return 0 unless node
      if node[:children].empty?
        1 + node[:recs].sum do |record|
          record[:forms].each_with_index.sum do |form, column|
            length = case form
            when :external then @codec.value_payload(record[:table][:columns][column][:type], record[:row][column]).bytesize
            when :external_comp then record[:comps][column].bytesize
            else 0
            end
            (length + @capacity - 1) / @capacity
          end
        end
      else
        1 + node[:children].sum { |child| tree_live_pages(child) }
      end
    end

    def write_tree(node)
      return 0 unless node
      if (page = @page_ids[node] || node[:page])
        remember_tree(node)
        @live += tree_live_pages(node)
        return page
      end
      children = node[:children].map { |child| write_tree(child) }
      page = @allocator.allocate
      @assignments << [node, page]
      @live_pages.add(page)
      @live += 1
      if children.empty?
        overflow = {}
        payload = @codec.emit_leaf_pax(node[:recs], @capacity, -> { @allocator.allocate }, overflow)
        overflow.each do |id, (kind, count, bytes, following)|
          @body[id] = [kind, count, following || 0, bytes]
          @live_pages.add(id)
        end
        @overflow_ids[node] = overflow.keys
        @live += overflow.length
        @body[page] = [FileFormat::PAGE_LEAF, node[:keys].length, 0, payload]
      else
        @body[page] = [FileFormat::PAGE_INTERIOR, node[:keys].length, 0, @codec.emit_interior(node[:keys], children)]
      end
      page
    end

    def gist_live_pages(node)
      node[:leaf] ? 1 : 1 + node[:children].sum { |_, child| gist_live_pages(child) }
    end

    def write_gist(node, operations)
      return 0 unless node
      if (page = @page_ids[node] || node[:page])
        remember_tree(node, gist: true)
        @live += gist_live_pages(node)
        return page
      end
      payload = +''.b
      if node[:leaf]
        node[:entries].each do |bound, key|
          bytes = @codec.gist_encode_bound(operations, bound)
          payload << [bytes.bytesize].pack('n') << bytes << [key.bytesize].pack('n') << key
        end
        kind, count = FileFormat::PAGE_GIST_LEAF, node[:entries].length
      else
        node[:children].each do |bound, child|
          child_page = write_gist(child, operations)
          bytes = @codec.gist_encode_bound(operations, bound)
          payload << [bytes.bytesize].pack('n') << bytes << [child_page].pack('N')
        end
        kind, count = FileFormat::PAGE_GIST_INTERIOR, node[:children].length
      end
      raise Error.new('0A000', 'GiST page exceeds capacity') if payload.bytesize > @capacity
      page = @allocator.allocate
      @assignments << [node, page]
      @live_pages.add(page)
      @body[page] = [kind, count, 0, payload]
      @live += 1
      page
    end

    def remember_tree(node, gist: false)
      page = @page_ids[node] || node[:page]
      @next_page_ids[node] = page if page
      @live_pages.add(page) if page
      overflow = @old_overflow_ids[node] || node[:overflow_pages] || []
      @overflow_ids[node] = overflow unless overflow.empty?
      @live_pages.merge(overflow)
      children = gist ? (node[:leaf] ? [] : node[:children].map(&:last)) : node[:children]
      children.each { |child| remember_tree(child, gist: gist) }
    end
  end
end
