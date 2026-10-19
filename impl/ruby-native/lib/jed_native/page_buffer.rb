# frozen_string_literal: true

module JedNative
  # All generations on a byte device share one decoded-leaf budget. Entries
  # contain only raw PAX regions and keys; typed/decompressed values belong to
  # the caller's row or operator, never this cache.
  class DecodedLeafCache
    def initialize(limit)
      @limit, @entries, @generation = [limit, 1].max, {}, 0
      @mutex = Mutex.new
    end

    def fetch(key)
      cached, generation = @mutex.synchronize do
        value = @entries.delete(key)
        @entries[key] = value if value
        [value, @generation]
      end
      return cached if cached
      value = yield
      @mutex.synchronize do
        # A concurrent publication may invalidate a reused page while it is
        # being decoded. Its old bytes remain valid for the pinned caller, but
        # must not be inserted into the new generation's shared cache.
        if generation == @generation
          @entries.delete(key)
          @entries.shift while @entries.length >= @limit
          @entries[key] = value
        end
      end
      value
    end

    def invalidate(indices = nil)
      @mutex.synchronize do
        @generation += 1
        if indices
          selected = indices.to_set
          @entries.delete_if { |(page, _), _| selected.include?(page) }
        else
          @entries.clear
        end
      end
    end

    def size = @mutex.synchronize { @entries.length }
  end

  # Pending pages retain their written prefix. Zero padding is reconstructed at
  # the byte-device boundary; a sparse GiST node need not retain an entire page.
  # This is an in-memory representation only: checksums and writes still see the
  # canonical full page, including every zero byte.
  class PendingPages
    include Enumerable

    def initialize(page_size)
      @page_size, @entries = page_size, {}
    end

    def [](page)
      bytes = @entries[page]
      bytes && (bytes.bytesize == @page_size ? bytes : bytes.ljust(@page_size, "\0".b))
    end

    def fetch(page)
      self[page] || raise(KeyError, "page not present: #{page}")
    end

    def []=(page, bytes)
      raise ArgumentError, 'page length mismatch' unless bytes.bytesize == @page_size
      @entries[page] = bytes.sub(/\x00+\z/n, ''.b)
    end

    def write(page, offset, bytes)
      target = (@entries[page] ||= +''.b)
      target << "\0".b * (offset - target.bytesize) if offset > target.bytesize
      target[offset, bytes.bytesize] = bytes
    end

    def slice(page, offset, length)
      bytes = @entries[page]
      value = bytes && offset < bytes.bytesize ? bytes.byteslice(offset, length) : +''.b
      value.bytesize == length ? value : value.ljust(length, "\0".b)
    end

    def keys = @entries.keys
    def length = @entries.length
    alias size length
    def empty? = @entries.empty?
    def key?(page) = @entries.key?(page)
    def retained_bytes = @entries.values.sum(&:bytesize)

    def each
      return enum_for(:each) unless block_given?
      @entries.each_key { |page| yield page, self[page] }
      self
    end

    def merge!(other)
      other.each { |page, bytes| self[page] = bytes }
      self
    end

    def select
      return enum_for(:select) unless block_given?
      each_with_object({}) { |(page, bytes), result| result[page] = bytes if yield(page, bytes) }
    end

    def reject
      return enum_for(:reject) unless block_given?
      each_with_object({}) { |(page, bytes), result| result[page] = bytes unless yield(page, bytes) }
    end
  end

  # The codec addresses logical pages. A commit buffers only its newly encoded
  # pages, so an append commit does not allocate the preceding file's length.
  class PageBuffer
    attr_reader :page_size, :page_count, :pages

    def initialize(page_size, page_count, compact: false)
      raise ArgumentError, "invalid page count" unless page_count.between?(3, 0xFFFF_FFFF)

      @page_size = page_size
      @page_count = page_count
      @pages = compact ? PendingPages.new(page_size) : {}
    end

    def bytesize = @page_size * @page_count

    def resize(page_count)
      raise ArgumentError, "invalid page count" unless page_count.between?(3, 0xFFFF_FFFF) && @pages.keys.none? { |page| page >= page_count }

      @page_count = page_count
    end

    def []=(offset, length, value)
      raise ArgumentError, "page write length mismatch" unless length == value.bytesize
      raise ArgumentError, "page write outside image" unless offset >= 0 && offset + length <= bytesize

      page, within = offset.divmod(@page_size)
      raise ArgumentError, "page write crosses boundary" if within + length > @page_size

      if @pages.is_a?(PendingPages)
        @pages.write(page, within, value)
      else
        bytes = (@pages[page] ||= "\0".b * @page_size)
        bytes[within, length] = value
      end
    end

    def byteslice(offset, length)
      return nil if offset.negative? || offset > bytesize

      length = [length, bytesize - offset].min
      out = +"".b
      while length.positive?
        page, within = offset.divmod(@page_size)
        count = [length, @page_size - within].min
        out << if @pages.is_a?(PendingPages)
          @pages.slice(page, within, count)
        else
          @pages[page] ? @pages[page].byteslice(within, count) : "\0".b * count
        end
        offset += count
        length -= count
      end
      out
    end

    def to_s = byteslice(0, bytesize)
  end

  # A bounded page reader implementing the codec's image seam. Recovery can
  # inspect manifests and catalogs without copying the entire file into a String.
  class FileImage
    DEFAULT_CACHE_BYTES = 256 * 1024 * 1024
    attr_reader :bytesize, :cache_pages, :decoded_leaf_cache

    def initialize(file, page_size, cache_bytes: DEFAULT_CACHE_BYTES)
      @file = file
      @page_size = page_size
      @bytesize = file.size
      @cache_pages = [1, cache_bytes / page_size].max
      @cache = {}
      @decoded_leaf_cache = DecodedLeafCache.new(@cache_pages)
    end

    def invalidate(pages = nil)
      pages ? pages.each { |page| @cache.delete(page) } : @cache.clear
      @decoded_leaf_cache.invalidate(pages)
      @bytesize = @file.size
    end

    def cached_pages = @cache.length

    def byteslice(offset, length)
      return nil if offset.negative? || offset > @bytesize

      length = [length, @bytesize - offset].min
      out = +"".b
      while length.positive?
        page, within = offset.divmod(@page_size)
        bytes = @cache.delete(page)
        unless bytes
          bytes = @file.pread(@page_size, page * @page_size)
          @cache.shift if @cache.size >= @cache_pages
        end
        @cache[page] = bytes
        count = [length, @page_size - within].min
        part = bytes.byteslice(within, count)
        raise IOError, "short database page read" unless part && part.bytesize == count

        out << part
        offset += count
        length -= count
      end
      out
    end
  end

  # Commit preparation reads new pages from its private body and clean pages
  # from the existing pager. Publication switches the same views to the file.
  class PageOverlay
    attr_reader :cache_pages
    def initialize(image, pages)
      @image, @pages = image, pages
      @page_size, @cache_pages = pages.page_size, image.cache_pages
      @private_leaf_cache = DecodedLeafCache.new(@cache_pages)
    end
    def bytesize = @pages ? @pages.bytesize : @image.bytesize
    def cached_pages = @image.cached_pages
    def decoded_leaf_cache = @pages ? @private_leaf_cache : @image.decoded_leaf_cache
    def publish
      @private_leaf_cache.invalidate
      @pages = nil
    end
    def byteslice(offset, length)
      return @image.byteslice(offset, length) unless @pages
      return nil if offset.negative? || offset > bytesize
      length = [length, bytesize - offset].min
      out = +''.b
      while length.positive?
        page, within = offset.divmod(@page_size)
        count = [length, @page_size - within].min
        bytes = @pages.pages[page]
        out << (bytes ? bytes.byteslice(within, count) : @image.byteslice(offset, count))
        offset += count
        length -= count
      end
      out
    end
  end
end
