# frozen_string_literal: true

module JedNative
  # Only implicit statements live here. Public prepare still returns an
  # independent value. Entries retain SQL and its parsed tree, never execution
  # state, bindings, results, a session, or a database. Existing engine caches
  # revalidate the tree against the executing session's current context.
  class StatementCache
    MAX_ENTRIES = 64
    MAX_SQL_BYTES = 65_536
    MAX_STATEMENT_BYTES = 16_384

    def initialize
      @entries, @sql_bytes, @mutex, @closed = {}, 0, Mutex.new, false
    end

    def fetch(sql)
      return Statement.new(sql) if sql.bytesize > MAX_STATEMENT_BYTES
      # ASCII-only Strings compare equal across compatible encodings. Keep
      # encoding in the key so literal results retain fresh-parse ownership.
      lookup = [sql.encoding, sql]
      hit = @mutex.synchronize do
        entry = @entries.delete(lookup)
        @entries[entry[0]] = entry if entry
        entry
      end
      return hit[1] if hit

      text = sql.dup.freeze
      key = [text.encoding, text].freeze
      statement = Statement.new(text)
      # EXPLAIN annotates relation nodes with execution-specific plan details.
      # Such trees must remain private to this invocation.
      return statement if statement.ast[:kind] == :explain
      @mutex.synchronize do
        return statement if @closed
        # Another session may have parsed the same SQL while this one parsed.
        return @entries[key][1] if @entries.key?(key)
        while @entries.length >= MAX_ENTRIES || @sql_bytes + text.bytesize > MAX_SQL_BYTES
          oldest, = @entries.shift
          @sql_bytes -= oldest[1].bytesize
        end
        @entries[key] = [key, statement]
        @sql_bytes += text.bytesize
      end
      statement
    end

    def close
      @mutex.synchronize do
        @entries.clear
        @sql_bytes = 0
        @closed = true
      end
    end
  end

  module CachedStatements
    private

    def cached_statement(sql)
      # Settings and input validity apply even when an identical tree exists.
      raise Error.new('54000', 'SQL input exceeds max_sql_length') if @max_sql_length.positive? && sql.bytesize > @max_sql_length
      return prepare(sql) unless sql.is_a?(String)
      raise Error.new('22021', 'invalid UTF-8 in SQL input') unless sql.valid_encoding?
      cache = @native_statement_cache || @database.instance_variable_get(:@native_statement_cache)
      cache.fetch(sql)
    end
  end
  Database.include(CachedStatements)
end
