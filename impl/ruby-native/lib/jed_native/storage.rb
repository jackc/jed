# frozen_string_literal: true

require_relative "error"
require_relative "file_format"
require_relative "paged_rows"
require_relative "paged_auxiliary"
require_relative "commit_plan"
require_relative "free_list_plan"

module JedNative
  # Experimental local-file host. It owns protocol-v2's crash-clean exclusive
  # lock and writes the shared v49 format. It does not serve page clients.
  # Commits encode dirty B+tree paths, preserve clean pages, and reclaim obsolete
  # generations under the validated-COW manifest protection contract.
  class Storage
    OPEN_PATHS = {}
    PATH_MUTEX = Mutex.new
    PAGE_SIZES = (8..16).map { |exponent| 1 << exponent }.freeze

    attr_reader :path, :page_size, :meta
    attr_writer :reader_watermark_provider
    def read_only? = @read_only
    def max_storage_bytes=(bytes)
      raise Error.new('22023', 'max_storage_bytes must be an integer') unless bytes.is_a?(Integer)
      @max_storage_bytes = [bytes, 0].max
    end

    def collation_key=(provider)
      @codec.instance_variable_set(:@collation_key, provider)
    end

    def initialize(path:, create: false, page_size: 8192, locking: :exclusive,
                   file_lock_timeout_ms: 5000, fsync: true, synchronous: :on, read_only: false,
                   max_storage_bytes: 0, cache_bytes: FileImage::DEFAULT_CACHE_BYTES, collation_key: nil, expression: nil)
      @codec = FileFormat.new(collation_key: collation_key, expression: expression)
      @pid = Process.pid
      @sync = fsync
      raise Error.new('22023', 'synchronous must be on or off') unless %i[on off].include?(synchronous.to_sym)
      @synchronous = synchronous.to_sym
      @durability_pending = {}
      @read_only = read_only
      unless cache_bytes.is_a?(Integer) && cache_bytes >= 0
        raise Error.new('22023', 'cache_bytes must be a nonnegative integer')
      end
      @cache_bytes = cache_bytes
      self.max_storage_bytes = max_storage_bytes
      @mutex = Mutex.new
      @closed = false
      @poisoned = false
      @claimed = false
      @creating = create
      @locking = locking.to_sym
      unless %i[auto exclusive none].include?(@locking)
        raise Error.new("22023", "unknown file locking mode")
      end
      unless file_lock_timeout_ms.is_a?(Integer) && file_lock_timeout_ms >= 0
        raise Error.new("22023", "file_lock_timeout_ms must be nonnegative")
      end
      unless @max_storage_bytes.is_a?(Integer) && @max_storage_bytes >= 0
        raise Error.new("22023", "max_storage_bytes must be nonnegative")
      end

      @path = if create
                File.join(File.realpath(File.dirname(path)), File.basename(path))
              else
                File.realpath(path)
              end
      claim_path
      acquire_lock(file_lock_timeout_ms) unless @locking == :none
      if @service_remote
        service_open_client
        return
      end
      if create
        raise Error.new("25006", "cannot create a read-only database") if @read_only
        raise Error.new("0A000", "invalid page size") unless @codec.valid_page_size?(page_size)
        raise Error.new("58P02", "database file already exists") if File.exist?(@path)

        @page_size = page_size
        if @max_storage_bytes.positive? && @page_size > @max_storage_bytes
          raise Error.new("54P06", "database storage limit exceeded")
        end
        publish_new(@codec.build_pages([], [], [], @page_size))
      end
      @file = File.open(@path, @read_only ? "rb" : "r+b")
      @file.close_on_exec = true
      if @locking != :none && @file.stat.nlink != 1
        raise Error.new("55006", "coordinated access rejects hard-linked database files")
      end
      @page_size ||= discover_page_size
      @snapshot = decode_file
      @meta = @snapshot.fetch(:meta)
      @free_generation = @meta[:txid]
      @free_pages = @snapshot.fetch(:free_pages)
      @live_at_compaction = @meta[:page_count] - @free_pages.length
      @needs_stabilization = !create
      @needs_directory_sync = !create
      initialize_service_owner if @locking == :auto
    rescue Error
      release_resources
      raise
    rescue Errno::ENOENT => error
      release_resources
      raise Error.new("58P01", error.message)
    rescue SystemCallError, IOError => error
      release_resources
      raise Error.new("58030", error.message)
    rescue StandardError => error
      release_resources
      raise Error.new("XX001", error.message)
    end

    def load
      ensure_open
      @snapshot
    end

    def to_image(snapshot = @snapshot)
      ensure_open
      build(snapshot).to_s
    end

    def flush(shrink: true)
      return if @service_remote
      @mutex.synchronize do
        ensure_open
        flush_pending(shrink: shrink)
      end
    end

    # Serialization completes before the first storage mutation. A size/codec
    # error leaves the handle usable; an I/O failure poisons it until reopen.
    def commit(snapshot)
      return @service_remote.commit(snapshot) if @service_remote
      @mutex.synchronize do
        ensure_open(write: true)
        @durability_floor_slot ||= @meta[:slot] || (@meta[:txid] & 1)
        plan = plan_commit(snapshot)
        pages, root, live = plan.pages, plan.root, plan.live
        if @max_storage_bytes.positive? && live > @meta[:live] && live * @page_size > @max_storage_bytes
          raise Error.new("54P06", "database storage limit exceeded")
        end
        txid = @meta.fetch(:txid) + 1
        raise Error.new("54000", "transaction id overflow") if txid > 0xFFFF_FFFF_FFFF_FFFF

        slot = (@durability_floor_slot || @meta[:slot] || (@meta[:txid] & 1)) ^ 1
        pending = @durability_pending
        free_plan = FreeListPlan.new(@codec, plan, @page_size, @live_at_compaction, txid, slot,
                                    truncate: pending.empty?, pending: pending,
                                    deferred_free: @durability_deferred_free || [])
        overlay = PageOverlay.new(@image, pages)
        prepared_snapshot = published_snapshot(snapshot, plan, overlay)
        begin
          if @needs_directory_sync
            sync_directory if @sync
            @needs_directory_sync = false
          end
          if @needs_stabilization
            durable_sync(@file) if @sync
            @needs_stabilization = false
          end
          pages.pages.keys.sort.each do |page|
            next if page < 2

            write_at(@file, pages.pages.fetch(page), page * @page_size)
          end
          write_at(@file, pages.pages.fetch(slot), slot * @page_size)
          durable_sync(@file) if @sync && @synchronous == :on
          if @synchronous == :on && @file.size > pages.page_count * @page_size
            begin
              @file.truncate(pages.page_count * @page_size)
            rescue SystemCallError, IOError
              # A completed commit is durable already. Excess trailing bytes
              # are outside its logical high-water and may be trimmed later.
            end
          end
        rescue SystemCallError, IOError => error
          @poisoned = true
          raise Error.new("58030", error.message)
        end
        @meta = { txid: txid, root_page: root, page_count: pages.page_count,
                  free_head: free_plan.head, live: live, slot: slot, protected: free_plan.protected }
        @page_ids = plan.adopt
        @overflow_ids = plan.overflow_ids
        @free_pages, @live_at_compaction = free_plan.free, free_plan.live_at_compaction
        @free_generation = txid
        @image.invalidate(pages.pages.keys)
        overlay.publish
        @snapshot = prepared_snapshot.merge(meta: @meta, free_pages: @free_pages)
        if @synchronous == :on
          reset_durability_floor(@meta)
        else
          pages.pages.each { |page, bytes| @durability_pending[page] = @codec.crc64(bytes) if page >= 2 && bytes.getbyte(0) != 8 }
        end
        @service_owner&.publish(@meta, pages.pages.fetch(slot), pages.pages.keys.reject { |page| page < 2 })
        if @synchronous == :off && @durability_pending.length >= (@page_size - 64) / 12
          begin
            flush_pending
          rescue Error
            # This commit has published. A failed cap flush revokes the handle;
            # subsequent operations and close surface the durability failure.
          end
        end
        @meta
      end
    end

    def close
      return if @closed

      ensure_pid
      flush unless @poisoned
      @closed = true
      poisoned = @poisoned
      release_resources
      raise Error.new("58030", "storage handle failed; reopen through recovery") if poisoned
    end

    private

    def reset_durability_floor(meta = @meta)
      @durability_floor_slot = meta[:slot] || (meta[:txid] & 1)
      @durability_pending = {}
      @durability_floor_free = nil
      @durability_deferred_free = nil
    end

    def flush_pending(shrink: true)
      return if @durability_pending.empty?
      durable_sync(@file) if @sync
      reset_durability_floor
      @file.truncate(@meta[:page_count] * @page_size) if shrink && @file.size > @meta[:page_count] * @page_size
    rescue SystemCallError, IOError => error
      @poisoned = true
      raise Error.new('58030', error.message)
    end

    # Adopt the already validated commit roots directly. The owner foreground
    # path neither rereads meta pages nor reparses SQL after its own publication.
    def published_snapshot(snapshot, plan, image)
      reader = FileFormat.new(collation_key: @codec.instance_variable_get(:@collation_key), expression: @codec.instance_variable_get(:@expression))
      reader.instance_variable_set(:@ctypes, snapshot.fetch(:types, []).to_h { |type| [type[:name], type[:fields]] })
      pool = LeafPool.new(reader, image, @page_size, image.cache_pages)
      previous = @snapshot.fetch(:tables, []).to_h { |table| [table[:name], table] }
      tables = plan.table_catalog.map do |entry|
        old = previous[entry[:name]]
        rows = if old && old[:root_data_page] == entry[:root_data_page] && old[:columns] == entry[:columns] && old[:rows].is_a?(PagedRows)
                 old[:rows]
               else
                 PagedRows.new(pool, entry[:root_data_page], entry[:columns], entry[:row_count])
               end
        entry[:indexes].each do |index|
          prior = old && old[:indexes].find { |item| item[:name] == index[:name] && item[:root_page] == index[:root_page] }
          if prior && (prior[:tree_profile] || prior[:gist_profile])
            index[:tree_profile], index[:gist_profile] = prior.values_at(:tree_profile, :gist_profile)
          elsif index[:kind] == 'gist'
            operations = index[:cols].all? { |column| column.is_a?(Integer) } ? reader.gist_opclasses_for(entry, index) : nil
            index[:gist_profile] = reader.read_gist_tree(image, @page_size, index[:root_page], operations)
          else
            index[:tree_profile] = PagedRows.new(pool, index[:root_page], [], nil).profile
          end
        end
        entry.merge(pk_order: entry[:pk], rows: rows, storage_keys: rows.keys, tree_profile: rows.profile)
      end
      snapshot.merge(tables: tables, meta: @meta, free_pages: @free_pages, live_pages: plan.live_pages)
    end

    def plan_commit(snapshot)
      reusable = effective_reader_watermark >= @free_generation
      @durability_floor_free = @free_pages.to_set if @durability_pending.empty?
      allowed = reusable ? @free_pages : []
      allowed = allowed.select { |page| @durability_floor_free.include?(page) } unless @durability_pending.empty?
      @durability_deferred_free = @free_pages - allowed
      CommitPlan.new(@codec, snapshot, @page_size, PageAllocator.new(@meta.fetch(:page_count), allowed),
                     page_ids: @page_ids || {}.compare_by_identity, overflow_ids: @overflow_ids || {}.compare_by_identity)
    rescue NotImplementedError => error
      raise Error.new('0A000', error.message)
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new('54000', "cannot encode database: #{error.message}")
    end

    def effective_reader_watermark
      # Service pins already include every local session and cursor. Avoid
      # taking Database's publication mutex while holding the storage mutex:
      # foreign-root adoption necessarily takes those locks in the other order.
      return @service_owner.oldest_pin if @service_owner
      local = @reader_watermark_provider ? @reader_watermark_provider.call : @meta[:txid] + 1
      local
    end

    def build(snapshot, start_page: FileFormat::ROOT_PAGE)
      @codec.build_pages(snapshot[:types] || [], snapshot[:sequences] || [],
                         snapshot[:tables] || [], @page_size, snapshot[:collations] || [],
                         start_page: start_page, views: snapshot[:views] || [])
    rescue NotImplementedError => error
      raise Error.new("0A000", error.message)
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new("54000", "cannot encode database: #{error.message}")
    end

    def discover_page_size
      PAGE_SIZES.each do |size|
        next if @file.size < size * 3

        image = FileImage.new(@file, size)
        next unless @codec.read_meta(image, size, 0) || @codec.read_meta(image, size, 1)

        return size
      end
      raise Error.new("XX001", "no valid meta page")
    end

    def decode_file
      image = @image = FileImage.new(@file, @page_size, cache_bytes: @cache_bytes)
      reader = FileFormat.new(collation_key: @codec.instance_variable_get(:@collation_key), expression: @codec.instance_variable_get(:@expression))
      snapshot = reader.decode_image(image, page_size: @page_size, lazy: true)
      a = @codec.read_meta(image, @page_size, 0)
      b = @codec.read_meta(image, @page_size, 1)
      snapshot[:meta][:slot] = if a && b && a[:txid] == b[:txid]
                                a[:txid] & 1
                              elsif a && (!b || a[:txid] >= b[:txid])
                                0
                              else
                                1
                              end
      snapshot
    end

    def publish_new(pages)
      temporary = "#{@path}.ruby-create-#{@pid}-#{object_id}"
      begin
        File.open(temporary, File::WRONLY | File::CREAT | File::EXCL, 0o600) do |file|
          pages.pages.keys.sort.each { |page| write_at(file, pages.pages.fetch(page), page * @page_size) }
          durable_sync(file) if @sync
        end
        # Linking publishes without clobbering a competing creator's path. The
        # temporary second name is removed while presence EX is still held.
        File.link(temporary, @path)
        File.unlink(temporary)
        sync_directory if @sync
      rescue Errno::EEXIST
        raise Error.new("58P02", "database file already exists")
      ensure
        File.unlink(temporary) if File.exist?(temporary)
      end
    end

    def write_at(file, bytes, offset)
      written = 0
      while written < bytes.bytesize
        count = file.pwrite(bytes.byteslice(written..), offset + written)
        raise IOError, "short database write" if count.zero?

        written += count
      end
    end

    def durable_sync(file)
      file.fsync
      # Ruby exposes fcntl directly; Darwin's full device-cache barrier is part
      # of the shared host contract and does not require a native extension.
      file.fcntl(51, 0) if RUBY_PLATFORM.include?("darwin") # F_FULLFSYNC
    end

    def sync_directory
      File.open(File.dirname(@path), File::RDONLY) { |directory| directory.fsync }
    rescue Errno::EINVAL, Errno::ENOTSUP
      # Directory synchronization is best effort on hosts that do not expose it.
      nil
    end

    def claim_path
      PATH_MUTEX.synchronize do
        raise Error.new("55006", "database is already open in this process") if OPEN_PATHS[@path]

        OPEN_PATHS[@path] = self
        @claimed = true
      end
    end

    def acquire_lock(timeout_ms)
      unless File.const_defined?(:NOFOLLOW) && File.method_defined?(:flock)
        raise Error.new("0A000", "host lacks crash-clean file locking")
      end
      @bundle = "#{@path}.lock"
      begin
        Dir.mkdir(@bundle, 0o700)
      rescue Errno::EEXIST
        raise Error.new("58030", "lock bundle must be a directory without a symlink") unless File.lstat(@bundle).directory?
      end
      flags = File::RDWR | File::CREAT | File::NOFOLLOW
      @arrival = File.open(File.join(@bundle, "arrival"), flags, 0o600)
      @presence = File.open(File.join(@bundle, "presence"), flags, 0o600)
      @arrival.close_on_exec = @presence.close_on_exec = true
      @arrival.flock(File::LOCK_SH)
      unknown = Dir.children(@bundle).grep(/\Aprotocol-v/).reject { |name| %w[protocol-v1 protocol-v2].include?(name) }
      raise Error.new("0A000", "unsupported lock-bundle protocol") unless unknown.empty?

      File.open(File.join(@bundle, "protocol-v2"), flags, 0o600) { |marker| marker.close_on_exec = true }
      deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + timeout_ms / 1000.0
      until (@creating || (!@service_options || @service_options[:ownership] != :never) && (!@service_pause_until || Process.clock_gettime(Process::CLOCK_MONOTONIC) >= @service_pause_until)) && @presence.flock(File::LOCK_EX | File::LOCK_NB)
        if @locking == :auto && !@creating && try_page_service_client
          @arrival.close
          @presence.close
          @arrival = @presence = nil
          return
        end
        raise Error.new("55006", "database is owned by another process") if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline

        sleep 0.005
      end
      @owns_presence = true
      @arrival.flock(File::LOCK_UN)
      @arrival.close
      @arrival = nil
      write_owner_record(Random.urandom(16), '')
    end

    def write_owner_record(incarnation, endpoint)
      record = "JEDOWNER".b + [1, endpoint.empty? ? 0 : 1, Process.euid].pack("nCN") + incarnation + [@pid, endpoint.bytesize].pack("N2") + endpoint.b
      record << [@codec.crc32(record)].pack("N")
      temporary = File.join(@bundle, "owner.tmp-#{incarnation.unpack1('H*')}")
      begin
        File.open(temporary, File::WRONLY | File::CREAT | File::EXCL | File::NOFOLLOW, 0o600) do |owner|
          owner.write(record)
        end
        File.rename(temporary, File.join(@bundle, "owner"))
      ensure
        File.unlink(temporary) if File.exist?(temporary)
      end
    end

    def ensure_pid
      raise Error.new("55006", "database handle was inherited across fork; reopen it") unless @pid == Process.pid
    end

    def ensure_open(write: false)
      ensure_pid
      raise Error.new("55000", "database is closed") if @closed
      raise Error.new("58030", "storage handle failed; reopen through recovery") if @poisoned
      raise Error.new("25006", "database is read-only") if write && @read_only
    end

    def release_resources
      @service_owner&.close
      @service_remote&.close
      @file&.close unless @file&.closed?
      if @owns_presence
        begin
          File.unlink(File.join(@bundle, "owner"))
        rescue Errno::ENOENT
          nil
        end
      end
      @arrival&.close unless @arrival&.closed?
      @presence&.close unless @presence&.closed?
      @owns_presence = false
      PATH_MUTEX.synchronize { OPEN_PATHS.delete(@path) } if @claimed
      @claimed = false
    end
  end
end

require_relative 'page_service_storage'
