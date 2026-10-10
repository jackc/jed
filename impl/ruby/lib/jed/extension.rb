# frozen_string_literal: true

require "fiddle"

module Jed
  # A set of host scalar functions to open databases with (spec/design/ruby.md §5b) — the gem's face of
  # the engine's host-function seam (spec/design/extensibility.md §4.2) and its batched kernel ABI
  # (§4.2.1). Each open builds the engine's registry from the functions registered so far, so a
  # handle's set is frozen when it opens; one registry serves any number of handles.
  #
  #   reg = Jed::ExtensionRegistry.new
  #   reg.register("add_tax", [:decimal, :decimal], :decimal, volatility: :immutable) { |a, r| a * (1 + r) }
  #   reg.register_batch("mix", [:i32], :i64, volatility: :immutable) do |xs, out|
  #     xs.each { |x| out << x * 2654435761 % 1000003 }
  #   end
  #   Jed.memory(extensions: reg) { |db| db.query("SELECT mix(amount) FROM orders") }
  #
  # A host function is host code, outside the engine's untrusted-query guarantee (CLAUDE.md §13).
  class ExtensionRegistry
    VOLATILITY = { immutable: 0, stable: 1, volatile: 2 }.freeze

    def initialize
      @native = Jed::FFI::REGISTRY_NEW.call
      @functions = []
      # Serializes registration against opens, which borrow the native registry (ruby.md §5b).
      @lock = Mutex.new
      ObjectSpace.define_finalizer(self, self.class.send(:finalizer, @native.to_i))
    end

    # Register a SINGLE-ROW kernel: the block receives one row's arguments and returns its result
    # (`nil` for SQL NULL). Called once per row. `arg_types`/`result` are jed type names (Symbols or
    # Strings, aliases included). Raises {Jed::Error} with the engine's code on a duplicate signature
    # (`42723`), a negative cost (`22023`), an unknown type (`42704`), or an unsupported one (`0A000`).
    def register(name, arg_types, result, volatility: :volatile, cost: 1, &block)
      add(name, arg_types, result, volatility, cost, false, block)
    end

    # Register a BATCH kernel: the block receives one Array per argument (`columns[j][i]` is argument
    # `j` of row `i`), then an empty output Array, and appends one result per row in row order. To
    # fail, raise: the results already appended are the rows that succeeded, so `out.length` is the
    # failing row (the §4.2.1 prefix rule). A non-volatile batch kernel is called once per chunk of up
    # to 1,024 rows where the engine holds one; elsewhere with a one-row batch.
    def register_batch(name, arg_types, result, volatility: :volatile, cost: 1, &block)
      add(name, arg_types, result, volatility, cost, true, block)
    end

    # Yield the native registry pointer while no registration can change it, and return the block's
    # value with the function list the open captured (the {Database} keeps it to keep the closures
    # alive). Internal to the gem.
    def open_with
      @lock.synchronize { [yield(@native), @functions.dup.freeze] }
    end

    def self.finalizer(addr)
      proc { Jed::FFI::REGISTRY_FREE.call(Fiddle::Pointer.new(addr)) }
    end
    private_class_method :finalizer

    private

    def add(name, arg_types, result, volatility, cost, batched, block)
      raise ArgumentError, "a host function needs a block" unless block

      vol = VOLATILITY.fetch(volatility) do
        raise ArgumentError, "volatility must be one of #{VOLATILITY.keys.inspect}, got #{volatility.inspect}"
      end
      raise ArgumentError, "cost must be an Integer, got #{cost.inspect}" unless cost.is_a?(Integer)

      types = Array(arg_types).map { |t| type_name(t) }
      fn = HostFunction.new(name.to_s, batched, block)
      @lock.synchronize do
        res = Jed::Codec.take(Jed::FFI::REGISTRY_REGISTER.call(
          @native, name.to_s, types.join(","), type_name(result), vol, cost, batched ? 1 : 0,
          fn.closure.to_i, 0
        ))
        raise Jed::Error.new(res[:sqlstate], res[:message]) if res[:kind] == :error

        fn.bind_types(res[:types])
        @functions << fn
      end
      self
    end

    def type_name(type)
      unless type.is_a?(Symbol) || type.is_a?(String)
        raise ArgumentError, "a type is named by a Symbol or String, got #{type.inspect}"
      end
      if type.to_s.include?(",")
        raise ArgumentError, "invalid type name #{type.to_s.inspect}"
      end

      type.to_s
    end
  end

  # One registered host function: the Ruby block, the Fiddle closure the engine calls, and the
  # marshalling between them (spec/design/ruby.md §5b). Internal to the gem.
  class HostFunction
    attr_reader :name, :closure

    INT_TAG = [Jed::Params::TAG_INT].pack("C").freeze
    NO_ERROR = "\x00".b.freeze
    FORM_TAGGED = "\x00".b.freeze
    FORM_COLUMN = "\x01".b.freeze

    # The types with a fixed-width column encoding (ruby.md §5b), and their `pack` directives.
    COLUMN_KINDS = {
      "i16" => :int, "i32" => :int, "i64" => :int, "f64" => :f64, "boolean" => :bool,
    }.freeze
    ROW_FORMATS = { int: "q<", f64: "E", bool: "C" }.freeze

    # The fiber-local list of exceptions recorded during the current {Database#execute} (see
    # {.capture}).
    RECORDED = :__jed_host_function_errors

    # Run the block (one native execute) with a fresh list of the host-function exceptions raised
    # inside it, and return `[block value, recorded]`. Each record is `[exception, sqlstate, message]`.
    # Nested captures (a kernel executing on another handle) restore the outer list.
    def self.capture
      outer = Thread.current[RECORDED]
      recorded = Thread.current[RECORDED] = []
      [yield, recorded]
    ensure
      Thread.current[RECORDED] = outer
    end

    def initialize(name, batched, block)
      @name = name
      @batched = batched
      @block = block
      @closure = Callback.new(self)
    end

    # Install the canonical argument and result type names the engine resolved at registration.
    def bind_types(types)
      @arg_types = types[0...-1].freeze
      @result = types.last
      @decoders = @arg_types.map { |t| decoder(t) }.freeze
      @column_kind = COLUMN_KINDS[@result]
      # The single-row fast path: when every argument has a fixed-width encoding, one `unpack` reads
      # the whole row, and a fixed-width result is written by one `pack`.
      kinds = @arg_types.map { |t| COLUMN_KINDS[t] }
      @row_format = ("x4" + kinds.map { |k| ROW_FORMATS[k] }.join).freeze if kinds.all?
      @row_bools = kinds.each_index.select { |i| kinds[i] == :bool }.freeze
      @row_result_format = ("CL<" + ROW_FORMATS[@column_kind] + "C").freeze if @column_kind
    end

    # The upcall (ruby.md §5b): decode the argument columns, run the block, and write the encoded
    # results to `out` (or the sink when they do not fit). Returns the result length. Called only by
    # {Callback#call}, which keeps any exit from crossing the native frames.
    def invoke(args_addr, args_len, out_addr, out_cap, sink)
      if @row_format && !@batched
        return deliver(invoke_row(args_addr, args_len), out_addr, out_cap, sink)
      end

      input = Fiddle::Pointer.read(args_addr, args_len)
      nrows = input.unpack1("L<")
      pos = 4
      columns = @decoders.map do |decode|
        col, pos = decode.call(input, pos, nrows)
        col
      end
      results = []
      error = nil
      begin
        if @batched
          @block.call(*columns, results)
        else
          results << @block.call(*columns.map(&:first))
        end
      rescue Exception => e # rubocop:disable Lint/RescueException -- nothing may unwind into Rust
        error = e
      end
      deliver(encode(results, error), out_addr, out_cap, sink)
    end

    private

    # Write the result buffer into the caller's scratch, or hand it to the sink when it does not fit,
    # and return its length (ruby.md §5b).
    def deliver(payload, out_addr, out_cap, sink)
      if payload.bytesize <= out_cap
        Fiddle::Pointer.write(out_addr, payload)
      else
        Jed::FFI::HOST_RESULT.call(sink, payload, payload.bytesize)
      end
      payload.bytesize
    end

    # The result buffer for a single-row kernel over fixed-width arguments: one `unpack` reads the row,
    # and a fixed-width result of its exact class is written by one `pack`.
    def invoke_row(args_addr, args_len)
      args = Fiddle::Pointer.read(args_addr, args_len).unpack(@row_format)
      @row_bools.each { |i| args[i] = args[i] != 0 }
      begin
        value = @block.call(*args)
      rescue Exception => e # rubocop:disable Lint/RescueException -- nothing may unwind into Rust
        return encode([], e)
      end
      row_payload(value) || encode([value], nil)
    end

    # A one-row column-form result for `value` when it is exactly the declared fixed-width class.
    def row_payload(value)
      case @column_kind
      when :int
        return unless value.is_a?(Integer) && value.between?(Jed::Params::I64_MIN, Jed::Params::I64_MAX)
      when :f64
        return unless value.is_a?(Float)
      when :bool
        return unless value == true || value == false

        value = value ? 1 : 0
      else
        return
      end
      [1, 1, value, 0].pack(@row_result_format)
    end

    # The result buffer: `u8 form ; u32 n ; n values ; u8 has_error ; [5] sqlstate ; lstr message`.
    # The column form packs the whole result column in one call when every value has the exact class
    # of a fixed-width result type; otherwise each value is tagged (§3a). A value the gem cannot encode
    # fails at its own row, superseding any later error.
    def encode(results, error)
      column = encode_column(results)
      if column
        buf = String.new(capacity: 16 + column.bytesize, encoding: Encoding::BINARY)
        buf << FORM_COLUMN << [results.length].pack("L<") << column
      else
        buf = String.new(capacity: 16 + (results.length * 9), encoding: Encoding::BINARY)
        buf << FORM_TAGGED << "\x00\x00\x00\x00".b # the count, back-filled below
        count = 0
        results.each do |value|
          mark = buf.bytesize
          failure = encode_value(buf, value)
          if failure
            buf.slice!(mark, buf.bytesize - mark)
            error = failure
            break
          end
          count += 1
        end
        buf[1, 4] = [count].pack("L<")
      end
      return buf << NO_ERROR if error.nil?

      sqlstate, message = error_fields(error)
      buf << "\x01".b << sqlstate.b.ljust(5, " ")[0, 5]
      bytes = message.encode(Encoding::UTF_8, invalid: :replace, undef: :replace).b
      buf << [bytes.bytesize].pack("L<") << bytes
    end

    # The whole result column in the declared type's fixed-width encoding, or nil when the type has
    # none or some value is not exactly its class (then the tagged form decides each value). `pack`
    # silently wraps an out-of-range Integer, so the range is checked first.
    def encode_column(results)
      case @column_kind
      when :int
        return nil unless results.all?(Integer)

        min, max = results.minmax
        return nil if min && (min < Jed::Params::I64_MIN || max > Jed::Params::I64_MAX)

        results.pack("q<*")
      when :f64
        results.pack("E*") if results.all?(Float)
      when :bool
        results.map { |v| v == true ? 1 : 0 }.pack("C*") if results.all? { |v| v == true || v == false }
      end
    end

    # Append one result value, or return the error (a [sqlstate, message] pair) that fails its row.
    def encode_value(buf, value)
      if value.is_a?(Integer)
        if value.between?(Jed::Params::I64_MIN, Jed::Params::I64_MAX)
          buf << INT_TAG
          [value].pack("q<", buffer: buf)
          return nil
        end
        unless @result == "decimal"
          return ["22003", "host function #{@name} returned #{value}, out of range for #{@result}"]
        end

        Jed::Params.append_decimal(buf, BigDecimal(value))
        return nil
      end
      Jed::Params.append(buf, value)
      nil
    rescue ArgumentError, EncodingError => e
      ["22000", "host function #{@name} returned a value jed cannot take as #{@result}: #{e.message}"]
    end

    # The SQLSTATE and message an exception from the block fails its row with, recording it so
    # {Database#execute} can attach it as the cause (or re-raise a non-StandardError as itself).
    def error_fields(error)
      return error if error.is_a?(Array) # an encoding failure, already [sqlstate, message]

      sqlstate, message =
        if error.is_a?(Jed::Error)
          [error.sqlstate.to_s, error.raw_message.to_s]
        else
          ["38000", "host function #{@name} raised #{error.class}: #{error.message}"]
        end
      Thread.current[RECORDED]&.push([error, sqlstate, message])
      [sqlstate, message]
    end

    # A column decoder for a canonical argument type: `(input, pos, nrows) → [values, next pos]`.
    # Numeric columns are one `unpack`; the rest are rendered values coerced like a query cell (§3).
    def decoder(type)
      case type
      when "i16", "i32", "i64"
        ->(input, pos, n) { [input.unpack("q<#{n}", offset: pos), pos + (8 * n)] }
      when "f64"
        ->(input, pos, n) { [input.unpack("E#{n}", offset: pos), pos + (8 * n)] }
      when "boolean"
        ->(input, pos, n) { [input.unpack("C#{n}", offset: pos).map { |b| b != 0 }, pos + n] }
      else
        lambda do |input, pos, n|
          col = Array.new(n) do
            len = input.unpack1("L<", offset: pos)
            text = input.byteslice(pos + 4, len).force_encoding(Encoding::UTF_8)
            pos += 4 + len
            Jed::Coerce.value(type, text)
          end
          [col, pos]
        end
      end
    end

    # The Fiddle closure the engine calls (ruby.md §5b). Fiddle's trampoline calls Ruby without
    # `rb_protect`, so an exception or a jump (`throw`, `break`, `Thread#kill`) leaving {#call} would
    # longjmp across the Rust frames below it — undefined behavior. The `ensure` + explicit `return`
    # stops every such exit; a non-local exit returns -1, which the engine reports as `38000`.
    class Callback < Fiddle::Closure
      ARGS = [
        Jed::FFI::UINTPTR, # user_data
        Jed::FFI::UINTPTR, # args
        Jed::FFI::U64,     # args_len
        Jed::FFI::UINTPTR, # out
        Jed::FFI::U64,     # out_cap
        Jed::FFI::UINTPTR, # sink
      ].freeze

      def initialize(function)
        @function = function
        super(Jed::FFI::I64, ARGS)
      end

      # rubocop:disable Lint/EnsureReturn, Lint/RescueException -- the point: nothing crosses into Rust
      def call(_user_data, args, args_len, out, out_cap, sink)
        status = -1
        begin
          status = @function.invoke(args, args_len, out, out_cap, sink)
        rescue Exception => e
          Thread.current[RECORDED]&.push([e, nil, nil])
          status = -1
        ensure
          return status
        end
      end
      # rubocop:enable Lint/EnsureReturn, Lint/RescueException
    end
  end
end
