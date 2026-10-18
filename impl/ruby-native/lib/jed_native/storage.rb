# frozen_string_literal: true

require_relative "error"
require_relative "file_format"

module JedNative
  # Experimental local-file host. It owns protocol-v2's crash-clean exclusive
  # lock and writes the shared v49 format. It does not serve page clients.
  # Commits currently serialize a complete snapshot to fresh pages; recovery
  # follows the validated-COW manifest contract, including adopted generations.
  class Storage
    OPEN_PATHS = {}
    PATH_MUTEX = Mutex.new
    PAGE_SIZES = (8..16).map { |exponent| 1 << exponent }.freeze

    attr_reader :path, :page_size, :meta

    def initialize(path:, create: false, page_size: 8192, locking: :exclusive,
                   file_lock_timeout_ms: 5000, fsync: true, read_only: false,
                   max_storage_bytes: 0, collation_key: nil, expression: nil)
      @codec = FileFormat.new(collation_key: collation_key, expression: expression)
      @pid = Process.pid
      @sync = fsync
      @read_only = read_only
      @max_storage_bytes = max_storage_bytes
      @mutex = Mutex.new
      @closed = false
      @poisoned = false
      @claimed = false
      @locking = locking.to_sym
      unless %i[exclusive none].include?(@locking)
        raise Error.new("0A000", "experimental Ruby file host requires locking: :exclusive or :none")
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
      @needs_stabilization = !create
      @needs_directory_sync = !create
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

    # Serialization completes before the first storage mutation. A size/codec
    # error leaves the handle usable; an I/O failure poisons it until reopen.
    def commit(snapshot)
      @mutex.synchronize do
        ensure_open(write: true)
        pages = build(snapshot, start_page: @meta.fetch(:page_count))
        root = pages.byteslice(20, 4).unpack1("N")
        live = pages.page_count - @meta.fetch(:page_count)
        if @max_storage_bytes.positive? && live * @page_size > @max_storage_bytes
          raise Error.new("54P06", "database storage limit exceeded")
        end
        txid = @meta.fetch(:txid) + 1
        raise Error.new("54000", "transaction id overflow") if txid > 0xFFFF_FFFF_FFFF_FFFF

        slot = (@meta[:slot] || (@meta[:txid] & 1)) ^ 1
        prepare_manifest(pages, txid, root, live, slot)
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
          durable_sync(@file) if @sync
        rescue SystemCallError, IOError => error
          @poisoned = true
          raise Error.new("58030", error.message)
        end
        @meta = { txid: txid, root_page: root, page_count: pages.page_count,
                  free_head: 0, live: live, slot: slot }
        @snapshot = snapshot.merge(meta: @meta)
        @meta
      end
    end

    def close
      return if @closed

      ensure_pid
      @closed = true
      poisoned = @poisoned
      release_resources
      raise Error.new("58030", "storage handle failed; reopen through recovery") if poisoned
    end

    private

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

    def prepare_manifest(pages, txid, root, live, slot)
      references = pages.pages.keys.sort.filter_map do |page|
        [page, @codec.crc64(pages.pages.fetch(page))] if page >= 2
      end
      inline_capacity = (@page_size - 64) / 12
      chain_capacity = (@page_size - 32) / 12
      remaining = [references.length - inline_capacity, 0].max
      chain_count = (remaining + chain_capacity - 1) / chain_capacity
      first_manifest = chain_count.zero? ? 0 : pages.page_count
      pages.resize(pages.page_count + chain_count)
      chain_digest = 0
      chain_count.times do |ordinal|
        page = first_manifest + ordinal
        entries = references.slice(inline_capacity + ordinal * chain_capacity, chain_capacity) || []
        following = ordinal + 1 < chain_count ? page + 1 : 0
        payload = [txid, ordinal, 0].pack("Q>NN") + entries.flatten.pack("NQ>" * entries.length)
        @codec.write_page(pages, @page_size, page, 8, entries.length, following, payload)
        chain_digest = @codec.crc64([page].pack("N") + pages.pages.fetch(page), chain_digest)
      end
      @codec.write_meta(pages, @page_size, slot, @page_size, txid, root, pages.page_count, live)
      offset = slot * @page_size
      pages[offset + 36, 12] = [first_manifest, references.length, chain_count].pack("N3")
      pages[offset + 48, 8] = [chain_digest].pack("Q>")
      inline = references.first(inline_capacity)
      pages[offset + 64, inline.length * 12] = inline.flatten.pack("NQ>" * inline.length)
      pages[offset + 32, 4] = [@codec.meta_crc(pages.pages.fetch(slot))].pack("N")
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
      image = FileImage.new(@file, @page_size)
      snapshot = @codec.decode_image(image, page_size: @page_size)
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
      until @presence.flock(File::LOCK_EX | File::LOCK_NB)
        raise Error.new("55006", "database is owned by another process") if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline

        sleep 0.005
      end
      @owns_presence = true
      @arrival.flock(File::LOCK_UN)
      @arrival.close
      @arrival = nil
      incarnation = Random.urandom(16)
      record = "JEDOWNER".b + [1, 0, Process.uid].pack("nCN") + incarnation + [@pid, 0].pack("N2")
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
