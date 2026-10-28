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
    attr_reader :pages, :root, :live, :allocator, :assignments, :live_pages, :overflow_ids, :table_catalog

    def initialize(codec, snapshot, page_size, allocator, page_ids: {}.compare_by_identity, overflow_ids: {}.compare_by_identity)
      @codec, @page_size, @allocator = codec, page_size, allocator
      @page_ids, @next_page_ids = page_ids, {}.compare_by_identity
      @old_overflow_ids, @overflow_ids = overflow_ids, {}.compare_by_identity
      @live_pages = Set.new
      @capacity = codec.payload_capacity(page_size)
      @body, @assignments, @live = {}, [], 0
      @codec.instance_variable_set(:@ctypes, snapshot.fetch(:types, []).to_h { |type| [type[:name], type[:fields]] })
      entries = []
      snapshot.fetch(:types, []).sort_by { |type| type[:name].b }.each { |type| entries << "\x01".b + @codec.composite_type_entry_bytes(type) }
      snapshot.fetch(:sequences, []).sort_by { |sequence| sequence[:name].b }.each { |sequence| entries << "\x02".b + @codec.sequence_entry_bytes(sequence) }
      snapshot.fetch(:collations, []).sort_by { |collation| collation[:name].b }.each { |collation| entries << "\x03".b + @codec.collation_entry_bytes(collation) }
      tables = snapshot.fetch(:tables, []).sort_by { |table| table[:name].b }
      @table_catalog = []
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
        encoded = @codec.table_entry_bytes(table, data_root, roots, table[:row_count] || table[:rows].length)
        metadata, = @codec.decode_table_entry(encoded, 0)
        metadata[:statistics] = table[:statistics] if table[:statistics]
        @table_catalog << metadata
        entries << "\x00".b + encoded
      end
      tables.each { |table| entries.concat(@codec.statistics_entries(table)) }
      snapshot.fetch(:views, []).sort_by { |view| view[:name].b }.each { |view| entries << "\x05".b + @codec.view_entry_bytes(view) }
      catalog = @codec.catalog_layout(entries, @codec.chain_capacity(@capacity))
      ids = catalog.map { @allocator.allocate }
      @root = ids.first
      @codec.catalog_out(@body, @page_size, ids, catalog)
      @live_pages.merge(ids)
      @live += catalog.length
      @pages = PageBuffer.new(page_size, @allocator.high_water, compact: true)
      @body.each { |page, descriptor| @codec.write_descriptor(@pages, @page_size, page, descriptor) }
    end

    def adopt
      @next_page_ids
    end

    # Publication reuses the already validated bounds and leaf entries. Only
    # routing nodes need their newly assigned physical page identity; decoding
    # all freshly encoded pages would duplicate the entire index inventory.
    def published_gist(node, operations)
      count = height = 0
      visit = lambda do |current, depth|
        next nil unless current
        count += 1
        height = [height, depth].max
        page = @next_page_ids[current] || @page_ids[current] || current[:page]
        raise Error.new('XX001', 'GiST node has no committed page') unless page
        if current[:leaf]
          current[:page] == page ? current : current.merge(page: page)
        else
          children = current[:children].map { |bound, child| [bound, visit.call(child, depth + 1)] }
          current.merge(page: page, children: children)
        end
      end
      tree = visit.call(node, 1)
      { tree: tree, node_count: count, height: height, opclasses: operations }
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

    def write_tree(node)
      return 0 unless node
      if (page = @page_ids[node] || node[:page])
        @live += remember_tree(node)
        return page
      end
      children = node[:children].map { |child| write_tree(child) }
      if children.empty?
        overflow = {}
        kept = []
        retain = lambda do |reference|
          reference.each_overflow_page do |id|
            raise Error.new('XX001', 'overflow chain belongs to multiple records') unless @live_pages.add?(id)
            kept << id
          end
        end
        payload = @codec.emit_leaf_pax(node[:recs], @capacity, -> { @allocator.allocate }, overflow, retain: retain)
        overflow.each do |id, descriptor|
          @body[id] = descriptor
          @live_pages.add(id)
        end
        @overflow_ids[node] = (kept + overflow.keys).freeze
        @live += kept.length + overflow.length
        kind = FileFormat::PAGE_LEAF
      else
        kind = FileFormat::PAGE_INTERIOR
        payload = @codec.emit_interior(node[:keys], children)
      end
      # Incremental commits allocate a dirty leaf after its fresh chains. The
      # separate from-scratch writer allocates the leaf first, as the golden
      # image contract requires.
      page = @allocator.allocate
      @assignments << [node, page]
      @next_page_ids[node] = page
      @overflow_ids[page] = @overflow_ids[node] if children.empty?
      @live_pages.add(page)
      @live += 1
      @codec.page_out(@body, @page_size, page, kind, node[:keys].length, payload)
      page
    end

    def write_gist(node, operations)
      return 0 unless node
      if (page = @page_ids[node] || node[:page])
        @live += remember_tree(node, gist: true)
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
          payload << [bytes.bytesize].pack('n') << bytes << @codec.pointer(child_page)
        end
        kind, count = FileFormat::PAGE_GIST_INTERIOR, node[:children].length
      end
      raise Error.new('0A000', 'GiST page exceeds capacity') if payload.bytesize > @capacity
      page = @allocator.allocate
      @assignments << [node, page]
      @next_page_ids[node] = page
      @live_pages.add(page)
      @codec.page_out(@body, @page_size, page, kind, count, payload)
      @live += 1
      page
    end

    def remember_tree(node, gist: false)
      page = @page_ids[node] || node[:page]
      @next_page_ids[node] = page if page
      @live_pages.add(page) if page
      children = gist ? (node[:leaf] ? [] : node[:children].map(&:last)) : node[:children]
      count = page ? 1 : 0
      if !gist && children.empty?
        cache_key = node.is_a?(PagedLeaf) ? page : node
        overflow = @old_overflow_ids[cache_key] || @old_overflow_ids[node] || node[:overflow_pages] || []
        overflow = overflow.freeze
        @overflow_ids[cache_key] = overflow
        @live_pages.merge(overflow)
        count += overflow.length
      end
      children.each { |child| count += remember_tree(child, gist: gist) }
      count
    end
  end
end
