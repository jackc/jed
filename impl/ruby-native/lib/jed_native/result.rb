# frozen_string_literal: true

module JedNative
  # A result and a binding are owned by the caller. Mutable containers and
  # strings must never alias a stored cell or a retained prepared expression.
  module HostValues
    module_function
    def copy(value)
      case value
      when String then value.dup
      when ArrayValue
        ArrayValue.new(value.map { |element| copy(element) }, element_type: value.element_type&.dup,
                       dimensions: value.dimensions.dup, lower_bounds: value.lower_bounds.dup)
      when CompositeValue
        CompositeValue.new(value.map { |element| copy(element) }, type_name: value.type_name.dup,
                           field_names: value.field_names&.map(&:dup))
      when Array then value.map { |element| copy(element) }
      when Hash then value.to_h { |key, element| [copy(key), copy(element)] }
      when JsonValue then JsonValue.new(copy(value.value), binary: value.binary, source: value.source&.dup)
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
