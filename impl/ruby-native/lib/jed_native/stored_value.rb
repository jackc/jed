# frozen_string_literal: true

module JedNative
  # Storage facts survive demand faults independently of the evaluator's Ruby
  # value. An incremental rewrite can retain these bytes without reading or
  # recompressing an unchanged value (large-values.md §15).
  class StoredValue
    attr_reader :pool, :type, :stored_bytes, :form, :first_page,
                :payload_length, :compressed_length, :stored_length,
                :compression_outcome

    class CompressedBlock
      def initialize(value)
        @value = value
        freeze
      end

      def bytesize = @value.compressed_length
      def to_s = @value.compressed_bytes
    end

    def self.from_leaf(leaf, slot, column)
      region = leaf.data[:regions].fetch(column)
      return nil if region[:width]

      low = slot.zero? ? 0 : region[:ends][slot - 1]
      length = region[:ends][slot] - low
      return nil if length.zero?

      new(pool: leaf.pool, type: leaf.columns[column][:type],
          bytes: region[:body].byteslice(low, length))
    end

    def initialize(pool:, type:, bytes:)
      @pool, @type, @stored_bytes = pool, type, bytes.b.freeze
      @first_page = 0
      @compressed_length = nil
      header = %w[text bytea json].include?(type) ? 3 : 1
      case @stored_bytes.getbyte(0)
      when 0
        raise Error.new('XX001', 'short inline value span') if @stored_bytes.bytesize < header
        @form = :inline
        @payload_length = @stored_bytes.bytesize - header
        @stored_length = @payload_length
        @compression_outcome = :unknown
      when FileFormat::TAG_EXTERNAL
        check_span_length(FileFormat::EXTERNAL_PTR_LEN)
        @form = :external
        @first_page, @payload_length = @stored_bytes.byteslice(1, 8).unpack('N2')
        @stored_length = @payload_length
        @compression_outcome = :rejected
      when FileFormat::TAG_INLINE_COMP
        raise Error.new('XX001', 'short compressed value span') if @stored_bytes.bytesize < FileFormat::INLINE_COMP_OVERHEAD
        @form = :inline_comp
        @payload_length, @compressed_length = @stored_bytes.byteslice(1, 6).unpack('Nn')
        check_span_length(FileFormat::INLINE_COMP_OVERHEAD + @compressed_length)
        @stored_length = @compressed_length
        @compression_outcome = :accepted
      when FileFormat::TAG_EXTERNAL_COMP
        check_span_length(FileFormat::EXTERNAL_COMP_PTR_LEN)
        @form = :external_comp
        @first_page, @compressed_length, @payload_length = @stored_bytes.byteslice(1, 12).unpack('N3')
        @stored_length = @compressed_length
        @compression_outcome = :accepted
      else
        raise Error.new('XX001', 'invalid stored value form')
      end
      if external? && (@first_page < FileFormat::ROOT_PAGE || @stored_length.zero?)
        raise Error.new('XX001', 'invalid stored overflow pointer')
      end
      @plain_size = @payload_length + header
      freeze
    end

    def plain_size = @plain_size
    def external? = @form == :external || @form == :external_comp
    def large? = @form != :inline
    def compressed? = @compression_outcome == :accepted
    def overflow_count(capacity = @pool.page_size - FileFormat::PAGE_HEADER)
      external? ? (@stored_length + capacity - 1) / capacity : 0
    end

    def resolve
      value, finish = @pool.codec.decode_value(@type, @stored_bytes, 0, ->(page) { @pool.page(page) })
      raise Error.new('XX001', 'stored value has trailing bytes') unless finish == @stored_bytes.bytesize
      if %w[text json].include?(@type) && !value.valid_encoding?
        raise Error.new('XX001', 'stored text is not valid UTF-8')
      end
      value
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new('XX001', error.message)
    end

    # Only inline-plain values have an unknown compression outcome. Their
    # payload is already in the leaf; planning needs neither decoding nor I/O.
    def planning_payload
      raise Error.new('XX001', 'large stored value has a known compression outcome') unless @form == :inline
      @stored_bytes.byteslice(@stored_bytes.bytesize - @payload_length, @payload_length)
    end

    def compressed_block
      return nil unless compressed?
      @form == :inline_comp ? compressed_bytes : CompressedBlock.new(self)
    end

    def compressed_bytes
      return @stored_bytes.byteslice(FileFormat::INLINE_COMP_OVERHEAD, @compressed_length) if @form == :inline_comp
      raise Error.new('XX001', 'stored value is not compressed') unless @form == :external_comp
      stored_payload
    end

    # Used by a from-scratch image writer: copy the stored block verbatim into
    # fresh pages, without decoding or changing the disposition.
    def stored_payload
      return planning_payload if @form == :inline
      return compressed_bytes if @form == :inline_comp
      output = +''.b
      each_overflow { |_, page| output << page[:payload].byteslice(0, page[:item_count]) }
      output
    end

    def each_overflow_page
      return enum_for(:each_overflow_page) unless block_given?
      each_overflow { |index, _| yield index }
      self
    end

    private

    def check_span_length(expected)
      raise Error.new('XX001', 'stored value span length mismatch') unless @stored_bytes.bytesize == expected
    end

    def each_overflow
      return unless external?
      index, remaining = @first_page, @stored_length
      seen = Set.new
      capacity = @pool.page_size - FileFormat::PAGE_HEADER
      until index.zero?
        raise Error.new('XX001', 'overflow chain cycle') unless seen.add?(index)
        page = @pool.page(index)
        count = page[:item_count]
        unless page[:type] == FileFormat::PAGE_OVERFLOW && count.positive? && count <= capacity && count <= remaining
          raise Error.new('XX001', 'invalid overflow chain')
        end
        remaining -= count
        following = page[:next_page]
        raise Error.new('XX001', 'overflow length mismatch') unless following.zero? == remaining.zero?
        yield index, page
        index = following
      end
      raise Error.new('XX001', 'overflow length mismatch') unless remaining.zero?
    end
  end
end
