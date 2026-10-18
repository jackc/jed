# frozen_string_literal: true

module JedNative
  class PagedPostings
    def initialize(index, rows)
      @index, @rows = index, rows
    end
    def [](term)
      entries = @index.entries
      position = entries.bsearch_index { |key, _| key >= term } || entries.length
      ordinals = []
      while position < entries.length
        key = entries[position][0]
        break unless key.start_with?(term)
        @index.pool.codec.gin_decode_run(term, key).each { |storage_key| ordinals << @rows.ordinal_for_key(storage_key) }
        position += 1
      end
      ordinals
    end
  end

  class PagedRowPositions
    def initialize(rows) = (@rows = rows)
    def fetch(key) = @rows.ordinal_for_key(key)
    alias [] fetch
  end

  module GistReader
    def read_gist_tree(image, page_size, root, operations)
      seen = Set.new
      count = height = 0
      decode_bound = lambda do |bytes|
        next bytes unless operations
        offset = 0
        bound = operations.map do |operation|
          if operation[:kind] == :range
            value, offset = decode_range_body(operation[:elem], bytes, offset)
            value
          else
            2.times.map do
              length, offset = take(bytes, offset, 2)
              value, offset = take(bytes, offset, length.unpack1('n'))
              value
            end
          end
        end
        raise Error.new('XX001', 'trailing GiST bound bytes') unless offset == bytes.bytesize
        bound
      end
      walk = lambda do |page, depth|
        next nil if page.zero?
        raise Error.new('XX001', 'GiST page reached twice') unless seen.add?(page)
        count += 1
        height = [height, depth].max
        raw = read_page(image, page_size, page)
        leaf = raw[:type] == FileFormat::PAGE_GIST_LEAF
        raise Error.new('XX001', 'expected a GiST page') unless leaf || raw[:type] == FileFormat::PAGE_GIST_INTERIOR
        offset = 0
        entries = Array.new(raw[:item_count]) do
          length, offset = take(raw[:payload], offset, 2)
          bytes, offset = take(raw[:payload], offset, length.unpack1('n'))
          bound = decode_bound.call(bytes)
          if leaf
            length, offset = take(raw[:payload], offset, 2)
            key, offset = take(raw[:payload], offset, length.unpack1('n'))
            [bound, key]
          else
            child, offset = take(raw[:payload], offset, 4)
            [bound, walk.call(child.unpack1('N'), depth + 1)]
          end
        end
        { page: page, leaf: leaf, leaf ? :entries : :children => entries }
      end
      tree = walk.call(root, 1)
      { tree: tree, node_count: count, height: height, opclasses: operations }
    end
  end
  FileFormat.include(GistReader)
end
