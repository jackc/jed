# frozen_string_literal: true

require "fiddle"

module Jed
  # An open jed database handle (spec/design/ruby.md §2). Wraps the native handle and drives it
  # through the C ABI. Single-writer, autocommit by default — the same model every jed host sees
  # (CLAUDE.md §3). Prefer the block forms ({Database.memory}, {.create}, {.open}), which close the
  # handle automatically; otherwise call {#close} when done.
  #
  # Each open takes an optional `extensions:` {Jed::ExtensionRegistry}; the handle gets the host
  # functions registered so far, frozen for its life (spec/design/ruby.md §5b).
  class Database
    class << self
      # Open a new in-memory database. With a block, yields the database and closes it after.
      def memory(extensions: nil)
        handle, functions = with_registry(extensions) { |reg| Jed::FFI::OPEN_MEMORY.call(reg) }
        db = new(handle, functions)
        return db unless block_given?

        manage(db) { yield db }
      end

      # Create a new file-backed database at `path` (`58P02` if it already exists). With a block,
      # yields the database and closes it after.
      def create(path, extensions: nil)
        result, functions = with_registry(extensions) do |reg|
          Jed::Codec.take(Jed::FFI::CREATE.call(path.to_s, reg))
        end
        db = handle_or_raise(result, functions)
        return db unless block_given?

        manage(db) { yield db }
      end

      # Open an existing file-backed database at `path` (`58P01` if missing). `read_only: true`
      # opens it like a PG hot standby — every write is `25006`. With a block, yields and closes.
      def open(path, read_only: false, extensions: nil)
        result, functions = with_registry(extensions) do |reg|
          Jed::Codec.take(Jed::FFI::OPEN.call(path.to_s, read_only ? 1 : 0, reg))
        end
        db = handle_or_raise(result, functions)
        return db unless block_given?

        manage(db) { yield db }
      end

      private

      # Run an open with the native registry pointer (null for none), returning its value and the
      # host functions the handle must keep alive.
      def with_registry(extensions)
        return [yield(Fiddle::NULL), [].freeze] if extensions.nil?
        unless extensions.is_a?(Jed::ExtensionRegistry)
          raise ArgumentError, "extensions: must be a Jed::ExtensionRegistry, got #{extensions.class}"
        end

        extensions.open_with { |reg| yield reg }
      end

      def handle_or_raise(result, functions)
        raise Jed::Error.new(result[:sqlstate], result[:message]) if result[:kind] == :error

        new(Fiddle::Pointer.new(result[:ptr]), functions)
      end

      def manage(db)
        yield
      ensure
        db.close
      end
    end

    def initialize(handle, host_functions = [].freeze)
      @handle = handle
      @addr = handle.to_i
      @closed = false
      # The host functions this handle was opened with. Holding them keeps their Fiddle closures —
      # whose trampolines the engine calls — alive for the handle's life (ruby.md §5b "Lifetime").
      @host_functions = host_functions
      # Serializes every native call on this handle: one thread at a time while the GVL is released
      # during a query, and a re-entrant call from a host function is refused (ruby.md §5b).
      @lock = Mutex.new
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
      buf = Jed::Params.encode(params)
      ptr = buf || Fiddle::NULL
      len = buf ? buf.bytesize : 0
      result, recorded = native do
        Jed::HostFunction.capture { Jed::Codec.take(Jed::FFI::EXECUTE.call(@handle, sql.to_s, ptr, len)) }
      end
      # An interrupt, signal, or exit raised inside a host function is the host's, not a SQL error:
      # re-raise it as itself (ruby.md §5b).
      signal = recorded.find { |(e, _, _)| !e.is_a?(StandardError) }
      raise signal[0] if signal

      case result[:kind]
      when :error
        # The Ruby exception a host function failed the statement with, if that is this error.
        cause = recorded.find { |(_, state, msg)| state == result[:sqlstate] && msg == result[:message] }
        raise Jed::Error.new(result[:sqlstate], result[:message]), cause: cause&.first
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
      result = native { Jed::Codec.take(Jed::FFI::COMMIT.call(@handle)) }
      raise Jed::Error.new(result[:sqlstate], result[:message]) if result[:kind] == :error

      self
    end

    # Close the handle (rolls back any open explicit transaction; never commits implicitly). Safe to
    # call more than once. Returns nil.
    def close
      reentrancy_check
      @lock.synchronize do
        return if @closed

        @closed = true
        ObjectSpace.undefine_finalizer(self)
        Jed::FFI::CLOSE.call(@handle)
        @handle = nil
      end
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

    # Run a native call on this handle under its lock, refusing a re-entrant call from a host function
    # (which would alias the handle the running statement holds — ruby.md §5b).
    def native
      reentrancy_check
      @lock.synchronize do
        check_open
        yield
      end
    end

    def reentrancy_check
      return unless @lock.owned?

      raise Jed::Error.new("55006",
        "database handle is in use by the statement running this host function; a host function " \
        "cannot use the handle that called it (spec/design/ruby.md §5b)")
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
