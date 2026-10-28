# frozen_string_literal: true

module JedNative
  class PageServiceClient
    attr_reader :connection, :page_size, :connection_id
    def initialize(storage, socket)
      @storage = storage
      @codec = storage.instance_variable_get(:@codec)
      @connection = PageServiceConnection.new(socket)
      name, hello = @connection.request(:hello, protocol_min: 1, protocol_max: 1, format_version: FileFormat::VERSION,
                                               flags: storage.read_only? ? 1 : 0, client_name: 'jed-ruby-native', credential: nil)
      raise Error.new('08P01', 'expected hello_ok') unless name == :hello_ok && hello[:protocol] == 1 && hello[:format_version] == FileFormat::VERSION && @codec.valid_page_size?(hello[:page_size])
      @page_size, @connection_id, @owner_flags = hello.values_at(:page_size, :connection_id, :flags)
      @connection.page_size = @page_size
      @connection.notice_handler = lambda do |notice|
        if notice[:kind] == 1 && notice[:connection_id] != @connection_id
          @yield_preferred_other = true
        elsif notice[:kind] == 2
          @yield_preferred_other = false
          @storage.instance_variable_set(:@service_pause_until, nil)
        end
      end
      @connection.failure_handler = lambda do
        @storage.instance_variable_set(:@service_pause_until, Process.clock_gettime(Process::CLOCK_MONOTONIC) + 2.0) if @yield_preferred_other
      end
      @incarnation, @txid = "\0".b * 16, 0
      @tokens, @counts = {}, {}
      @state_mutex, @adopt_mutex = Mutex.new, Mutex.new
      @image = RemotePageImage.new(self, @page_size, storage.instance_variable_get(:@cache_bytes))
      @open_token = Object.new
      preference = storage.instance_variable_get(:@service_options)[:ownership]
      @connection.request(:request_ownership) if preference == :prefer && @owner_flags & 4 == 4
    rescue Exception
      @connection&.close
      raise
    end

    def open
      @storage.instance_variable_set(:@page_size, @page_size)
      @storage.instance_variable_set(:@image, @image)
      begin_read(@open_token)
    end
    def finish_open = end_read(@open_token)
    def current = { incarnation: @incarnation, last_txid: @txid }
    def begin_read(token)
      return if @state_mutex.synchronize { @tokens.key?(token) }
      name, reply = @connection.request(:begin_read, current)
      raise Error.new('08P01', 'expected read_begun') unless name == :read_begun
      @adopt_mutex.synchronize do
        @state_mutex.synchronize do
          txid = reply[:txid]
          if @counts[txid]
            @connection.request(:end_read, { txid: txid }, reply: false)
          end
          @counts[txid] = @counts.fetch(txid, 0) + 1
          @tokens[token] = txid
        end
        adopt(reply)
      end
    end
    def copy_pin(source, target)
      @state_mutex.synchronize do
        return if @tokens.key?(target)
        if (txid = @tokens[source])
          @counts[txid] += 1
          @tokens[target] = txid
          return
        end
      end
      begin_read(target)
    end
    def end_read(token)
      @state_mutex.synchronize do
        txid = @tokens.delete(token)
        return unless txid
        @counts[txid] -= 1
        if @counts[txid].zero?
          @counts.delete(txid)
          @connection.request(:end_read, { txid: txid }, reply: false)
        end
      end
    end
    def begin_write(token, timeout_ms)
      if @owner_flags & 2 == 2 && @storage.instance_variable_get(:@service_options)[:ownership] != :never && !@storage.read_only?
        @connection.request(:request_ownership)
        deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + timeout_ms / 1000.0 if timeout_ms.positive?
        until @connection.failure
          raise Error.new('55P03', 'ownership handoff timeout') if deadline && Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline
          sleep 0.005
        end
        raise Error.new('08006', 'owner yielded to a writer')
      end
      # flags bit0 (relaxed) stays clear: this client commits synchronously through its owner even when
      # its handle runs synchronous = off, so the owner flushes its batch first and names no pending set.
      name, reply = @connection.request(:begin_write, current.merge(lock_timeout_ms: timeout_ms, flags: 0))
      raise Error.new('08P01', 'expected write_begun') unless name == :write_begun
      @state_mutex.synchronize { @write_token, @write_txid, @grant = token, reply[:txid], reply[:reuse_grant] }
      @adopt_mutex.synchronize { adopt(reply) }
    end
    def end_write(token)
      committed = false
      @state_mutex.synchronize do
        return unless @write_token.equal?(token)
        @connection.request(:abort, {}, reply: false) if @write_txid
        committed = !@write_txid
        @write_token, @write_txid = nil, nil
      end
      if committed
        @pending_overlay&.publish
        @pending_overlay = nil
      end
      end_read(@commit_token) if @commit_token
      @commit_token = nil
    end
    def read_pages(indices)
      @state_mutex.synchronize do
        txid = (@counts.keys + [@write_txid].compact).max
        raise Error.new('08006', 'no live page-service pin') unless txid
        name, reply = @connection.request(:read_pages, txid: txid, indices: indices)
        raise Error.new('08P01', 'expected pages') unless name == :pages && reply[:pages].length == indices.length
        reply[:pages]
      rescue Error => error
        # Reporting revocation consumes the owner's write transaction. Cleanup
        # must not send the abort reserved for a still-open writer gate.
        @write_txid, @pending_overlay = nil, nil if error.sqlstate == '25P03'
        raise
      end
    end

    # The variant of a served meta against the key this handle holds (encrypted.md §14). A first open
    # of an encrypted database unlocks with the handle's credential from the key-block pages read
    # through the service; a later connection keeps the data key it holds and adopts the selected
    # key-block copy. A plaintext meta where a key is held, or one the held key cannot authenticate, is
    # 28P04 (a rekey since); an encrypted meta where none is held is 28P02.
    def check_variant(bytes)
      encrypted = bytes.getbyte(6).to_i != 0
      crypto = @codec.crypto
      opened = !@storage.meta.nil?
      if crypto
        raise Error.new('28P04', 'the database was rekeyed since this handle opened it; reopen it') unless encrypted
        unless @key_block_read
          crypto.key_block = KeyBlock.select(*read_pages([2, 3]))
          @key_block_read = true
        end
      elsif encrypted
        credential = @storage.send(:service_credential)
        if opened || credential.nil?
          raise Error.new('28P02', 'the database is encrypted; a credential is required')
        end
        @codec.crypto = PageCodec.unlock(*read_pages([2, 3]), credential, @storage.send(:service_key_random))
        @key_block_read = true
      elsif !opened && @storage.send(:service_credential)
        raise Error.new('28P03', 'a credential was given for a database that is not encrypted')
      end
    end

    # The served meta must authenticate under the held key for one of the two slots (it carries no
    # slot), its manifest-overflow chain read through the service; its pointers' tags are recorded.
    def authenticate_served(bytes, meta)
      head, _, chain_count = bytes.byteslice(36, 12).unpack('N3')
      chain = []
      page = head
      chain_count.times do
        break if page.zero? || chain.length >= 4096
        block = read_pages([page]).first
        chain << [page, block]
        page = block.byteslice(8, 4).unpack1('N')
      end
      unless [0, 1].any? { |slot| @codec.crypto.meta_authentic?(slot, bytes, chain) }
        raise Error.new('28P04', 'the database was rekeyed since this handle opened it; reopen it')
      end
      @codec.crypto.note(meta[:root_page], meta[:root_tag])
      @codec.crypto.note(meta[:free_head], meta[:free_tag])
    end

    def adopt(reply)
      bytes = reply[:meta]
      check_variant(bytes)
      high = bytes.byteslice(24, 4).unpack1('N')
      image = PageBuffer.new(@page_size, high)
      image[0, @page_size] = bytes
      meta = @codec.read_meta(image, @page_size, 0, manifest: false)
      raise Error.new('08P01', 'invalid owner meta') unless meta && meta[:txid] == reply[:txid]
      changed = reply[:invalidate] == 1 || @txid != reply[:txid] || @incarnation != reply[:incarnation]
      @image.adopt(bytes, high, reply[:invalidate] == 1 ? nil : reply[:indices])
      authenticate_served(bytes, meta) if @codec.encrypted?
      @incarnation, @txid = reply.values_at(:incarnation, :txid)
      return unless changed
      meta[:slot] = meta[:txid] & 1
      reader = @codec.reader
      snapshot = reader.decode_image(@image, page_size: @page_size, lazy: true, meta: meta)
      @storage.instance_variable_set(:@meta, meta)
      @storage.instance_variable_set(:@snapshot, snapshot)
      @storage.instance_variable_set(:@free_pages, snapshot[:free_pages])
      @storage.instance_variable_set(:@free_generation, meta[:txid])
      @storage.instance_variable_set(:@live_at_compaction, meta[:live])
      @storage.instance_variable_set(:@page_ids, {}.compare_by_identity)
      @storage.instance_variable_set(:@overflow_ids, {}.compare_by_identity)
    end

    def commit(snapshot)
      raise Error.new('08006', 'client does not hold writer gate') unless @write_txid
      @storage.send(:ensure_open, write: true)
      # The writer pin protects preparation. Publication uses already decoded
      # routing metadata and private dirty pages, so it performs no page RPC
      # after acknowledgement and needs no additional old-generation pin.
      base = @storage.meta
      allocator = PageAllocator.new(base[:page_count], @grant ? @storage.instance_variable_get(:@free_pages) : [])
      plan = CommitPlan.new(@codec, snapshot, @page_size, allocator,
                            page_ids: @storage.instance_variable_get(:@page_ids) || {}.compare_by_identity,
                            overflow_ids: @storage.instance_variable_get(:@overflow_ids) || {}.compare_by_identity)
      maximum = @storage.instance_variable_get(:@max_storage_bytes)
      raise Error.new('54P06', 'database storage limit exceeded') if maximum.positive? && plan.live > base[:live] && plan.live * @page_size > maximum
      txid, slot = base[:txid] + 1, (base[:slot] || base[:txid] & 1) ^ 1
      free_plan = FreeListPlan.new(@codec, plan, @page_size, base[:live], txid, slot, truncate: false, reclaim: false,
                                  deferred_free: @grant ? [] : @storage.instance_variable_get(:@free_pages))
      pages = plan.pages
      meta_bytes = pages.pages.fetch(slot)
      overlay = PageOverlay.new(@image, pages)
      prepared = @storage.send(:published_snapshot, snapshot, plan, overlay)
      manifest_ids = free_plan.manifest_ids.to_set
      chain = pages.pages.select { |page, _| manifest_ids.include?(page) }
      ordered_chain = []
      page = meta_bytes.byteslice(36, 4).unpack1('N')
      until page.zero?
        bytes = chain.fetch(page)
        ordered_chain << [page, bytes]
        page = bytes.byteslice(8, 4).unpack1('N')
      end
      # Check the complete first frame before sending any part of a delta.
      PageServiceWire.encode(:commit_begin, 1, { txid: txid, meta: meta_bytes, manifest_pages: ordered_chain }, @page_size)
      @connection.request(:commit_begin, { txid: txid, meta: meta_bytes, manifest_pages: ordered_chain }, reply: false)
      body = pages.pages.keys.select { |index| index >= 2 && !manifest_ids.include?(index) }.sort
      batch = (PageServiceWire::MAX_FRAME - 9) / (@page_size + 4)
      body.each_slice(batch) do |indices|
        entries = indices.map { |index| [index, pages.pages.fetch(index)] }
        @connection.request(:commit_pages, { pages: entries }, reply: false)
      end
      begin
        name, reply = @connection.request(:commit_end)
      rescue Error => error
        if error.sqlstate == '25P03'
          @state_mutex.synchronize { @write_txid, @pending_overlay = nil, nil }
          raise
        end
        raise unless error.sqlstate == '08006'
        return @storage.send(:resolve_service_commit, txid, @codec.resolution_digest(meta_bytes), @write_token)
      end
      raise Error.new('08P01', 'invalid commit acknowledgement') unless name == :committed && reply[:txid] == txid
      meta = { txid: txid, slot: slot, root_page: plan.root, page_count: pages.page_count, free_head: free_plan.head,
               live: plan.live, protected: free_plan.protected }
      @txid = txid
      @image.adopt(meta_bytes, pages.page_count, pages.pages.keys)
      @pending_overlay = overlay
      @storage.instance_variable_set(:@meta, meta)
      @storage.instance_variable_set(:@snapshot, prepared.merge(meta: meta, free_pages: free_plan.free))
      @storage.instance_variable_set(:@free_pages, free_plan.free)
      @storage.instance_variable_set(:@free_generation, txid)
      @storage.instance_variable_set(:@page_ids, plan.adopt)
      @storage.instance_variable_set(:@overflow_ids, plan.overflow_ids)
      @state_mutex.synchronize { @write_txid = nil }
      meta
    end
    def close = @connection.close
    def discard_inherited = @connection.discard_inherited
  end
end
