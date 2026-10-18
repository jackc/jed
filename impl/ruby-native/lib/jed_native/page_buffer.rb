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
      raise ArgumentError, "invalid page count" unless page_count.between?(@page_count, 0xFFFF_FFFF)

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
    attr_reader :bytesize

    def initialize(file, page_size)
      @file = file
      @page_size = page_size
      @bytesize = file.size
      @cache = {}
    end

    def byteslice(offset, length)
      return nil if offset.negative? || offset > @bytesize

      length = [length, @bytesize - offset].min
      out = +"".b
      while length.positive?
        page, within = offset.divmod(@page_size)
        bytes = @cache[page]
        unless bytes
          bytes = @file.pread(@page_size, page * @page_size)
          @cache.shift if @cache.size >= 256
          @cache[page] = bytes
        end
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
end
