# frozen_string_literal: true

module JedNative
  # Handwritten from spec/pageservice/messages.toml. No SQL or Ruby objects cross
  # this protocol: the payloads are fixed integers, UTF-8 strings and page bytes.
  module PageServiceWire
    MAX_FRAME = 16_777_216
    MESSAGES = {
      hello: [1, 'protocol_min:u16 protocol_max:u16 format_version:u16 flags:u8 client_name:string credential:credential'],
      begin_read: [2, 'incarnation:id16 last_txid:u64'], read_pages: [3, 'txid:u64 indices:u32_list'],
      end_read: [4, 'txid:u64'], begin_write: [5, 'incarnation:id16 last_txid:u64 lock_timeout_ms:u64 flags:u8'],
      commit_begin: [6, 'txid:u64 meta:page manifest_pages:indexed_page_list'],
      commit_pages: [7, 'pages:indexed_page_list'], commit_end: [8, ''], abort: [9, ''],
      commit_status: [10, 'txid:u64 digest:u64'], request_ownership: [11, ''],
      hello_ok: [65, 'protocol:u16 format_version:u16 page_size:u32 connection_id:u64 flags:u8 max_read_pages:u32'],
      read_begun: [66, 'incarnation:id16 txid:u64 invalidate:u8 indices:u32_list meta:page'],
      pages: [67, 'pages:page_list'],
      write_begun: [69, 'incarnation:id16 txid:u64 invalidate:u8 indices:u32_list reuse_grant:bool meta:page pending:digest_list reuse:u32_list'],
      committed: [72, 'txid:u64'], commit_status_reply: [74, 'status:u8'], ownership_reply: [75, 'status:u8'],
      notice: [126, 'kind:u8 connection_id:u64'], error: [127, 'sqlstate:sqlstate message:string detail:string flags:u8']
    }.transform_values { |code, fields| [code, fields.split.map { |field| field.split(':').map(&:to_sym) }.freeze].freeze }.freeze
    BY_CODE = MESSAGES.to_h { |name, (code, _)| [code, name] }.freeze
    INTEGERS = { u8: [1, 'C'], u16: [2, 'n'], u32: [4, 'N'], u64: [8, 'Q>'] }.freeze
    OWNER_FIELDS = [[:flags, :u8], [:uid, :u32], [:incarnation, :id16], [:pid, :u32], [:endpoint, :string]].freeze

    class Reader
      attr_reader :position
      def initialize(bytes) = (@bytes, @position = bytes.b, 0)
      def remaining = @bytes.bytesize - @position
      def take(length)
        raise Error.new('08P01', 'truncated page-service message') if length.negative? || length > remaining
        value = @bytes.byteslice(@position, length)
        @position += length
        value
      end
    end

    module_function

    def encode_value(type, value, page_size)
      if (integer = INTEGERS[type])
        raise Error.new('08P01', 'integer outside wire range') unless value.is_a?(Integer) && value.between?(0, (1 << (integer[0] * 8)) - 1)
        return [value].pack(integer[1])
      end
      case type
      when :bool
        raise Error.new('08P01', 'invalid boolean') unless value == true || value == false
        value ? "\1".b : "\0".b
      when :id16
        raise Error.new('08P01', 'invalid incarnation') unless value.bytesize == 16
        value.b
      when :sqlstate
        raise Error.new('08P01', 'invalid SQLSTATE') unless value.match?(/\A[0-9A-Z]{5}\z/)
        value.b
      when :string
        raise Error.new('08P01', 'invalid UTF-8') unless value.dup.force_encoding(Encoding::UTF_8).valid_encoding?
        [value.bytesize].pack('N') + value.b
      when :page
        raise Error.new('08P01', 'invalid page extent') unless page_size.positive? && value.bytesize == page_size
        value.b
      when :credential
        # nil (none), [:passphrase, text] or [:key, 32 bytes] (README.md §3.1).
        if value.nil?
          "\0".b
        elsif value.is_a?(Array) && value[0] == :passphrase && value[1].is_a?(String)
          "\1".b + encode_value(:string, value[1], page_size)
        elsif value.is_a?(Array) && value[0] == :key && value[1].is_a?(String) && value[1].bytesize == 32
          "\2".b + value[1].b
        else
          raise Error.new('08P01', 'invalid credential')
        end
      when :u32_list, :page_list, :indexed_page_list, :digest_list
        body = value.map do |item|
          case type
          when :u32_list then encode_value(:u32, item, page_size)
          when :digest_list then encode_value(:u32, item[0], page_size) + encode_value(:u64, item[1], page_size)
          when :page_list then encode_value(:page, item, page_size)
          else encode_value(:u32, item[0], page_size) + encode_value(:page, item[1], page_size)
          end
        end.join.b
        [value.length].pack('N') + body
      end
    end

    def decode_value(type, reader, page_size)
      if (integer = INTEGERS[type])
        return reader.take(integer[0]).unpack1(integer[1])
      end
      case type
      when :bool
        byte = reader.take(1).getbyte(0)
        raise Error.new('08P01', 'invalid boolean') if byte > 1
        byte == 1
      when :id16 then reader.take(16)
      when :sqlstate
        value = reader.take(5)
        raise Error.new('08P01', 'invalid SQLSTATE') unless value.match?(/\A[0-9A-Z]{5}\z/)
        value
      when :string
        value = reader.take(decode_value(:u32, reader, page_size)).force_encoding(Encoding::UTF_8)
        raise Error.new('08P01', 'invalid UTF-8') unless value.valid_encoding?
        value
      when :page
        raise Error.new('08P01', 'no connection page size') unless page_size.positive?
        reader.take(page_size)
      when :credential
        case reader.take(1).getbyte(0)
        when 0 then nil
        when 1 then [:passphrase, decode_value(:string, reader, page_size)]
        when 2 then [:key, reader.take(32)]
        else raise Error.new('08P01', 'invalid credential kind')
        end
      when :u32_list, :page_list, :indexed_page_list, :digest_list
        count = decode_value(:u32, reader, page_size)
        width = { u32_list: 4, page_list: page_size, indexed_page_list: page_size + 4, digest_list: 12 }.fetch(type)
        raise Error.new('08P01', 'invalid list count') if width.zero? || count > reader.remaining / width
        Array.new(count) do
          case type
          when :u32_list then decode_value(:u32, reader, page_size)
          when :digest_list then [decode_value(:u32, reader, page_size), decode_value(:u64, reader, page_size)]
          when :page_list then decode_value(:page, reader, page_size)
          else [decode_value(:u32, reader, page_size), decode_value(:page, reader, page_size)]
          end
        end
      end
    end

    def encode(name, request_id, fields = {}, page_size = 0)
      code, layout = MESSAGES.fetch(name)
      body = layout.map { |field, type| encode_value(type, fields.fetch(field), page_size) }.join.b
      raise Error.new('54000', 'page-service frame exceeds limit') if body.bytesize + 5 > MAX_FRAME
      [body.bytesize + 5, code, request_id].pack('NCN') + body
    end

    def decode(bytes, page_size = 0)
      reader = Reader.new(bytes)
      length = decode_value(:u32, reader, page_size)
      raise Error.new('08P01', 'invalid frame length') unless length.between?(5, MAX_FRAME) && length == reader.remaining
      code, request_id = reader.take(5).unpack('CN')
      name = BY_CODE[code] || raise(Error.new('08P01', 'unknown page-service message'))
      fields = MESSAGES.fetch(name)[1].to_h { |field, type| [field, decode_value(type, reader, page_size)] }
      raise Error.new('08P01', 'trailing frame bytes') unless reader.remaining.zero?
      [name, request_id, fields]
    end

    def read_exact(stream, length)
      bytes = +''.b
      while bytes.bytesize < length
        piece = stream.read(length - bytes.bytesize)
        raise Error.new('08006', 'page-service connection ended') if piece.nil? || piece.empty?
        bytes << piece
      end
      bytes
    end

    def read(stream, page_size = 0)
      prefix = read_exact(stream, 4)
      length = prefix.unpack1('N')
      raise Error.new('08P01', 'invalid frame length') unless length.between?(5, MAX_FRAME)
      decode(prefix + read_exact(stream, length), page_size)
    end

    def encode_owner(fields, codec)
      body = 'JEDOWNER'.b + [1].pack('n') + OWNER_FIELDS.map { |name, type| encode_value(type, fields.fetch(name), 0) }.join.b
      body + [codec.crc32(body)].pack('N')
    end

    def decode_owner(bytes, codec)
      reader = Reader.new(bytes)
      raise Error.new('08P01', 'invalid owner record') unless reader.take(10) == 'JEDOWNER'.b + [1].pack('n')
      fields = OWNER_FIELDS.to_h { |name, type| [name, decode_value(type, reader, 0)] }
      crc = reader.take(4).unpack1('N')
      raise Error.new('08P01', 'invalid owner record') unless fields[:flags] <= 1 && reader.remaining.zero? && crc == codec.crc32(bytes.byteslice(0...-4))
      fields
    end
  end
end
