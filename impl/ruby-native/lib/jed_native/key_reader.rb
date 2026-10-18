# frozen_string_literal: true

module JedNative
  # Skip a self-delimiting ordered key body without converting it back to a SQL
  # value. Descending slots invert their body, including nested delimiters.
  module KeyReader
    def skip_key_body(type, bytes, offset = 0, collation = nil, inverted = false)
      byte = ->(at) { value = bytes.getbyte(at); raise Error.new('XX001', 'truncated index key') unless value; inverted ? value ^ 255 : value }
      if (width = fixed_width(type))
        finish = offset + width
      elsif (element = array_elem(type))
        loop do
          tag = byte.call(offset)
          offset += 1
          break if tag.zero?
          raise Error.new('XX001', 'invalid array key marker') unless [1, 2].include?(tag)
          offset = skip_key_body(element, bytes, offset, collation, inverted) if tag == 1
        end
        finish = offset + 1 + 8 * byte.call(offset)
      elsif (element = range_elem(type))
        empty = byte.call(offset).zero?
        offset += 1
        unless empty
          2.times do
            tag = byte.call(offset)
            offset += 1
            offset = skip_key_body(element, bytes, offset, nil, inverted) + 1 if tag == 1
          end
        end
        finish = offset
      elsif (fields = composite_fields(type))
        fields.each do |field|
          tag = byte.call(offset)
          offset += 1
          offset = skip_key_body(field[:type], bytes, offset, nil, inverted) if tag.zero?
        end
        finish = offset
      elsif type == 'decimal'
        tag = byte.call(offset)
        finish = offset + 1
        unless tag == 4
          raise Error.new('XX001', 'invalid decimal key class') unless [3, 5].include?(tag)
          finish += 4
          terminator = tag == 3 ? 255 : 0
          finish += 1 while byte.call(finish) != terminator
          finish += 1
        end
      elsif %w[text bytea].include?(type)
        if collation && type == 'text'
          3.times do
            loop do
              a, b = byte.call(offset), byte.call(offset + 1)
              offset += 2
              break if a.zero? && b.zero?
            end
          end
        end
        loop do
          a = byte.call(offset)
          offset += 1
          next unless a.zero?
          tail = byte.call(offset)
          offset += 1
          break if tail == 1
          raise Error.new('XX001', 'invalid text key escape') unless tail == 255
        end
        finish = offset
      else
        raise Error.new('XX001', "unsupported stored index key #{type}")
      end
      raise Error.new('XX001', 'truncated index key') if finish > bytes.bytesize
      finish
    end
  end
  FileFormat.include(KeyReader)
end
