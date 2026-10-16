# frozen_string_literal: true

require "fiddle"

module Jed
  # Everything a database takes from outside itself except storage, frozen (spec/design/ruby.md §5a;
  # spec/design/environment.md): the loaded Unicode data (collations + the casing table), the loaded
  # time-zone data, the host functions, and the random and clock sources. Build one with
  # {Environment.build} or an {EnvironmentBuilder}, and pass it to each open:
  #
  #   env = Jed::Environment.build do |b|
  #     b.load_unicode_data(File.binread("unicode.jucd"))
  #     b.load_time_zone_data(File.binread("tzdata.jtz"))
  #     b.register_function("add_tax", [:decimal, :decimal], :decimal, volatility: :immutable) { |a, r| a * (1 + r) }
  #   end
  #   Jed.memory(environment: env) { |db| db.query(%(SELECT 'a' < 'B' COLLATE "unicode")) }
  #
  # An environment has no mutating methods, so a database's outside inputs never change while it is
  # open. Opening without one gives the database a fresh empty environment (`C` collation, `UTC` and
  # fixed offsets, no host functions, OS randomness, the wall clock) — there is no process-wide one.
  # One environment serves any number of databases; {#to_builder} derives a new one from it.
  class Environment
    class << self
      # Build an environment: yields a fresh {EnvironmentBuilder} (when a block is given) and returns
      # what it built. Without a block, the empty environment.
      def build
        builder = EnvironmentBuilder.new
        yield builder if block_given?
        builder.build
      end

      # Wrap a native environment and the host functions it holds (internal to the gem).
      def from_native(native, functions)
        env = allocate
        env.send(:init, native, functions)
        env
      end

      def finalizer(addr)
        proc { Jed::FFI::ENV_FREE.call(Fiddle::Pointer.new(addr)) }
      end
      private :new, :finalizer
    end

    # The host functions this environment holds (internal: a {Database} keeps them, so their Fiddle
    # closures stay alive as long as any database opened with this environment — ruby.md §5b).
    attr_reader :host_functions

    # A builder pre-filled with this environment's contents, shared by reference. It may add bundles
    # and functions and replace the sources, never remove; this environment is unaffected. With a
    # block, yields the builder and returns what it builds.
    #
    #   seeded = env.to_builder { |b| b.random_source(Jed::SeededRandom.new(42)) }
    def to_builder
      builder = EnvironmentBuilder.send(:from_native, Jed::FFI::ENV_TO_BUILDER.call(@native), @host_functions)
      return builder unless block_given?

      yield builder
      builder.build
    end

    # The native environment pointer (internal to the gem).
    def native_pointer = @native

    private

    def init(native, functions)
      raise Jed::LoadError, "the native library failed to build an environment" if native.null?

      @native = native
      @host_functions = functions
      ObjectSpace.define_finalizer(self, self.class.send(:finalizer, native.to_i))
      freeze
    end
  end

  # Builds a {Jed::Environment} (spec/design/ruby.md §5a). Every call validates as it is made and
  # raises {Jed::Error} with the engine's code, leaving the builder unchanged; each returns the builder,
  # so calls chain. {#build} freezes the current contents into an environment and leaves the builder
  # usable, so building more later affects only environments built later.
  class EnvironmentBuilder
    VOLATILITY = { immutable: 0, stable: 1, volatile: 2 }.freeze

    def initialize
      init(Jed::FFI::ENV_BUILDER_NEW.call, [])
    end

    class << self
      # A builder over an existing native builder (internal: {Environment#to_builder}).
      def from_native(native, functions)
        builder = allocate
        builder.send(:init, native, functions.dup)
        builder
      end

      def finalizer(addr)
        proc { Jed::FFI::ENV_BUILDER_FREE.call(Fiddle::Pointer.new(addr)) }
      end
      private :from_native, :finalizer
    end

    # Load a Unicode collation bundle (a `JUCD` byte string, spec/design/collation.md): the linguistic
    # collations it provides (`COLLATE "unicode"`) and its casing table (`upper`/`lower`, `ILIKE`).
    # First-wins, so loading the same bundle twice changes nothing. Raises {Jed::Error} (`XX001`) on a
    # malformed bundle. Typical use: `b.load_unicode_data(File.binread(path))`.
    def load_unicode_data(bytes) = load_bundle(Jed::FFI::ENV_LOAD_UNICODE, bytes)

    # Load an IANA time-zone bundle (a `JTZ` byte string, spec/design/timezones.md): the named zones
    # it provides (`AT TIME ZONE 'America/New_York'`, `date_trunc(…, zone)`, the session `time_zone`).
    # `UTC` and fixed offsets are built in. First-wins. Raises {Jed::Error} (`XX001`) on a malformed
    # bundle.
    def load_time_zone_data(bytes) = load_bundle(Jed::FFI::ENV_LOAD_TIMEZONE, bytes)

    # Register a SINGLE-ROW host function (spec/design/ruby.md §5b): the block receives one row's
    # arguments and returns its result (`nil` for SQL NULL). Called once per row. `arg_types`/`result`
    # are jed type names (Symbols or Strings, aliases included). Raises {Jed::Error} with the engine's
    # code on a duplicate signature (`42723`), a negative cost (`22023`), an unknown type (`42704`), or
    # an unsupported one (`0A000`).
    def register_function(name, arg_types, result, volatility: :volatile, cost: 1, &block)
      add(name, arg_types, result, volatility, cost, false, block)
    end

    # Register a BATCH host function: the block receives one Array per argument (`columns[j][i]` is
    # argument `j` of row `i`), then an empty output Array, and appends one result per row in row
    # order. To fail, raise: the results already appended are the rows that succeeded, so
    # `out.length` is the failing row (the §4.2.1 prefix rule). A non-volatile batch kernel is called
    # once per chunk of up to 1,024 rows where the engine holds one; elsewhere with a one-row batch.
    def register_batch_function(name, arg_types, result, volatility: :volatile, cost: 1, &block)
      add(name, arg_types, result, volatility, cost, true, block)
    end

    # Use `source` — a {Jed::SeededRandom}, or `nil` for OS randomness (the default) — for the uuid
    # generators (spec/design/ruby.md §5c).
    def random_source(source)
      unless source.nil? || source.is_a?(Jed::SeededRandom)
        raise ArgumentError, "random_source takes a Jed::SeededRandom or nil, got #{source.class}"
      end

      set_source(Jed::FFI::ENV_RANDOM_SOURCE, source)
    end

    # Use `source` — a {Jed::ManualClock} or {Jed::AdvancingClock}, or `nil` for the wall clock (the
    # default) — for `now()`, `clock_timestamp()`, and `uuidv7` (spec/design/ruby.md §5c).
    def clock_source(source)
      unless source.nil? || source.is_a?(Jed::ManualClock) || source.is_a?(Jed::AdvancingClock)
        raise ArgumentError,
          "clock_source takes a Jed::ManualClock, a Jed::AdvancingClock, or nil, got #{source.class}"
      end

      set_source(Jed::FFI::ENV_CLOCK_SOURCE, source)
    end

    # Freeze the builder's current contents into a {Jed::Environment}. The builder stays usable.
    def build
      @lock.synchronize { Environment.from_native(Jed::FFI::ENV_BUILD.call(@native), @functions.dup.freeze) }
    end

    private

    def init(native, functions)
      raise Jed::LoadError, "the native library failed to make an environment builder" if native.null?

      @native = native
      # The host functions registered so far; each environment built keeps its own copy.
      @functions = functions
      # Serializes every native call on this builder.
      @lock = Mutex.new
      ObjectSpace.define_finalizer(self, self.class.send(:finalizer, native.to_i))
    end

    def load_bundle(fn, bytes)
      raw = bytes.to_s.b # a binary copy; Fiddle passes a pointer to its bytes
      @lock.synchronize { check(Jed::Codec.take(fn.call(@native, raw, raw.bytesize))) }
      self
    end

    def set_source(fn, source)
      @lock.synchronize { check(Jed::Codec.take(fn.call(@native, source ? source.native_pointer : Fiddle::NULL))) }
      self
    end

    def check(result)
      raise Jed::Error.new(result[:sqlstate], result[:message]) if result[:kind] == :error

      result
    end

    def add(name, arg_types, result, volatility, cost, batched, block)
      raise ArgumentError, "a host function needs a block" unless block

      vol = VOLATILITY.fetch(volatility) do
        raise ArgumentError, "volatility must be one of #{VOLATILITY.keys.inspect}, got #{volatility.inspect}"
      end
      raise ArgumentError, "cost must be an Integer, got #{cost.inspect}" unless cost.is_a?(Integer)

      types = Array(arg_types).map { |t| type_name(t) }
      fn = HostFunction.new(name.to_s, batched, block)
      @lock.synchronize do
        res = check(Jed::Codec.take(Jed::FFI::ENV_REGISTER.call(
          @native, name.to_s, types.join(","), type_name(result), vol, cost, batched ? 1 : 0,
          fn.closure.to_i, 0
        )))
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

  # A shared native random or clock source (spec/design/ruby.md §5c). Installing one in an
  # {EnvironmentBuilder} shares this object, so controlling it reaches every database whose
  # environment holds it. Internal base class.
  class Source
    # The native source pointer (internal to the gem).
    def native_pointer = @native

    def self.finalizer(addr)
      proc { Jed::FFI::SOURCE_FREE.call(Fiddle::Pointer.new(addr)) }
    end
    private_class_method :finalizer

    private

    def init(native)
      @native = native
      ObjectSpace.define_finalizer(self, Source.send(:finalizer, native.to_i))
    end

    # Microseconds since the Unix epoch from an Integer (already micros) or a Time.
    def micros(value)
      case value
      when Integer then value
      when Time then (value.to_r * 1_000_000).floor
      else raise ArgumentError, "a clock reading is an Integer (µs since the epoch) or a Time, got #{value.class}"
      end
    end

    def integer(value, what)
      raise ArgumentError, "#{what} must be an Integer, got #{value.inspect}" unless value.is_a?(Integer)

      value
    end
  end

  # The deterministic random source (a splitmix64 stream, spec/encoding/prng.toml): `uuidv4`/`uuidv7`
  # draw from it, so a seeded environment generates the same uuids in every run (and every core).
  class SeededRandom < Source
    def initialize(seed)
      init(Jed::FFI::SEEDED_RANDOM_NEW.call(seed_value(seed)))
    end

    # Restart the stream exactly as a fresh source seeded with `seed` would begin. Returns self.
    def reseed(seed)
      Jed::FFI::SEEDED_RANDOM_RESEED.call(@native, seed_value(seed))
      self
    end

    private

    def seed_value(seed)
      integer(seed, "seed")
      raise ArgumentError, "seed must fit in 64 unsigned bits, got #{seed}" unless seed.between?(0, (2**64) - 1)

      seed
    end
  end

  # A clock that reads the instant it was last set to, until it is set or advanced again. Readings
  # are an Integer of microseconds since the Unix epoch, or a Time.
  class ManualClock < Source
    def initialize(reading)
      init(Jed::FFI::MANUAL_CLOCK_NEW.call(micros(reading)))
    end

    # Set the reading. Returns self.
    def set(reading)
      Jed::FFI::MANUAL_CLOCK_SET.call(@native, micros(reading))
      self
    end

    # Move the reading forward by `step` microseconds. Returns self.
    def advance(step)
      Jed::FFI::MANUAL_CLOCK_ADVANCE.call(@native, integer(step, "step"))
      self
    end
  end

  # A clock whose reads return `start`, then `start + step`, `start + 2·step`, … — one step per read,
  # so `clock_timestamp()` is deterministic and distinguishable from the statement-stable `now()`.
  class AdvancingClock < Source
    def initialize(start, step)
      init(Jed::FFI::ADVANCING_CLOCK_NEW.call(micros(start), integer(step, "step")))
    end

    # Restart the sequence: the next read returns `start`, each later read `step` more. Returns self.
    def reset(start, step)
      Jed::FFI::ADVANCING_CLOCK_RESET.call(@native, micros(start), integer(step, "step"))
      self
    end
  end
end
