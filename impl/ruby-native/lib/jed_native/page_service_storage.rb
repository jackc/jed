# frozen_string_literal: true

require_relative 'page_service_owner'
require_relative 'page_service_client'

module JedNative
  module ServiceStorage
    DEFAULTS = { ownership: :prefer, yield_grace_ms: 5000, shutdown_grace_ms: 30_000,
                 idle_session_timeout_ms: 0, max_connections: 0, idle_in_transaction_timeout_ms: 60_000 }.freeze
    def initialize(**options)
      @service_role_mutex = Mutex.new
      @service_epoch = 0
      @service_timeout = options.fetch(:file_lock_timeout_ms, 5000)
      @service_options = DEFAULTS.to_h { |key, value| [key, options.key?(key) ? options.delete(key) : value] }
      @service_options[:ownership] = @service_options[:ownership].to_sym
      raise Error.new('22023', 'invalid ownership preference') unless %i[prefer yield never].include?(@service_options[:ownership])
      @service_options.each do |key, value|
        next if key == :ownership
        raise Error.new('22023', "#{key} must be nonnegative") unless value.is_a?(Integer) && value >= 0
      end
      super(**options)
    end
    def service_role
      return :client if @service_options[:ownership] == :never && !@creating
      service_ensure_role
      @service_remote ? :client : :owner
    end
    # An internal equality token, not a wire/file field. Keep failure live: a
    # disconnected client invalidates its cursors before role reselection.
    def service_generation = (@service_epoch << 1) | (@service_remote&.connection&.failure ? 1 : 0)
    def service_finish_open
      ensure_pid
      @service_remote&.finish_open
    end
    def service_begin_read(token)
      service_ensure_role
      @service_remote ? @service_remote.begin_read(token) : @service_owner&.pin_local(token)
      @service_remote&.finish_open
    rescue Error => error
      raise unless error.sqlstate == '08006' && (@service_reselect || @service_remote&.connection&.failure)
      service_ensure_role
      @service_remote ? @service_remote.begin_read(token) : @service_owner&.pin_local(token)
      @service_remote&.finish_open
    end
    def service_copy_pin(source, target)
      ensure_pid
      @service_remote ? @service_remote.copy_pin(source, target) : @service_owner&.copy_local_pin(source, target)
    end
    def service_end_read(token)
      ensure_pid
      @service_remote&.end_read(token) unless @service_remote&.connection&.failure
      @service_owner&.unpin_local(token)
    end
    def service_acquire(token, timeout_ms)
      service_ensure_role
      @service_remote ? @service_remote.begin_write(token, timeout_ms) : @service_owner&.acquire(token, timeout_ms)
      @service_remote&.finish_open
    rescue Error => error
      raise unless error.sqlstate == '08006' && (@service_reselect || @service_remote&.connection&.failure)
      service_ensure_role
      @service_remote ? @service_remote.begin_write(token, timeout_ms) : @service_owner&.acquire(token, timeout_ms)
      @service_remote&.finish_open
    end
    def service_release(token)
      ensure_pid
      return if @service_remote&.connection&.failure
      @service_remote ? @service_remote.end_write(token) : @service_owner&.release(token)
      @service_remote&.finish_open if @service_resolved_token.equal?(token)
      @service_resolved_token = nil if @service_resolved_token.equal?(token)
    end
    def load
      if @foreign_snapshot
        @mutex.synchronize do
          if @foreign_snapshot
            reader = @codec.reader
            @snapshot = reader.decode_image(@image, page_size: @page_size, lazy: true, meta: @meta)
            @foreign_snapshot = false
          end
        end
      end
      super
    end
    def close
      ensure_pid
      @service_owner&.shutdown unless @closed || @poisoned
      super
    end

    private

    def service_open_client
      @service_remote.open
    rescue Error => error
      # An accepted ownership request may close the serving connection before
      # this opener obtains its first pin. No caller-visible snapshot exists
      # yet, so re-election is transparent.
      raise unless error.sqlstate == '08006' && @service_remote&.connection&.failure
      service_ensure_role
    end

    def resolve_service_commit(txid, digest, token)
      service_ensure_role
      status = if @service_remote
        name, reply = @service_remote.connection.request(:commit_status, txid: txid, digest: digest)
        raise Error.new('08P01', 'expected commit_status_reply') unless name == :commit_status_reply
        reply[:status]
      else
        known = @service_owner.instance_variable_get(:@resolution)[txid]
        known ? (known == digest ? 0 : 1) : (txid > @meta[:txid] ? 1 : 2)
      end
      raise Error.new(status == 1 ? '08006' : '08007', 'commit outcome could not be confirmed') unless status.zero?
      @service_owner&.acquire(token, 0)
      @service_resolved_token = token
      @meta
    rescue Error => error
      raise error if %w[08007 08006].include?(error.sqlstate) && defined?(status)
      raise Error.new('08007', "commit outcome is unknown: #{error.message}")
    end

    def service_ensure_role
      ensure_pid
      return unless @service_reselect || @service_remote&.connection&.failure
      @service_role_mutex.synchronize do
        return unless @service_reselect || @service_remote&.connection&.failure
        deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + @service_timeout / 1000.0
        loop do
          @service_remote&.close
          @service_remote = nil
          @service_reselect = false
          @creating = false
          @service_epoch += 1
          remaining = [(deadline - Process.clock_gettime(Process::CLOCK_MONOTONIC)) * 1000, 0].max.to_i
          acquire_lock(remaining)
          if @service_remote
            begin
              @service_remote.open
            rescue Error => error
              # A yielding transport can disappear again while election is
              # underway. No snapshot was exposed by this attempt: retry only
              # within the original open deadline.
              raise unless error.sqlstate == '08006' && @service_remote.connection.failure && Process.clock_gettime(Process::CLOCK_MONOTONIC) < deadline
              next
            end
          else
            @file = File.open(@path, @read_only ? 'rb' : 'r+b')
            @file.close_on_exec = true
            promote_with_held_key
            @snapshot = decode_file
            @meta, @free_pages = @snapshot.values_at(:meta, :free_pages)
            reset_durability_floor(@meta)
            @free_generation = @meta[:txid]
            @live_at_compaction = @meta[:page_count] - @free_pages.length
            @page_ids, @overflow_ids = {}.compare_by_identity, {}.compare_by_identity
            @needs_stabilization = @needs_directory_sync = true
            initialize_service_owner
          end
          break
        end
      rescue Error => error
        @service_reselect = true
        raise if %w[28P02 28P04].include?(error.sqlstate)
        raise Error.new('08006', "could not re-elect page-service owner: #{error.message}")
      end
    end
    def yield_service_ownership(owner)
      flush
      @service_role_mutex.synchronize do
        return unless @service_owner.equal?(owner)
        @service_owner = nil
        @service_reselect = true
        @service_epoch += 1
        owner.close
        @file.close unless @file.closed?
        File.unlink(File.join(@bundle, 'owner')) rescue nil
        @presence.close unless @presence.closed?
        @presence, @owns_presence = nil, false
        @service_pause_until = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 2.0
      end
      service_ensure_role
    rescue Error
      @service_reselect = true
    end

    def service_credential = @credential
    def service_key_random = @key_random

    # A client that promotes itself opens the file with the data key it holds, not its credential
    # (encrypted.md §14): it adopts the file's selected key-block copy, leaves a differing pair
    # unrepaired, and fails 28P04 when the held key authenticates neither meta slot (a rekey since).
    def promote_with_held_key
      slots = [0, 1].map { |slot| @file.pread(@page_size, slot * @page_size) }
      suite = JedNative.file_suite(*slots)
      crypto = @codec.crypto
      unless crypto
        # An open that becomes the owner before it ever unlocked (the owner yielded during the open)
        # unlocks with its credential like a local open.
        return unlock_file if @credential && @meta.nil?
        raise Error.new('28P02', 'the database was encrypted since this handle opened it') unless suite.zero?
        return
      end
      raise Error.new('28P04', 'the database was decrypted since this handle opened it; reopen it') if suite.zero?
      crypto.key_block = KeyBlock.select(*[2, 3].map { |page| @file.pread(@page_size, page * @page_size) })
      image = FileImage.new(@file, @page_size)
      candidates = [0, 1].filter_map { |slot| @codec.read_meta(image, @page_size, slot) }
      selected = candidates.max_by { |meta| meta[:txid] }
      return unless selected
      stored = image.byteslice(selected[:slot_index] * @page_size, @page_size)
      manifest = (selected[:manifest_pages] || []).map { |page| [page, image.byteslice(page * @page_size, @page_size)] }
      unless crypto.meta_authentic?(selected[:slot_index], stored, manifest)
        raise Error.new('28P04', 'the database was rekeyed since this handle opened it; reopen it')
      end
    end

    def initialize_service_owner
      @service_owner = PageServiceOwner.new(self, @service_options)
      @service_owner.start
    end
    def try_page_service_client
      raise Error.new('55006', 'coordinated access rejects hard-linked database files') if File.stat(@path).nlink != 1
      bytes = File.binread(File.join(@bundle, 'owner'))
      record = PageServiceWire.decode_owner(bytes, @codec)
      return false unless record[:uid] == Process.euid && record[:flags] == 1 && record[:endpoint].start_with?('/')
      @service_remote = PageServiceClient.new(self, UNIXSocket.new(record[:endpoint]))
      true
    rescue Errno::ENOENT, Errno::ECONNREFUSED, Errno::EACCES, IOError
      false
    rescue Error => error
      raise if %w[0A000 53300 55006].include?(error.sqlstate)
      false
    end
    def accept_service_commit(delta)
      @mutex.synchronize do
        ensure_open(write: true)
        meta, bytes = delta.values_at(:meta, :bytes)
        raise Error.new('08P01', 'writer snapshot changed') unless meta[:txid] == @meta[:txid] + 1
        indices = delta[:expected].keys + delta[:manifest]
        # Check the linked free inventory before the publication point. The body
        # is already on disk, but remains unreachable from committed meta.
        @image.invalidate(indices)
        slot = (@meta[:slot] || @meta[:txid] & 1) ^ 1
        if @codec.encrypted?
          # The client's meta authenticated under the database key; reseal it for the slot written.
          bytes = @codec.crypto.seal_meta(slot, bytes.dup, delta[:manifest_pages])
          @codec.crypto.note(meta[:root_page], meta[:root_tag])
          @codec.crypto.note(meta[:free_head], meta[:free_tag])
        end
        free = @codec.validate_free_list!(@image, @page_size, meta)
        write_at(@file, bytes, slot * @page_size)
        durable_sync(@file)
        @meta = meta.merge(slot: slot)
        reset_durability_floor(@meta)
        # Clients only consume the owner's free list. They never add newly
        # orphaned pages, so the reuse watermark remains its rebuild generation.
        @free_pages = free
        @page_ids, @overflow_ids = {}.compare_by_identity, {}.compare_by_identity
        @snapshot, @foreign_snapshot = nil, true
        @image.invalidate([slot])
        @service_owner.publish(@meta, bytes, indices)
      rescue SystemCallError, IOError => error
        @poisoned = true
        raise Error.new('58030', error.message)
      end
    end

    def service_maintenance
      unlisted = @meta[:page_count] - @free_pages.length
      rebuild = @live_at_compaction.zero? || unlisted > FreeListPlan::COMPACT_MIN_PAGES && unlisted > FreeListPlan::COMPACT_GROWTH * @live_at_compaction
      truncate = @free_pages.last == @meta[:page_count] - 1 && effective_reader_watermark >= @free_generation
      return unless rebuild || truncate

      # Reclamation is structural. Reuse the decoded, immutable physical roots
      # without planning any persisted SQL or evaluating index expressions.
      snapshot = load
      tables = snapshot[:tables].map do |table|
        indexes = table[:indexes].map do |index|
          kind = index[:kind] == 'gist' ? :physical_gist : :physical_tree
          profile = index[:kind] == 'gist' ? index[:gist_profile] : index[:tree_profile]
          index.merge(kind => profile&.fetch(:tree))
        end
        table.merge(physical_tree: table[:tree_profile]&.fetch(:tree), indexes: indexes)
      end
      commit(snapshot.merge(tables: tables))
    end
  end
  Storage.prepend(ServiceStorage)
end
