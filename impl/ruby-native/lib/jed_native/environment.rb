# frozen_string_literal: true

module JedNative
  class WallClock
    def now_micros
      time = Time.now
      time.to_i * 1_000_000 + time.nsec / 1000
    end
  end

  class ManualClock
    def initialize(micros) = (@micros = micros; @lock = Mutex.new)
    def now_micros = @lock.synchronize { @micros }
    def set(micros) = @lock.synchronize { @micros = micros }
    def advance(micros) = @lock.synchronize { @micros += micros }
  end

  class AdvancingClock
    def initialize(start, step) = (@micros = start; @step = step; @lock = Mutex.new)
    def now_micros
      @lock.synchronize do
        value = @micros
        @micros += @step
        value
      end
    end
    def reset(start, step) = @lock.synchronize { @micros, @step = start, step }
  end

  class OSRandom
    def bytes(length) = Random.urandom(length)
  end

  # The deterministic source is opt-in; production defaults draw OS entropy.
  class SeededRandom
    MASK = (1 << 64) - 1
    def initialize(seed) = (@state = seed & MASK; @lock = Mutex.new)
    def reseed(seed) = @lock.synchronize { @state = seed & MASK }
    def bytes(length)
      @lock.synchronize do
        output = +''.b
        while output.bytesize < length
          @state = (@state + 0x9E3779B97F4A7C15) & MASK
          value = @state
          value = ((value ^ (value >> 30)) * 0xBF58476D1CE4E5B9) & MASK
          value = ((value ^ (value >> 27)) * 0x94D049BB133111EB) & MASK
          value ^= value >> 31
          output << [value].pack('Q>')
        end
        output.byteslice(0, length)
      end
    end
  end

  class Environment
    attr_reader :clock, :random
    def initialize(clock: WallClock.new, random: OSRandom.new)
      @clock, @random = clock, random
      freeze
    end
    def with(clock: @clock, random: @random) = self.class.new(clock: clock, random: random)
  end

  module HostInputs
    def select(statement)
      previous_clock, previous_counter = @statement_clock, @uuid_counter
      @statement_clock, @uuid_counter = nil, 0
      super
    ensure
      @statement_clock, @uuid_counter = previous_clock, previous_counter
    end

    def clock_value(name)
      @environment ||= Environment.new
      if name == 'clock_timestamp'
        TimestampValue.new(@environment.clock.now_micros, zoned: true)
      else
        @statement_clock ||= TimestampValue.new(@environment.clock.now_micros, zoned: true)
      end
    end

    def uuid_value(name, shift = nil)
      @environment ||= Environment.new
      if name == 'uuidv7'
        instant = clock_value('now')
        instant = Types.binary('+', instant, shift, type: 'timestamptz') if shift
        milliseconds = instant.micros.div(1000)
        raise Error.new('22008', 'UUID timestamp outside its supported range') unless milliseconds.between?(0, (1 << 48) - 1)
        counter = (@uuid_counter || 0) & 0xfff
        @uuid_counter = (@uuid_counter || 0) + 1
        bytes = [milliseconds].pack('Q>').byteslice(2, 6) + [0x7000 | counter].pack('n') + @environment.random.bytes(8)
      else
        bytes = @environment.random.bytes(16).dup
        bytes.setbyte(6, (bytes.getbyte(6) & 0x0f) | 0x40)
      end
      raise Error.new('58000', 'random source returned the wrong byte count') unless bytes.bytesize == 16
      bytes.setbyte(8, (bytes.getbyte(8) & 0x3f) | 0x80)
      hex = bytes.unpack1('H*')
      UUID.new([hex[0, 8], hex[8, 4], hex[12, 4], hex[16, 4], hex[20, 12]].join('-'))
    end
  end
  Engine.prepend(HostInputs)
end
