# frozen_string_literal: true

require "fiddle"

module Jed
  # An open jed database handle (spec/design/ruby.md §2). Wraps the native handle and drives it
  # through the C ABI. Single-writer, autocommit by default — the same model every jed host sees
  # (CLAUDE.md §3). Prefer the block forms ({Database.memory}, {.create}, {.open}), which close the
  # handle automatically; otherwise call {#close} when done.
  class Database
    class << self
      # Open a new in-memory database. With a block, yields the database and closes it after.
      def memory
        db = new(Jed::FFI::OPEN_MEMORY.call)
        return db unless block_given?

        manage(db) { yield db }
      end

      # Create a new file-backed database at `path` (`58P02` if it already exists). With a block,
      # yields the database and closes it after.
      def create(path)
        db = handle_or_raise(Jed::Codec.take(Jed::FFI::CREATE.call(path.to_s)))
        return db unless block_given?

        manage(db) { yield db }
      end

      # Open an existing file-backed database at `path` (`58P01` if missing). `read_only: true`
      # opens it like a PG hot standby — every write is `25006`. With a block, yields and closes.
      def open(path, read_only: false)
        result = Jed::Codec.take(Jed::FFI::OPEN.call(path.to_s, read_only ? 1 : 0))
        db = handle_or_raise(result)
        return db unless block_given?

        manage(db) { yield db }
      end

      private

      def handle_or_raise(result)
        raise Jed::Error.new(result[:sqlstate], result[:message]) if result[:kind] == :error

        new(Fiddle::Pointer.new(result[:ptr]))
      end

      def manage(db)
        yield
      ensure
        db.close
      end
    end

    def initialize(handle)
      @handle = handle
      @addr = handle.to_i
      @closed = false
      # Best-effort safety net: close the native handle if the caller forgets and the object is
      # GC'd. {#close} undefines this so an explicit close never double-frees (ruby.md §4). The
      # proc captures only the address, never `self`, so it does not pin the object.
      ObjectSpace.define_finalizer(self, self.class.send(:finalizer, @addr))
    end

    # Execute one SQL statement, binding any `$N` placeholders to `params` (positional, 1-based:
    # `$1` ⇒ the first). Returns a {Jed::Result} for a query, or a Hash `{rows_affected:, cost:}`
    # for a non-query statement (DDL/DML). Raises {Jed::Error} on a structured engine error.
    #
    #   db.execute("INSERT INTO t VALUES ($1, $2)", 7, "alice")
    #   db.execute("UPDATE t SET v = $1 WHERE id = $2", 10, 7)
    #
    # Each param is `nil`/`Integer`/`Float`/`true`/`false`/`String`; the engine context-types every
    # `$N` and coerces it (ruby.md §3a). Pass an array of values with the splat: `db.execute(sql, *vals)`.
    def execute(sql, *params)
      check_open
      buf = Jed::Params.encode(params)
      ptr = buf || Fiddle::NULL
      len = buf ? buf.bytesize : 0
      result = Jed::Codec.take(Jed::FFI::EXECUTE.call(@handle, sql.to_s, ptr, len))
      case result[:kind]
      when :error then raise Jed::Error.new(result[:sqlstate], result[:message])
      when :query then build_result(result)
      when :statement then { rows_affected: result[:rows_affected], cost: result[:cost] }
      else raise Jed::LoadError, "unexpected result kind #{result[:kind].inspect}"
      end
    end

    # Execute a query, binding `$N` params, and return a {Jed::Result}. Raises if the statement
    # produces no rows (use {#execute} for DDL/DML).
    def query(sql, *params)
      result = execute(sql, *params)
      return result if result.is_a?(Jed::Result)

      raise Jed::Error.new("42601",
        "query() called on a statement that produces no rows; use execute()")
    end

    # Commit the current transaction, making prior writes durable (per `synchronous`). On an
    # in-memory database this is a no-op success. Returns self. Raises {Jed::Error} on failure.
    def commit
      check_open
      result = Jed::Codec.take(Jed::FFI::COMMIT.call(@handle))
      raise Jed::Error.new(result[:sqlstate], result[:message]) if result[:kind] == :error

      self
    end

    # Close the handle (rolls back any open explicit transaction; never commits implicitly). Safe to
    # call more than once. Returns nil.
    def close
      return if @closed

      @closed = true
      ObjectSpace.undefine_finalizer(self)
      Jed::FFI::CLOSE.call(@handle)
      @handle = nil
      nil
    end

    def closed? = @closed

    def self.finalizer(addr)
      proc { Jed::FFI::CLOSE.call(Fiddle::Pointer.new(addr)) }
    end
    private_class_method :finalizer

    private

    def check_open
      raise Jed::Error.new("XX000", "database handle is closed") if @closed
    end

    def build_result(result)
      types = result[:types]
      rows = result[:rows].map do |row|
        row.each_with_index.map { |raw, col| Jed::Coerce.value(types[col], raw) }
      end
      Jed::Result.new(columns: result[:columns], column_types: types, rows: rows, cost: result[:cost])
    end
  end
end
