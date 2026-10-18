# frozen_string_literal: true

require 'tempfile'

module JedNative
  # Scratch is private to one statement. Marshal is used only on bytes written by
  # this process to its exclusively created file, never on database or host input.
  class ScratchFile
    attr_reader :count
    def initialize
      @file = Tempfile.new(['jed-ruby-', '.scratch'])
      @file.binmode
      @count = 0
    rescue SystemCallError, IOError => error
      raise Error.new('58030', error.message)
    end

    def append(value)
      data = Marshal.dump(value)
      @file.seek(0, IO::SEEK_END)
      position = @file.pos
      @file.write([data.bytesize].pack('Q<'))
      @file.write(data)
      @count += 1
      position
    rescue SystemCallError, IOError => error
      raise Error.new('58030', error.message)
    end

    def read(position)
      @file.seek(position)
      header = @file.read(8)
      return nil unless header
      raise Error.new('58030', 'truncated scratch header') unless header.bytesize == 8
      length = header.unpack1('Q<')
      bytes = @file.read(length)
      raise Error.new('58030', 'truncated scratch row') unless bytes && bytes.bytesize == length
      [Marshal.load(bytes), position + 8 + length]
    rescue SystemCallError, IOError => error
      raise Error.new('58030', error.message)
    end

    def each
      return enum_for(:each) unless block_given?
      position = 0
      @count.times do
        row, position = read(position)
        yield row
      end
    end

    def close
      @file&.close!
      @file = nil
    end
  end

  # Fixed partition metadata bounds lookup memory even for a single skewed key.
  # Forward links preserve insertion order; reverse links find the newest state.
  class ScratchBuckets
    PARTITIONS = 4096
    def initialize
      @file = Tempfile.new(['jed-ruby-', '.buckets'])
      @file.binmode
      @heads, @tails = Array.new(PARTITIONS), Array.new(PARTITIONS)
    rescue SystemCallError, IOError => error
      raise Error.new('58030', error.message)
    end

    def append(key, value)
      partition = key.hash % PARTITIONS
      previous = @tails[partition]
      data = Marshal.dump([key, value])
      @file.seek(0, IO::SEEK_END)
      position = @file.pos
      @file.write([previous ? previous + 1 : 0, 0, data.bytesize].pack('Q<3'))
      @file.write(data)
      if previous
        @file.seek(previous + 8)
        @file.write([position + 1].pack('Q<'))
      else
        @heads[partition] = position
      end
      @tails[partition] = position
    rescue SystemCallError, IOError => error
      raise Error.new('58030', error.message)
    end

    def get(key)
      position = @tails[key.hash % PARTITIONS]
      while position
        previous, _, stored_key, value = read(position)
        return [true, value] if stored_key.eql?(key)
        position = previous
      end
      [false, nil]
    end

    def each(key)
      return enum_for(:each, key) unless block_given?
      position = @heads[key.hash % PARTITIONS]
      while position
        _, following, stored_key, value = read(position)
        yield value if stored_key.eql?(key)
        position = following
      end
    end

    def read(position)
      @file.seek(position)
      header = @file.read(24)
      raise Error.new('58030', 'truncated scratch chain') unless header && header.bytesize == 24
      previous, following, length = header.unpack('Q<3')
      data = @file.read(length)
      raise Error.new('58030', 'truncated scratch value') unless data && data.bytesize == length
      key, value = Marshal.load(data)
      [previous.zero? ? nil : previous - 1, following.zero? ? nil : following - 1, key, value]
    rescue SystemCallError, IOError => error
      raise Error.new('58030', error.message)
    end

    def close
      @file&.close!
      @file = @heads = @tails = nil
    end
  end

  class SpillOwner
    attr_reader :resident_bytes
    def initialize(engine)
      @engine = engine
      @budget = engine.work_mem
      @account = engine.instance_variable_get(:@query_memory)
      @charged = @resident_bytes = 0
    end

    def reserve(bytes)
      @resident_bytes += bytes
      if bytes.negative?
        @account&.release(-bytes)
        @charged += bytes
        return true
      end
      accepted = !@account || @account.reserve(bytes)
      @charged += bytes if accepted
      accepted && (@budget.zero? || @resident_bytes <= @budget)
    end

    def release
      @account&.release(@charged)
      @charged = @resident_bytes = 0
    end
  end

  class RowSpool < SpillOwner
    include Enumerable
    attr_reader :length
    def initialize(engine)
      super
      @rows, @length = [], 0
    end

    def push(row)
      unless @file
        unless reserve(LogicalMemory.row_bytes(row))
          @file = ScratchFile.new
          @rows.each { |old| @file.append(old) }
          @rows = nil
          release
        end
      end
      @file ? @file.append(row) : @rows.push(row)
      @length += 1
      self
    end
    alias << push
    def empty? = @length.zero?
    def spilled? = !@file.nil?
    def each(&block)
      return enum_for(:each) unless block
      @file ? @file.each(&block) : @rows.each(&block)
    end
    def close
      release
      @file&.close
      @file = @rows = nil
    end
  end

  class SpillStateMap < SpillOwner
    def initialize(engine)
      super
      @entries = {}
    end
    def get(key) = @disk ? @disk.get(key) : [@entries.key?(key), @entries[key]]
    def put(key, value)
      return @disk.append(key, value) if @disk
      had = @entries.key?(key)
      old = @entries[key]
      delta = value.sum { |item| LogicalMemory.value_bytes(item) }
      delta -= old.sum { |item| LogicalMemory.value_bytes(item) } if had
      delta += MemorySchedule::ENTRY + key.sum { |item| LogicalMemory.value_bytes(item) } unless had
      @entries[key] = value
      unless reserve(delta)
        @disk = ScratchBuckets.new
        @entries.each { |entry_key, entry_value| @disk.append(entry_key, entry_value) }
        @entries = nil
        release
      end
    end
    def insert(key)
      return false if get(key).first
      put(key, [])
      true
    end
    def close
      release
      @disk&.close
      @disk = @entries = nil
    end
  end

  class SpillHashRows < SpillOwner
    def initialize(engine)
      super
      @rows = {}
    end
    def push(key, row)
      return @disk.append(key, row) if @disk
      (@rows[key] ||= []) << row
      unless reserve(MemorySchedule::ENTRY + LogicalMemory.row_bytes(row))
        @disk = ScratchBuckets.new
        @rows.each { |stored_key, rows| rows.each { |stored_row| @disk.append(stored_key, stored_row) } }
        @rows = nil
        release
      end
    end
    def each(key, &block)
      return enum_for(:each, key) unless block
      @disk ? @disk.each(key, &block) : (@rows[key] || []).each(&block)
    end
    def close
      release
      @disk&.close
      @disk = @rows = nil
    end
  end

  class ExternalSorter < SpillOwner
    include Enumerable
    attr_reader :length
    def initialize(engine, measure: nil, &compare)
      super(engine)
      @compare = lambda do |left, right|
        ordering = compare.call(left[0], right[0])
        ordering.zero? ? left[1] <=> right[1] : ordering
      end
      @measure = measure || LogicalMemory.method(:row_bytes)
      @rows, @runs, @length = [], [], 0
    end

    def push(row)
      @rows << [row, @length]
      @length += 1
      flush unless reserve(@measure.call(row))
      self
    end
    alias << push

    def flush
      return if @rows.empty?
      run = ScratchFile.new
      @rows.sort!(&@compare)
      @rows.each { |row| run.append(row) }
      @rows = []
      release
      level = 0
      while @runs[level]
        older = @runs[level]
        @runs[level] = nil
        merged = ScratchFile.new
        merge([older, run]) { |row| merged.append(row) }
        older.close
        run.close
        run = merged
        level += 1
      end
      @runs[level] = run
    rescue Exception
      run&.close
      merged&.close
      older&.close
      raise
    end

    def each
      return enum_for(:each) unless block_given?
      if @runs.compact.empty?
        @rows.sort!(&@compare)
        @rows.each { |row| yield row[0] }
      else
        flush
        merge(@runs.reverse.compact) { |row| yield row[0] }
      end
    end

    def merge(runs)
      heads = runs.map { |run| run.read(0) }
      charges = heads.map { |head| head ? reserve_head(head[0]) : 0 }
      loop do
        index = nil
        heads.each_index do |candidate|
          next unless heads[candidate]
          index = candidate if index.nil? || @compare.call(heads[candidate][0], heads[index][0]).negative?
        end
        break unless index
        row, position = heads[index]
        following = runs[index].read(position)
        charged = following ? reserve_head(following[0]) : 0
        @account&.release(charges[index])
        heads[index], charges[index] = following, charged
        yield row
      end
    ensure
      charges&.each { |bytes| @account&.release(bytes) }
    end

    def reserve_head(row)
      bytes = @measure.call(row[0])
      !@account || @account.reserve(bytes) ? bytes : 0
    end

    def close
      release
      @runs&.compact&.each(&:close)
      @rows = @runs = nil
    end
  end

  module WorkMemory
    DEFAULT_WORK_MEM = 256 * 1024 * 1024
    attr_accessor :work_mem
    def initialize(work_mem: DEFAULT_WORK_MEM, **options)
      @work_mem = work_mem.zero? ? DEFAULT_WORK_MEM : work_mem
      super(**options)
    end
    def spill_available? = @storage && @work_mem.positive?
  end
  Engine.prepend(WorkMemory)
end
