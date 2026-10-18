# frozen_string_literal: true

require_relative 'page_service_connection'

module JedNative
  class PageServiceOwner
    attr_reader :incarnation, :endpoint
    def initialize(storage, options)
      @storage, @options = storage, options
      @codec, @page_size = storage.instance_variable_get(:@codec), storage.page_size
      @incarnation = Random.urandom(16)
      @incarnation = Random.urandom(16) while @incarnation.bytes.all?(&:zero?)
      @mutex, @condition = Mutex.new, ConditionVariable.new
      @connections, @next_connection, @window, @resolution = {}, 1, [], {}
      @local_pins = {}
      @window_start = storage.meta[:txid]
      @meta_bytes = storage.instance_variable_get(:@image).byteslice(storage.meta[:slot] * @page_size, @page_size)
      @meta = storage.meta
      @resolution[@meta[:txid]] = @codec.crc64(@meta_bytes)
      image = storage.instance_variable_get(:@image)
      previous_slot = @meta[:slot] ^ 1
      previous = @codec.read_meta(image, @page_size, previous_slot)
      @resolution[previous[:txid]] = @codec.crc64(image.byteslice(previous_slot * @page_size, @page_size)) if previous && previous[:txid] + 1 == @meta[:txid]
      @endpoint = endpoint_path
    end

    def private_directory?(path)
      status = File.lstat(path)
      status.directory? && status.uid == Process.euid && status.mode & 0o7777 == 0o700
    rescue SystemCallError
      false
    end
    def ensure_private_directory(path)
      Dir.mkdir(path, 0o700) unless File.exist?(path)
      private_directory?(path)
    rescue SystemCallError
      false
    end
    def endpoint_path
      directory = "#{@storage.path}.lock/uid-#{Process.euid}"
      path = File.join(directory, 'endpoint')
      return path if path.bytesize <= 100 && ensure_private_directory(directory)
      candidates = []
      candidates << ENV['XDG_RUNTIME_DIR'] if RUBY_PLATFORM.include?('linux')
      candidates << ENV['TMPDIR'] if RUBY_PLATFORM.include?('darwin')
      fallback = "/tmp/jed-#{Process.euid}"
      candidates << fallback if ensure_private_directory(fallback)
      candidates.compact.each do |directory|
        path = File.join(directory, "jed-#{@incarnation.byteslice(0, 8).unpack1('H*')}.sock")
        return path if path.bytesize <= 100 && private_directory?(directory)
      end
      nil
    end

    def start
      @storage.send(:write_owner_record, @incarnation, @endpoint || '')
      return unless @endpoint
      File.unlink(@endpoint) if File.exist?(@endpoint)
      @listener = UNIXServer.new(@endpoint)
      @listener.close_on_exec = true
      @acceptor = Thread.new do
        loop do
          socket = @listener.accept
          socket.close_on_exec = true
          connection = @mutex.synchronize do
            id = @next_connection
            @next_connection += 1
            @connections[id] = OwnerConnection.new(self, socket, id)
          end
          connection.start
        end
      rescue IOError, SystemCallError
        nil
      end
      @acceptor.report_on_exception = false
      @timer = Thread.new do
        loop do
          sleep 0.05
          break if @closed
          @mutex.synchronize { @connections.values.dup }.each(&:expire)
        end
      end
      @timer.report_on_exception = false
    end

    def now = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    def oldest_pin
      @mutex.synchronize { (@connections.values.flat_map { |connection| connection.pins.keys } + @local_pins.values).min || @meta[:txid] + 1 }
    end
    def pin_local(token)
      await_admission
      @mutex.synchronize { @local_pins[token] ||= @meta[:txid] }
    end
    def copy_local_pin(source, token)
      @mutex.synchronize { @local_pins[token] ||= @local_pins[source] || @meta[:txid] }
    end
    def unpin_local(token) = @mutex.synchronize { @local_pins.delete(token); @condition.broadcast }
    def draining? = @mutex.synchronize { !!@draining }
    def await_admission
      @mutex.synchronize do
        @condition.wait(@mutex) while @draining && !@closed
        raise Error.new('08006', 'page-service owner left') if @closed
      end
    end
    def acquire(token, timeout_ms)
      await_admission
      deadline = now + timeout_ms / 1000.0 if timeout_ms.positive?
      @mutex.synchronize do
        while @writer && !@writer.equal?(token)
          remaining = deadline && deadline - now
          raise Error.new('55P03', 'writer lock timeout') if remaining && remaining <= 0
          raise Error.new('08006', 'page-service owner is closing') if @closed
          @condition.wait(@mutex, remaining)
        end
        @writer = token
      end
    end
    def release(token)
      @mutex.synchronize do
        if @writer.equal?(token)
          @writer = nil
          @condition.broadcast
        end
      end
    end
    def publish(meta, bytes, indices)
      @mutex.synchronize do
        @meta, @meta_bytes = meta, bytes
        @window << [meta[:txid], indices.sort.uniq]
        @window_start = @window.shift[0] while @window.length > 256
        @resolution[meta[:txid]] = @codec.crc64(bytes)
        @resolution.shift while @resolution.length > 256
      end
    end
    def snapshot(connection, request)
      await_admission
      @mutex.synchronize do
        txid = @meta[:txid]
        previous = request[:last_txid]
        all = request[:incarnation] != @incarnation || previous.zero? || previous > txid || previous < @window_start
        indices = all ? [] : @window.select { |version, _| version > previous }.flat_map(&:last).sort.uniq
        all, indices = true, [] if indices.length > 65_536
        pin = connection.pins[txid] ||= { count: 0, page_count: @meta[:page_count] }
        pin[:count] += 1
        { incarnation: @incarnation, txid: txid, invalidate: all ? 1 : 0, indices: indices, meta: @meta_bytes }
      end
    end
    def unpin(connection, txid)
      @mutex.synchronize do
        pin = connection.pins[txid] || raise(Error.new('08P01', 'snapshot is not pinned'))
        pin[:count] -= 1
        connection.pins.delete(txid) if pin[:count].zero?
        @condition.broadcast
      end
    end
    def read_pages(connection, txid, indices)
      count = @mutex.synchronize { connection.pins[txid]&.fetch(:page_count) }
      raise Error.new('08P01', 'invalid pinned page read') unless count && indices.length.between?(1, max_read_pages) && indices.all? { |page| page.between?(2, count - 1) }
      file = @storage.instance_variable_get(:@file)
      indices.map do |page|
        bytes = file.pread(@page_size, page * @page_size)
        raise Error.new('58030', 'short database read') unless bytes.bytesize == @page_size
        bytes
      end
    end
    def max_read_pages = [1024, (PageServiceWire::MAX_FRAME - 9) / @page_size].min
    def remove(connection)
      cancel = @mutex.synchronize do
        @connections.delete(connection.id)
        connection.pins.clear
        cancellation = @yield_target == connection.id && !@releasing
        @draining = @yield_target = nil if cancellation
        @condition.broadcast
        cancellation
      end
      broadcast_notice(2, 0) if cancel
      release(connection)
    end
    def broadcast_notice(kind, connection)
      @mutex.synchronize { @connections.values.dup }.each do |peer|
        peer.send_frame(:notice, 0, kind: kind, connection_id: connection)
      rescue IOError, SystemCallError
        peer.close
      end
    end
    def request_ownership(connection)
      accepted = @mutex.synchronize do
        next true if @yield_target == connection.id
        next false if @draining || @closed || !(@options[:ownership] == :yield || @storage.read_only?)
        @draining, @yield_target = true, connection.id
        true
      end
      return false unless accepted
      unless @yield_thread&.alive?
        @yield_thread = Thread.new do
          broadcast_notice(1, connection.id)
          if drain(@options[:yield_grace_ms], cancellation: connection.id)
            @mutex.synchronize { @releasing = true }
            @storage.send(:yield_service_ownership, self)
          end
        end
        @yield_thread.report_on_exception = false
      end
      true
    end
    def drain(milliseconds, cancellation: nil)
      deadline = now + milliseconds / 1000.0 if milliseconds.positive?
      @mutex.synchronize do
        @draining = true
        loop do
          return false if cancellation && @yield_target != cancellation
          active = @writer || !@local_pins.empty? || @connections.values.any? { |connection| !connection.pins.empty? || connection.committing? }
          return true unless active
          remaining = deadline && deadline - now
          return true if remaining && remaining <= 0 && @connections.values.none?(&:committing?)
          @condition.wait(@mutex, remaining && [remaining, 0.05].max)
        end
      end
    end
    def shutdown
      return if @closed
      broadcast_notice(3, 0)
      drain(@options[:shutdown_grace_ms])
    end
    def close
      return if @closed
      @closed = true
      @listener&.close
      connections = @mutex.synchronize { @condition.broadcast; @connections.values.dup }
      connections.each(&:close)
      @acceptor&.join unless Thread.current.equal?(@acceptor)
      @timer&.join unless Thread.current.equal?(@timer)
      File.unlink(@endpoint) if @endpoint && File.exist?(@endpoint)
    end

    class OwnerConnection
      attr_reader :id, :pins
      def initialize(owner, socket, id)
        @owner, @socket, @id = owner, socket, id
        @storage = owner.instance_variable_get(:@storage)
        @codec, @page_size = @storage.instance_variable_get(:@codec), @storage.page_size
        @options = owner.instance_variable_get(:@options)
        @send_mutex, @pins = Mutex.new, {}
        @last_active = owner.now
      end
      def start
        @thread = Thread.new { run }
        @thread.report_on_exception = false
      end
      def send_frame(name, request, values = {})
        @send_mutex.synchronize { @socket.write(PageServiceWire.encode(name, request, values, @page_size)) }
      end
      def fail_request(request, error, fatal = false)
        send_frame(:error, request, sqlstate: error.sqlstate, message: error.message.sub(/\A[A-Z0-9]{5}: /, ''), detail: '', flags: fatal ? 1 : 0)
        close if fatal
      rescue IOError, SystemCallError
        close
      end
      def run
        request = 0
        loop do
          name, request, fields = PageServiceWire.read(@socket, @page_size)
          @last_active = @owner.now
          if !@hello
            raise Error.new('08P01', 'first frame must be hello') unless name == :hello
            handshake(request, fields)
            next
          end
          raise Error.new('08P01', 'duplicate hello') if name == :hello
          if @delta && !%i[commit_pages commit_end].include?(name)
            raise Error.new('08P01', 'interleaved commit messages')
          end
          dispatch(name, request, fields)
        rescue Error => error
          fatal = %w[08P01 08006].include?(error.sqlstate)
          fail_request(request, error, fatal)
          break if fatal
        end
      rescue IOError, SystemCallError
        nil
      ensure
        close
      end
      def handshake(request, fields)
        unless fields[:protocol_min] <= 1 && fields[:protocol_max] >= 1 && fields[:format_version] == FileFormat::VERSION
          return fail_request(request, Error.new('0A000', 'owner supports protocol 1 and its current file format'), true)
        end
        maximum = @options[:max_connections]
        count = @owner.instance_variable_get(:@mutex).synchronize { @owner.instance_variable_get(:@connections).length }
        return fail_request(request, Error.new('53300', 'page-service connection limit exceeded'), true) if maximum.positive? && count > maximum
        @read_only = fields[:flags] & 1 == 1
        @hello = true
        flags = (@read_only ? 1 : 0) | (@storage.read_only? ? 2 : 0) | (@options[:ownership] == :yield ? 4 : 0)
        send_frame(:hello_ok, request, protocol: 1, format_version: FileFormat::VERSION, page_size: @page_size, connection_id: @id, flags: flags, max_read_pages: @owner.max_read_pages)
      end
      def dispatch(name, request, fields)
        case name
        when :begin_read
          if @owner.draining?
            Thread.new do
              send_frame(:read_begun, request, @owner.snapshot(self, fields))
            rescue Error, IOError, SystemCallError
              close
            end
          else
            send_frame(:read_begun, request, @owner.snapshot(self, fields))
          end
        when :end_read then @owner.unpin(self, fields[:txid])
        when :read_pages
          raise Error.new('25P03', 'idle write transaction was revoked') if @revoked && fields[:txid] == @revoked
          send_frame(:pages, request, pages: @owner.read_pages(self, fields[:txid], fields[:indices]))
        when :begin_write then begin_write(request, fields)
        when :abort then abort_write
        when :commit_begin then begin_commit(fields)
        when :commit_pages then receive_pages(fields[:pages])
        when :commit_end then end_commit(request)
        when :commit_status
          meta = @storage.meta
          digest = @owner.instance_variable_get(:@mutex).synchronize { @owner.instance_variable_get(:@resolution)[fields[:txid]] }
          status = digest ? (digest == fields[:digest] ? 0 : 1) : (fields[:txid] > meta[:txid] ? 1 : 2)
          send_frame(:commit_status_reply, request, status: status)
        when :request_ownership then send_frame(:ownership_reply, request, status: @owner.request_ownership(self) ? 0 : 1)
        else raise Error.new('08P01', 'unsupported page-service operation')
        end
      end
      def begin_write(request, fields)
        raise Error.new('25006', 'read-only page-service connection') if @read_only || @storage.read_only?
        raise Error.new('08P01', 'writer already requested') if @writing || @waiting
        if @revoked
          @revoked = nil
          raise Error.new('25P03', 'idle write transaction was revoked')
        end
        @waiting = true
        @waiter = Thread.new do
          begin
            @owner.acquire(self, fields[:lock_timeout_ms])
            raise Error.new('08006', 'connection closed') if @closed
            @storage.flush(shrink: false)
            @writing, @waiting = true, false
            reply = @owner.snapshot(self, fields)
            @write_txid = reply[:txid]
            @base = @storage.meta.dup
            @grant = @storage.send(:effective_reader_watermark) >= @storage.instance_variable_get(:@free_generation)
            @granted_pages = @grant ? @storage.instance_variable_get(:@free_pages).to_set : Set.new
            send_frame(:write_begun, request, reply.merge(reuse_grant: @grant))
          rescue Error => error
            @waiting = false
            @owner.release(self)
            fail_request(request, error, error.sqlstate == '08006')
          rescue IOError, SystemCallError
            close
          end
        end
        @waiter.report_on_exception = false
      end
      def legal_page?(page, high)
        page >= 2 && page < high && (page >= @base[:page_count] || @granted_pages.include?(page))
      end
      def begin_commit(fields)
        return if @revoked
        raise Error.new('08P01', 'commit without writer gate') unless @writing
        @delta = { error: nil, received: Set.new }
        meta_bytes, txid = fields.values_at(:meta, :txid)
        high = meta_bytes.byteslice(24, 4).unpack1('N')
        raise Error.new('08P01', 'invalid commit generation') unless txid == @base[:txid] + 1 && high >= @base[:page_count]
        image = PageBuffer.new(@page_size, high)
        image[0, @page_size] = meta_bytes
        manifest = fields[:manifest_pages]
        raise Error.new('08P01', 'duplicate manifest page') unless manifest.map(&:first).uniq.length == manifest.length
        manifest.each do |page, bytes|
          raise Error.new('08P01', 'manifest page is not reusable') unless legal_page?(page, high)
          image[page * @page_size, @page_size] = bytes
        end
        meta = @codec.read_meta(image, @page_size, 0, validate_body: false)
        raise Error.new('08P01', 'invalid commit manifest') unless meta && meta[:txid] == txid && !meta[:manifest].empty? && meta[:manifest_pages] == manifest.map(&:first)
        raise Error.new('08P01', 'body page is not reusable') unless meta[:manifest].all? { |page, _| legal_page?(page, high) }
        @delta.merge!(meta: meta, bytes: meta_bytes, expected: meta[:manifest].to_h, manifest: manifest.map(&:first))
        manifest.each { |page, bytes| @storage.send(:write_at, @storage.instance_variable_get(:@file), bytes, page * @page_size) }
      rescue Error, ArgumentError, RuntimeError => error
        @delta[:error] = Error.new('08P01', error.message)
      rescue IOError, SystemCallError => error
        poison(error)
      end
      def receive_pages(pages)
        return if @revoked || @delta && @delta[:error]
        raise Error.new('08P01', 'body without commit_begin') unless @delta
        pages.each do |page, bytes|
          expected = @delta[:expected][page]
          unless expected && @delta[:received].add?(page) && bytes.getbyte(0).between?(1, 7) && @codec.page_crc(bytes) == bytes.byteslice(12, 4).unpack1('N') && @codec.crc64(bytes) == expected
            raise Error.new('08P01', 'commit page identity mismatch')
          end
          @storage.send(:write_at, @storage.instance_variable_get(:@file), bytes, page * @page_size)
        end
      rescue Error => error
        @delta[:error] = error
      rescue IOError, SystemCallError => error
        poison(error)
      end
      def end_commit(request)
        if @revoked
          @revoked = nil
          raise Error.new('25P03', 'idle write transaction was revoked')
        end
        raise Error.new('08P01', 'commit_end without commit_begin') unless @delta
        error = @delta[:error]
        error ||= Error.new('08P01', 'commit pages are missing') unless @delta[:received].length == @delta[:expected]&.length
        if error
          abort_write
          return fail_request(request, error, true)
        end
        @committing = true
        @storage.send(:accept_service_commit, @delta)
        published = true
        if @storage.instance_variable_get(:@service_crash_ack)
          exit! 197
        end
        if @storage.instance_variable_get(:@service_drop_ack)
          @storage.instance_variable_set(:@service_drop_ack, false)
          close
          return
        end
        send_frame(:committed, request, txid: @delta[:meta][:txid])
        @storage.send(:service_maintenance)
      rescue IOError, SystemCallError => error
        poison(error)
      ensure
        abort_write if published
        @committing = false
        @owner.remove(self) if @closed
      end
      def abort_write
        @owner.unpin(self, @write_txid) if @write_txid && @pins[@write_txid]
        @write_txid, @writing, @delta, @revoked = nil, false, nil, nil
        @owner.release(self)
      end
      def expire
        return if @closed || @committing || @waiting
        elapsed = (@owner.now - @last_active) * 1000
        if @writing && @options[:idle_in_transaction_timeout_ms].positive? && elapsed >= @options[:idle_in_transaction_timeout_ms]
          txid = @write_txid
          abort_write
          @revoked = txid
        elsif @pins.empty? && @options[:idle_session_timeout_ms].positive? && elapsed >= @options[:idle_session_timeout_ms]
          close
        end
      end
      def committing? = !!@committing
      def poison(error)
        @storage.instance_variable_set(:@poisoned, true)
        close
        raise Error.new('58030', error.message)
      end
      def close
        return if @closed
        @closed = true
        @socket.close unless @socket.closed?
        @owner.remove(self) unless @committing
      rescue IOError, SystemCallError
        @owner.remove(self) unless @committing
      end
    end
  end
end
