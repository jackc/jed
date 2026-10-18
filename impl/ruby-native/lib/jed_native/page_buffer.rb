# frozen_string_literal: true

module JedNative
  # The codec addresses logical pages. A commit buffers only its newly encoded
  # pages, so an append commit does not allocate the preceding file's length.
  class PageBuffer
    attr_reader :page_size, :page_count, :pages

    def initialize(page_size, page_count)
      raise ArgumentError, "invalid page count" unless page_count.between?(3, 0xFFFF_FFFF)

      @page_size = page_size
      @page_count = page_count
      @pages = {}
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

      bytes = (@pages[page] ||= "\0".b * @page_size)
      bytes[within, length] = value
    end

    def byteslice(offset, length)
      return nil if offset.negative? || offset > bytesize

      length = [length, bytesize - offset].min
      out = +"".b
      while length.positive?
        page, within = offset.divmod(@page_size)
        count = [length, @page_size - within].min
        out << (@pages[page] ? @pages[page].byteslice(within, count) : "\0".b * count)
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
    attr_reader :bytesize, :cache_pages

    def initialize(file, page_size, cache_bytes: DEFAULT_CACHE_BYTES)
      @file = file
      @page_size = page_size
      @bytesize = file.size
      @cache_pages = [1, cache_bytes / page_size].max
      @cache = {}
    end

    def invalidate(pages = nil)
      pages ? pages.each { |page| @cache.delete(page) } : @cache.clear
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
    end
    def bytesize = @pages ? @pages.bytesize : @image.bytesize
    def cached_pages = @image.cached_pages
    def publish = (@pages = nil)
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
