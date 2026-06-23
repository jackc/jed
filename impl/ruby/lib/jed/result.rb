# frozen_string_literal: true

module Jed
  # A materialized query result (spec/design/ruby.md §2): the output column names, their canonical
  # type names, the coerced rows, and the deterministic execution cost (CLAUDE.md §13). Enumerable
  # over its {Row}s.
  class Result
    include Enumerable

    attr_reader :columns, :column_types, :cost

    def initialize(columns:, column_types:, rows:, cost:)
      @columns = columns
      @column_types = column_types
      @cost = cost
      @rows = rows.map { |values| Row.new(columns, values) }
    end

    def each(&) = @rows.each(&)
    def [](index) = @rows[index]
    def size = @rows.size
    alias length size
    def empty? = @rows.empty?

    # Every row as a plain Array of values, in column order.
    def values = @rows.map(&:values)

    def to_a = @rows.dup

    def inspect
      "#<Jed::Result #{columns.inspect} #{size} row#{size == 1 ? '' : 's'} cost=#{cost}>"
    end
  end

  # One result row: positional and by-name access to its coerced values (spec/design/ruby.md §2).
  class Row
    include Enumerable

    def initialize(columns, values)
      @columns = columns
      @values = values
    end

    # The coerced values in column order.
    def values = @values

    def to_a = @values.dup

    # Access a value by column index (Integer) or column name (String/Symbol).
    def [](key)
      if key.is_a?(Integer)
        @values[key]
      else
        idx = @columns.index(key.to_s)
        idx && @values[idx]
      end
    end

    def to_h = @columns.zip(@values).to_h
    def each(&) = @values.each(&)
    def size = @values.size
    def inspect = "#<Jed::Row #{to_h.inspect}>"
  end
end
