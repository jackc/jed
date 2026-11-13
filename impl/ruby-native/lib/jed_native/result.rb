# frozen_string_literal: true

module JedNative
  # A result and a binding are owned by the caller. Mutable containers and
  # strings must never alias a stored cell or a retained prepared expression.
  module HostValues
    module_function
    def copy(value, validate: true)
      case value
      when String
        copy = value.dup
        unless value.is_a?(Bytea) || value.encoding == Encoding::BINARY || !validate
          copy.force_encoding(Encoding::UTF_8)
          raise Error.new('22021', 'text must be valid UTF-8 without NUL') unless copy.valid_encoding? && !copy.include?("\0")
        end
        copy
      when ArrayValue
        ArrayValue.new(value.map { |element| copy(element, validate: validate) }, element_type: value.element_type&.dup,
                       dimensions: value.dimensions.dup, lower_bounds: value.lower_bounds.dup)
      when CompositeValue
        CompositeValue.new(value.map { |element| copy(element, validate: validate) }, type_name: value.type_name.dup,
                           field_names: value.field_names&.map(&:dup))
      when Array then value.map { |element| copy(element, validate: validate) }
      when Hash then value.to_h { |key, element| [copy(key, validate: validate), copy(element, validate: validate)] }
      when JsonValue then JsonValue.new(copy(value.value, validate: value.binary && validate), binary: value.binary, source: value.source&.dup)
      else value
      end
    end
  end

  class Row < Array
    def initialize(values, names)
      super(values)
      @names = names
    end

    def [](key, *rest)
      return super if key.is_a?(Integer) || key.is_a?(Range)
      index = @names.index(key.to_s)
      index ? super(index) : nil
    end

    def to_h = @names.zip(self).to_h
  end

  class Result
    include Enumerable
    attr_reader :columns, :column_types, :rows, :cost, :rows_affected
    alias column_names columns

    def initialize(columns: [], column_types: [], rows: [], cost: 0, rows_affected: nil)
      @columns, @column_types, @rows = columns, column_types, rows
      @cost, @rows_affected = cost, rows_affected
    end

    def each
      return enum_for(:each) unless block_given?
      @rows.each { |values| yield Row.new(values, @columns) }
      self
    end

    def close = nil
    def empty? = @rows.empty?
    def size = @rows.size

    def detach
      @columns = @columns.map(&:dup)
      @column_types = @column_types.map(&:dup)
      @rows = @rows.map { |row| row.map { |value| HostValues.copy(value) } }
      self
    end
  end
end
