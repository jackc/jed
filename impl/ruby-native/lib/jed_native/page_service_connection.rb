# frozen_string_literal: true

require 'socket'
require_relative 'page_service_wire'

module JedNative
  # Replies are demultiplexed independently of callers, so a blocked writer's
  # request never holds up a pinned reader fetching pages on the same socket.
  class PageServiceConnection
    attr_accessor :page_size, :notice_handler, :failure_handler
    attr_reader :failure
    def initialize(socket)
      @socket, @page_size, @next_id = socket, 0, 1
      @send_mutex, @state_mutex = Mutex.new, Mutex.new
      @pending = {}
      @reader = Thread.new do
        begin
          loop do
            name, request, fields = PageServiceWire.read(@socket, @page_size)
            @page_size = fields[:page_size] if name == :hello_ok
            if name == :notice
              @notice_handler&.call(fields)
              next
            end
            queue = @state_mutex.synchronize { @pending.delete(request) }
            raise Error.new('08P01', 'unexpected page-service reply') unless queue
            if name == :error
              error = Error.new(fields[:sqlstate], fields[:message] + (fields[:detail].empty? ? '' : ": #{fields[:detail]}"))
              fatal = fields[:flags] & 1 == 1
              fail_connection(Error.new('08006', 'page-service connection closed by owner')) if fatal
              queue << error
              break if fatal
            else
              queue << [name, fields]
            end
          end
        rescue Error, IOError, SystemCallError => error
          fail_connection(error.is_a?(Error) ? error : Error.new('08006', error.message))
        end
      end
      @reader.report_on_exception = false
    end

    def request(name, fields = {}, reply: true, **keywords)
      fields = fields.merge(keywords) unless keywords.empty?
      queue = reply && Queue.new
      @send_mutex.synchronize do
        request = @state_mutex.synchronize do
          raise @failure if @failure
          current = @next_id
          @next_id = @next_id == 0xFFFF_FFFF ? 1 : @next_id + 1
          @pending[current] = queue if queue
          current
        end
        @socket.write(PageServiceWire.encode(name, request, fields, @page_size))
      end
      return unless queue
      response = queue.pop
      raise response if response.is_a?(Exception)
      response
    rescue IOError, SystemCallError => error
      failure = Error.new('08006', error.message)
      fail_connection(failure)
      raise failure
    end

    def fail_connection(error)
      first = false
      queues = @state_mutex.synchronize do
        first = @failure.nil?
        @failure ||= error
        pending = @pending.values
        @pending = {}
        pending
      end
      @failure_handler&.call if first
      queues.each { |queue| queue << @failure }
      @socket.close unless @socket.closed?
    rescue IOError, SystemCallError
      nil
    end

    def close
      fail_connection(Error.new('08006', 'page-service connection closed'))
      @reader.join unless Thread.current.equal?(@reader)
    end

    def discard_inherited
      @failure = Error.new('55006', 'page-service connection was inherited across fork')
      @socket.close unless @socket.closed?
      @pending.clear
    end
  end

  class RemotePageImage
    attr_reader :bytesize, :cache_pages, :decoded_leaf_cache
    def initialize(client, page_size, cache_bytes)
      @client, @page_size = client, page_size
      @cache_pages, @cache = [1, cache_bytes / page_size].max, {}
      @mutex = Mutex.new
      @decoded_leaf_cache = DecodedLeafCache.new(@cache_pages)
    end
    def adopt(meta, page_count, invalidation)
      @decoded_leaf_cache.invalidate(invalidation)
      @mutex.synchronize do
        invalidation ? invalidation.each { |page| @cache.delete(page) } : @cache.clear
        @meta, @bytesize = meta, page_count * @page_size
      end
    end
    def cached_pages = @mutex.synchronize { @cache.length }
    def byteslice(offset, length)
      return nil if offset.negative? || offset > @bytesize
      length = [length, @bytesize - offset].min
      out = +''.b
      while length.positive?
        page, within = offset.divmod(@page_size)
        count = [length, @page_size - within].min
        bytes = if page < 2
                  @meta
                else
                  @mutex.synchronize do
                    value = @cache.delete(page)
                    unless value
                      value = @client.read_pages([page]).first
                      @cache.shift if @cache.length >= @cache_pages
                    end
                    @cache[page] = value
                  end
                end
        out << bytes.byteslice(within, count)
        offset += count
        length -= count
      end
      out
    end
  end
end
