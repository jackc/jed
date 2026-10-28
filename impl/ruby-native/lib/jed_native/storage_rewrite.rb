# frozen_string_literal: true

module JedNative
  # Rewrites existing nodes without rebalancing them. The sink owns each page
  # immediately, so a file rewrite never buffers the complete database image.
  # In an encrypted file the codec seals each page as it is emitted — post-order,
  # chains last page first — with fresh nonces; the reader `source` codec opens
  # the source image's pages (encrypted.md §7, §13).
  class PhysicalImage
    def initialize(codec, page_size, source_image: nil, source_codec: codec)
      @codec, @page_size, @source_image, @source_codec = codec, page_size, source_image, source_codec
      @capacity, @next_page = codec.payload_capacity(page_size), codec.first_body
    end

    def write(snapshot, txid, &sink)
      raise Error.new('54000', 'transaction id overflow') unless txid.between?(0, 0xFFFF_FFFF_FFFF_FFFF)
      @sink = sink
      if @codec.encrypted?
        block = @codec.crypto.key_block_page(@page_size)
        @sink.call(2, block)
        @sink.call(3, block)
      end
      @codec.instance_variable_set(:@ctypes, snapshot.fetch(:types, []).to_h { |type| [type[:name], type[:fields]] })
      entries = []
      snapshot.fetch(:types, []).sort_by { |type| type[:name].b }.each { |type| entries << "\x01".b + @codec.composite_type_entry_bytes(type) }
      snapshot.fetch(:sequences, []).sort_by { |sequence| sequence[:name].b }.each { |sequence| entries << "\x02".b + @codec.sequence_entry_bytes(sequence) }
      snapshot.fetch(:collations, []).sort_by { |collation| collation[:name].b }.each { |collation| entries << "\x03".b + @codec.collation_entry_bytes(collation) }
      tables = snapshot.fetch(:tables, []).sort_by { |table| table[:name].b }
      tables.each do |table|
        tree = table[:physical_tree] || table.dig(:tree_profile, :tree)
        raise Error.new('XX001', 'nonempty table has no physical tree') if !tree && (table[:row_count] || table[:rows].length).positive?
        root = write_tree(tree)
        roots = table.fetch(:indexes, []).map do |index|
          if index[:kind] == 'gist'
            gist = index[:physical_gist] || index.dig(:gist_profile, :tree)
            operations = index.dig(:gist_profile, :opclasses)
            operations ||= @codec.gist_opclasses_for(table, index) if gist && !@source_image
            write_gist(gist, operations)
          else
            write_tree(index[:physical_tree] || index.dig(:tree_profile, :tree))
          end
        end
        entries << "\x00".b + @codec.table_entry_bytes(table, root, roots, table[:row_count] || table[:rows].length)
      end
      tables.each { |table| entries.concat(@codec.statistics_entries(table)) }
      snapshot.fetch(:views, []).sort_by { |view| view[:name].b }.each { |view| entries << "\x05".b + @codec.view_entry_bytes(view) }
      catalog = @codec.catalog_layout(entries, @codec.chain_capacity(@capacity))
      root = @next_page
      ids = catalog.map { allocate }
      @codec.catalog_out(self, @page_size, ids, catalog)
      meta = { txid: txid, root_page: root, page_count: @next_page, live: @next_page - @codec.first_body, free_head: 0, protected: [], slot: txid & 1 }
      self.class.meta_pages(@codec, @page_size, meta).each { |page, bytes| @sink.call(page, bytes) }
      meta
    end

    # Both meta slots of a bootstrap meta (no manifest), each sealed for its own slot when encrypted.
    def self.meta_pages(codec, page_size, meta)
      buffer = PageBuffer.new(page_size, [codec.first_body + 1, meta[:page_count]].max)
      2.times do |slot|
        codec.write_meta(buffer, page_size, slot, page_size, meta[:txid], meta[:root_page], meta[:page_count], meta[:live], seal: false)
        codec.write_meta_free_head(buffer, page_size, slot, meta[:free_head] || 0)
        codec.finish_meta(buffer, page_size, slot)
      end
      buffer.pages
    end

    # FileFormat's page emitters use an indexed page sink: a descriptor, or sealed bytes.
    def []=(page, descriptor)
      return @sink.call(page, descriptor) if descriptor.is_a?(String)

      kind, count, payload, following, flags = descriptor
      emit(page, kind, count, payload, following || 0, flags: flags || 0)
    end

    private

    def allocate
      raise Error.new('54000', 'database page count overflow') if @next_page >= 0xFFFF_FFFF
      page = @next_page
      @next_page += 1
      page
    end

    def emit(page, kind, count, payload, following = 0, flags: 0)
      buffer = PageBuffer.new(@page_size, [@codec.first_body + 1, page + 1].max)
      @codec.write_page(buffer, @page_size, page, kind, count, following, payload, flags: flags)
      @sink.call(page, buffer.pages.fetch(page))
    end

    def write_tree(node)
      return 0 unless node
      children = node[:children].map { |child| write_tree(child) }
      page = allocate
      if children.empty?
        if node.is_a?(PagedLeaf)
          raw = node.pool.page(node.page)
          payload = rewrite_leaf(raw[:payload].dup, raw[:item_count], node.columns, node.pool)
          emit(page, FileFormat::PAGE_LEAF, raw[:item_count], payload)
        else
          payload = @codec.emit_leaf_pax(node[:recs], @capacity, -> { allocate }, self)
          emit(page, FileFormat::PAGE_LEAF, node[:keys].length, payload)
        end
      else
        emit(page, FileFormat::PAGE_INTERIOR, node[:keys].length, @codec.emit_interior(node[:keys], children))
      end
      page
    end

    def rewrite_leaf(payload, count, columns, pool)
      @codec.parse_pax_leaf(payload, count, columns)
      key_bytes = count.zero? ? 0 : payload.byteslice(4 * (count - 1), 4).unpack1('N')
      directory = 4 * count + key_bytes
      starts = payload.byteslice(directory, 4 * (columns.length + 1)).unpack('N*')
      count.times do |row|
        columns.each_with_index do |column, index|
          next if @codec.fixed_width(column[:type])
          start = starts[index]
          previous = row.zero? ? 0 : payload.byteslice(start + 1 + 4 * (row - 1), 4).unpack1('N')
          finish = payload.byteslice(start + 1 + 4 * row, 4).unpack1('N')
          next if previous == finish
          offset = start + 1 + 4 * count + previous
          tag = payload.getbyte(offset)
          next unless [FileFormat::TAG_EXTERNAL, FileFormat::TAG_EXTERNAL_COMP].include?(tag)
          @source_codec.note_external(payload.byteslice(offset, finish - previous))
          original = payload.byteslice(offset + 1, 4).unpack1('N')
          length = payload.byteslice(offset + 5, 4).unpack1('N')
          first = copy_overflow(pool, original, length)
          payload[offset + 1, 4] = [first].pack('N')
          if @codec.encrypted?
            width = tag == FileFormat::TAG_EXTERNAL ? FileFormat::EXTERNAL_PTR_LEN : FileFormat::EXTERNAL_COMP_PTR_LEN
            payload[offset + width, Crypto::TAG] = @codec.tag_of(first)
          end
        end
      end
      payload
    end

    def copy_overflow(pool, original, remaining)
      seen = Set.new
      slabs = []
      until original.zero?
        raise Error.new('XX001', 'overflow chain cycle') unless seen.add?(original)
        raw = pool.page(original)
        count = raw[:item_count]
        unless raw[:type] == FileFormat::PAGE_OVERFLOW && count.positive? && count <= @capacity && count <= remaining
          raise Error.new('XX001', 'invalid overflow chain')
        end
        remaining -= count
        following = raw[:next_page]
        raise Error.new('XX001', 'overflow length mismatch') unless following.zero? == remaining.zero?
        slabs << raw[:payload].byteslice(0, count)
        original = following
      end
      raise Error.new('XX001', 'overflow length mismatch') unless remaining.zero?
      ids = slabs.map { allocate }
      # Last page first: an encrypted chain page binds its successor's tag.
      (slabs.length - 1).downto(0) do |index|
        emit(ids[index], FileFormat::PAGE_OVERFLOW, slabs[index].bytesize, slabs[index], ids[index + 1] || 0)
      end
      ids.first
    end

    def write_gist(node, operations)
      return 0 unless node
      children = node[:leaf] ? [] : node[:children].map { |_, child| write_gist(child, operations) }
      page = allocate
      if @source_image && node[:page]
        raw = @source_codec.read_page(@source_image, @page_size, node[:page])
        payload = raw[:payload].dup
        unless node[:leaf]
          rebuilt = +''.b
          offset = 0
          children.each do |child|
            length = payload.byteslice(offset, 2).unpack1('n')
            rebuilt << payload.byteslice(offset, 2 + length) << @codec.pointer(child)
            offset += 2 + length + 4
          end
          payload = rebuilt
        end
        emit(page, raw[:type], raw[:item_count], payload)
      else
        payload = +''.b
        values = node[:leaf] ? node[:entries] : node[:children]
        values.each_with_index do |(bound, key), index|
          bytes = @codec.gist_encode_bound(operations, bound)
          payload << [bytes.bytesize].pack('n') << bytes
          payload << (node[:leaf] ? [key.bytesize].pack('n') + key : @codec.pointer(children[index]))
        end
        emit(page, node[:leaf] ? FileFormat::PAGE_GIST_LEAF : FileFormat::PAGE_GIST_INTERIOR, values.length, payload)
      end
      page
    end
  end

  module StorageRewrite
    # A node-preserving image of snapshot into a new file. An encrypted one keeps the data key and the
    # selected key-block copy and draws fresh nonces from the database's stream; its pointer tags are
    # the new file's (writer.crypto, adopted when the file replaces this one).
    def physical_scratch(snapshot = @snapshot, txid = @meta[:txid] + 1, writer: image_writer, &sink)
      PhysicalImage.new(writer, @page_size, source_image: @image, source_codec: @codec).write(snapshot, txid, &sink)
    end

    def compact
      ensure_open(write: true)
      raise Error.new('55006', 'a page client cannot compact its owner') if @service_remote
      operation = lambda do
        flush(shrink: false) if respond_to?(:flush)
        load
        @mutex.synchronize { compact_file }
      end
      @service_owner ? @service_owner.with_exclusive_operation(&operation) : operation.call
    end

    def backup(destination, form: :page_identical)
      ensure_open
      raise Error.new('22023', 'unknown backup form') unless %i[page_identical compacted].include?(form)
      captured = nil
      loop do
        load
        captured = @mutex.synchronize { [@snapshot, @meta.dup, @image] }
        break if captured.first
      end
      snapshot, selected_meta, source_image = captured
      codec = @codec.reader
      destination = File.join(File.realpath(File.dirname(destination)), File.basename(destination))
      raise Error.new('58P02', 'backup destination exists') if File.exist?(destination) || File.symlink?(destination)
      temporary = "#{destination}.jedtmp"
      published = false
      begin
        flags = File::WRONLY | File::CREAT | File::TRUNC | File::NOFOLLOW
        File.open(temporary, flags, 0o600) do |output|
          if form == :compacted
            meta = PhysicalImage.new(image_writer(current_key_block), @page_size, source_image: source_image, source_codec: codec).write(snapshot, selected_meta[:txid] + 1) do |page, bytes|
              write_at(output, bytes, page * @page_size)
            end
          else
            meta = selected_meta
            reached = snapshot.fetch(:live_pages).to_set
            raise Error.new('XX001', 'backup live page count mismatch') unless reached.length == meta[:live]
            page = meta[:free_head] || 0
            until page.zero?
              raise Error.new('XX001', 'free-list chain cycle or overlap') unless reached.add?(page)
              raw = codec.read_page(source_image, @page_size, page)
              raise Error.new('XX001', 'expected free-list page') unless raw[:type] == 7
              page = raw[:next_page]
            end
            zero = "\0".b * @page_size
            if codec.encrypted?
              # Both key-block pages receive the selected copy (encrypted.md §13).
              block = current_key_block.ljust(@page_size, "\0")
              write_at(output, block, 2 * @page_size)
              write_at(output, block, 3 * @page_size)
            end
            (codec.first_body...meta[:page_count]).each do |id|
              codec.read_page(source_image, @page_size, id) if reached.include?(id)
              bytes = reached.include?(id) ? source_image.byteslice(id * @page_size, @page_size) : zero
              write_at(output, bytes, id * @page_size)
            end
            PhysicalImage.meta_pages(codec, @page_size, meta).each { |id, bytes| write_at(output, bytes, id * @page_size) }
          end
          output.truncate(meta[:page_count] * @page_size)
          durable_sync(output) if @sync
        end
        File.link(temporary, destination)
        published = true
        File.unlink(temporary)
        File.open(File.dirname(destination), File::RDONLY) { |directory| directory.fsync } if @sync
      rescue Errno::EEXIST
        raise Error.new('58P02', 'backup destination exists')
      rescue SystemCallError, IOError => error
        File.unlink(destination) if published && File.exist?(destination)
        raise Error.new('58030', error.message)
      rescue Error
        raise
      rescue StandardError => error
        raise Error.new('XX001', error.message)
      ensure
        File.unlink(temporary) if temporary && File.exist?(temporary)
      end
      nil
    end

    private

    def rewrite_codec = @codec.reader(crypto: nil)

    # The codec that writes a new file of this database: the same keys with a fresh pointer namespace.
    def image_writer(key_block = nil)
      return rewrite_codec unless @codec.encrypted?

      @codec.reader(crypto: @codec.crypto.fork(key_block || @codec.crypto.key_block))
    end

    # The file's selected key-block copy, read from pages 2 and 3 (a page client that unlocked before
    # a credential change copies the current pair — encrypted.md §13).
    def current_key_block
      return nil unless @codec.encrypted?

      KeyBlock.select(*[2, 3].map { |page| @image.byteslice(page * @page_size, @page_size) })
    end

    def compact_file
      temporary = "#{@path}.jedtmp"
      renamed = false
      writer = image_writer
      begin
        mode = @file.stat.mode & 0o7777
        File.open(temporary, File::WRONLY | File::CREAT | File::TRUNC | File::NOFOLLOW, mode) do |output|
          output.chmod(mode)
          meta = physical_scratch(writer: writer) { |page, bytes| write_at(output, bytes, page * @page_size) }
          output.truncate(meta[:page_count] * @page_size)
          durable_sync(output) if @sync
        end
        @file.close
        File.rename(temporary, @path)
        renamed = true
        @file = File.open(@path, 'r+b')
        @file.close_on_exec = true
        sync_directory if @sync
        @codec.crypto = writer.crypto if writer.encrypted?
        @snapshot = decode_file
        @meta = @snapshot.fetch(:meta)
        @page_ids, @overflow_ids = {}.compare_by_identity, {}.compare_by_identity
        @free_pages, @free_generation = [], @meta[:txid]
        @live_at_compaction = @meta[:live]
        @needs_stabilization = @needs_directory_sync = false
        reset_durability_floor(@meta) if respond_to?(:reset_durability_floor, true)
        @service_owner&.replace_image(@meta, @image.byteslice((@meta[:slot] || 0) * @page_size, @page_size))
        @meta
      rescue SystemCallError, IOError => error
        if renamed
          @poisoned = true
        elsif @file.closed?
          begin
            @file = File.open(@path, 'r+b')
            @file.close_on_exec = true
          rescue SystemCallError, IOError
            @poisoned = true
          end
        end
        raise Error.new('58030', error.message)
      rescue Error
        @poisoned = true if renamed
        raise
      rescue StandardError => error
        @poisoned = true if renamed
        raise Error.new('XX001', error.message)
      ensure
        File.unlink(temporary) if File.exist?(temporary)
      end
    end
  end
  Storage.prepend(StorageRewrite)
end
