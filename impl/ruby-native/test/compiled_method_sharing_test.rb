# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# SQL semantics belong to the corpus. These probes cover generated method
# identity, independent kernel data, bounded ownership and concurrent admission.
module NativeCompiledMethodSharingTests
  module_function

  Compiler = JedNative::CompiledExpression
  COLUMNS = [{ name: 'id', type: 'i32' }, { name: 'label', type: 'text' }].freeze
  VALUES = [[1, 'alpha'], [2, nil], [3, 'beta']].freeze

  def assert(value, message) = (raise(message) unless value)

  def engine(params = [])
    current = JedNative::Engine.new
    current.instance_variable_set(:@params, params)
    current.instance_variable_set(:@native_compilation_disabled, true)
    current
  end

  def resolve(body, filter: false)
    current = engine
    ast = JedNative::Statement.new(filter ? "SELECT 1 WHERE #{body}" : "SELECT #{body}").ast
    filter ? current.compile_filter(ast[:where], COLUMNS) : current.compile(ast[:columns].first[:expr], COLUMNS)
  end

  def image(value)
    case value
    when Array then value.map { |item| image(item) }
    when JedNative::CollatedText then [value.class.name, String.new(value), value.collation_label.to_h]
    when String then [value.class.name, String.new(value)]
    else value
    end
  end

  class Row
    attr_reader :ordinal
    def initialize(values, ordinal, events, fault)
      @values, @ordinal, @events, @fault = values, ordinal, events, fault
    end
    def [](index)
      @events << [:read, @ordinal, index]
      raise JedNative::Error.new('XX001', 'deferred sharing fixture fault') if @fault == [@ordinal, index]
      @values[index]
    end
    def length = @values.length
    def each_with_index
      length.times { |index| yield self[index], index }
    end
  end

  module Charges
    attr_accessor :sharing_events
    def charge(amount = 1)
      sharing_events << [:charge, amount, cost]
      super
    end
  end

  def fixture(mode, literal)
    predicate = resolve("id > #{literal}", filter: true)
    expressions = [resolve("id + #{literal}"), resolve('label')]
    kernel = case mode
    when :scalar then Compiler.build(expressions.first, types: {})
    when :projection then Compiler.build_many(expressions, types: {})
    when :arguments then Compiler.prepare_many(expressions, types: {}).materialize
    when :array, :stream
      JedNative::CompiledPipeline.build(predicate, expressions, types: {}, mode: mode)
    when :filter, :memory_filter
      JedNative::CompiledRelation.prepare_filter(predicate, memory: mode == :memory_filter).materialize
    when :keys then JedNative::CompiledRelation.prepare_keys(expressions).materialize
    when :aggregate
      expressions = [resolve("sum(id + #{literal})")]
      JedNative::CompiledAggregate.build(expressions.first)
    end
    assert(kernel, "fixture rejected #{mode}")
    { mode: mode, expressions: expressions, predicate: predicate, kernel: kernel }
  end

  def outcome(fixture, compiled, limit: 0, lifetime: false, fault: nil)
    mode, expressions, predicate = fixture.values_at(:mode, :expressions, :predicate)
    current = engine
    current.singleton_class.prepend(Charges)
    events = current.sharing_events = []
    lifetime ? current.lifetime_max_cost = limit : current.max_cost = limit
    rows = VALUES.each_with_index.map { |values, index| Row.new(values, index, events, fault) }
    account = JedNative::QueryMemoryAccount.new(8192)
    account.reserve(4096)
    owner = JedNative::QueryMemoryAccount::Owner.new(4096)
    current.instance_variable_set(:@query_memory, account) if mode == :memory_filter
    begin
      value = if compiled
        kernel = fixture.fetch(:kernel)
        case mode
        when :scalar, :projection, :arguments then kernel.call(current, rows.first, [])
        when :array, :stream then kernel.call(current, rows, [], [])
        when :memory_filter then kernel.call(current, rows, [], owner, COLUMNS, [true, true])
        else kernel.call(current, rows, [])
        end
      else
        case mode
        when :scalar then current.evaluate(expressions.first, rows.first)
        when :projection, :arguments then expressions.map { |expression| current.evaluate(expression, rows.first) }
        when :keys then rows.map { |row| expressions.map { |expression| current.evaluate(expression, row) } }
        when :aggregate then current.compute_aggregate(expressions.first, rows)
        when :filter, :memory_filter
          rows.select do |row|
            accepted = current.evaluate(predicate, row) == true
            current.query_memory_release(owner, current.query_memory_row_bytes(row, COLUMNS, [true, true])) if mode == :memory_filter && !accepted
            accepted
          end
        when :array, :stream
          rows.each_with_object([]) do |row, output|
            current.charge if mode == :stream
            next unless current.evaluate(predicate, row) == true
            values = expressions.map { |expression| current.evaluate(expression, row) }
            current.charge if mode == :stream
            output << values
          end
        end
      end
      value = value.map(&:ordinal) if %i[filter memory_filter].include?(mode)
      answer = [:value, image(value)]
    rescue JedNative::Error => error
      answer = [:error, error.sqlstate, error.message]
    end
    [answer, current.cost, current.lifetime_cost, events, account.used, owner.bytes]
  end

  def compiler_families
    %i[scalar projection arguments array stream filter memory_filter keys aggregate].each do |mode|
      first, second = [1, 2].map { |literal| fixture(mode, literal) }
      a, b = first.fetch(:kernel), second.fetch(:kernel)
      assert(a.source == b.source && a.class.equal?(b.class), "#{mode} did not share its exact template")
      assert(!a.equal?(b) && !a.pool.equal?(b.pool) && a.pool != b.pool, "#{mode} shared literal data")
      assert(a.class.frozen? && a.frozen? && b.pool.frozen?, "#{mode} shared mutable code or data")
      assert(a.method(:call).owner.equal?(b.method(:call).owner), "#{mode} duplicated its method")
      [first, second, first].each do |item|
        [nil, [0, 0], [2, 1]].each do |fault|
          baseline = outcome(item, false, fault: fault)
          assert(outcome(item, true, fault: fault) == baseline, "#{mode} changed values, labels, reads or memory")
          (1..baseline[1] + 1).each do |limit|
            [false, true].each do |lifetime|
              options = { limit: limit, lifetime: lifetime, fault: fault }
              assert(outcome(item, true, **options) == outcome(item, false, **options), "#{mode} changed a cost/error prefix")
            end
          end
        end
      end
    end
    first = Compiler.build(resolve('id + 1'), types: {})
    changed = Compiler.build(resolve('id - 1'), types: {})
    assert(first.class != changed.class, 'different templates shared a method')

    # Identical Ruby text under different kernel kinds still has different
    # inherited helpers/initialization and must not share a method owner.
    source = "def call(*arguments)\n  @pool.fetch(0)\nend\n"
    families = [Compiler, JedNative::CompiledPipeline, JedNative::CompiledOrdering,
                JedNative::CompiledAggregate, JedNative::CompiledRelation]
    kernels = families.each_with_index.map { |family, index| family::Program.new(source, index + 1, [index]).materialize }
    assert(kernels.map(&:class).uniq.length == families.length, 'kernel superclass was omitted from cache identity')
    kernels.each_with_index do |kernel, index|
      assert(kernel.call == index && kernel.node_count == index + 1, 'shared method lost per-instance metadata')
    end
    ordering = kernels[2]
    another = JedNative::CompiledOrdering::Program.new(source, 7, [91]).materialize
    assert(ordering.class.equal?(another.class) && !ordering.to_proc.equal?(another.to_proc), 'ordering Proc shared a receiver')
    assert(ordering.to_proc.call == 2 && another.to_proc.call == 91, 'ordering Proc captured another kernel pool')
    assert(ordering.to_proc.equal?(ordering.to_proc), 'ordering Proc lost per-kernel reuse')
  end

  def cache_bounds_and_source
    cache = Compiler.const_get(:MethodCache).new
    source = +"def call(*)\n  @pool.fetch(0)\nend\n"
    original = source.dup
    first = cache.fetch(Compiler::Kernel, source, '(sharing fixture)')
    source.replace("def call(*)\n  @pool.fetch(1)\nend\n")
    assert(cache.fetch(Compiler::Kernel, original, '(sharing fixture)').equal?(first), 'cache retained a mutable miss key')
    hit = original.dup
    cache.fetch(Compiler::Kernel, hit, '(sharing fixture)')
    hit.replace('mutated after hit')
    assert(cache.fetch(Compiler::Kernel, original, '(sharing fixture)').equal?(first), 'cache retained a mutable hit key')
    state = cache.instance_variable_get(:@state)
    assert(state.entries.keys.all? { |key| key.frozen? && key.last.frozen? }, 'method keys were mutable')
    begin
      first.class_eval('def call(*) = :changed')
      raise 'shared class accepted method mutation'
    rescue FrozenError
      nil
    end
    before = state.entries.dup
    begin
      cache.fetch(Compiler::Kernel, 'def call(', '(sharing fixture)')
      raise 'invalid fixture source compiled'
    rescue SyntaxError
      assert(state.entries == before, 'failed compilation changed the cache')
    end

    limit = Compiler::MAX_METHOD_CACHE_ENTRIES
    sources = Array.new(limit) { |index| "#{original}# distinct #{index}\n" }
    classes = sources.map { |text| cache.fetch(Compiler::Kernel, text, '(sharing fixture)') }
    assert(state.entries.length == limit, 'global method cache exceeded its entry limit')
    cache.fetch(Compiler::Kernel, sources.first, '(sharing fixture)')
    cache.fetch(Compiler::Kernel, "#{original}# new\n", '(sharing fixture)')
    assert(cache.fetch(Compiler::Kernel, sources.first, '(sharing fixture)').equal?(classes.first), 'method LRU did not refresh a hit')
    assert(!cache.fetch(Compiler::Kernel, sources[1], '(sharing fixture)').equal?(classes[1]), 'method LRU failed to evict the oldest entry')
    assert(classes[1].new(sources[1], 1, [23]).call == 23, 'eviction invalidated a live kernel class')

    capped = original + '#' * (Compiler::MAX_SOURCE_BYTES - original.bytesize)
    at_cap = cache.fetch(Compiler::Kernel, capped, '(sharing fixture)')
    assert(cache.fetch(Compiler::Kernel, capped, '(sharing fixture)').equal?(at_cap), 'exact source bound did not share')
    oversized = capped + '#'
    before = state.entries.dup
    a, b = 2.times.map { cache.fetch(Compiler::Kernel, oversized, '(sharing fixture)') }
    assert(a != b && state.entries == before, 'oversized internal source entered the bounded cache')
    assert(state.entries.keys.sum { |key| key.last.bytesize } <= limit * Compiler::MAX_SOURCE_BYTES, 'method source retention exceeded its bound')
  end

  def concurrent_admission
    cache = Compiler.const_get(:MethodCache).new
    source = "def call(*)\n  @pool.fetch(0)\nend\n"
    ready, start = Queue.new, Queue.new
    threads = 8.times.map do |index|
      Thread.new do
        ready << true
        start.pop
        klass = cache.fetch(Compiler::Kernel, source, '(sharing threads)')
        kernel = klass.new(source, index + 1, [index])
        20.times { assert(kernel.call == index, 'concurrent admission mixed pools') }
        klass
      end
    end
    8.times { ready.pop }
    8.times { start << true }
    assert(threads.map(&:value).uniq.length == 1, 'concurrent misses compiled duplicate method classes')
  end

  def instance_admission
    first = engine
    first.instance_variable_set(:@native_compilation_disabled, false)
    predicate = ->(literal) { resolve("id > #{literal}", filter: true) }
    assert(first.native_compiled_relation_filter(predicate.call(1)).nil?, 'shared class bypassed initial plan admission')
    original = first.native_compiled_relation_filter(predicate.call(1))
    assert(original, 'second use did not admit a relation kernel')
    assert(first.native_compiled_relation_filter(predicate.call(2)).nil?, 'warm method bypassed a new pool admission')
    second = first.native_compiled_relation_filter(predicate.call(2))
    assert(second.class.equal?(original.class) && second.pool != original.pool, 'admitted pools lost method sharing or isolation')
    other = engine
    other.instance_variable_set(:@native_compilation_disabled, false)
    assert(other.native_compiled_relation_filter(predicate.call(1)).nil?, 'method sharing bypassed another engine admission')
    assert(other.native_compiled_relation_filter(predicate.call(1)).class.equal?(original.class), 'another engine duplicated admitted code')
    first.instance_variable_set(:@native_compilation_disabled, true)
    assert(first.native_compiled_relation_filter(predicate.call(1)).nil?, 'shared class bypassed compilation disable')
    first.instance_variable_set(:@native_compilation_disabled, false)
    first.instance_variable_set(:@native_compilation_immediate, true)
    70.times { |index| first.native_compiled_relation_filter(predicate.call(index + 10)) }
    cache = first.instance_variable_get(:@native_compiled_relations)
    assert(cache.length == JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES, 'shared methods changed per-pool LRU bound')
    assert(cache.keys.map(&:last).uniq.length == cache.length && cache.values.map(&:class).uniq == [original.class], 'pool cache collapsed independent values or duplicated code')
    assert(!cache.values.include?(original), 'shared class prevented kernel-instance eviction')
  end

  def fork_reset
    return unless Process.respond_to?(:fork)
    cache = Compiler.const_get(:MethodCache).new
    source = "def call(*)\n  @pool.fetch(0)\nend\n"
    parent_class = cache.fetch(Compiler::Kernel, source, '(sharing fork)')
    parent_state = cache.instance_variable_get(:@state)
    ready, release = Queue.new, Queue.new
    holder = Thread.new { parent_state.mutex.synchronize { ready << true; release.pop } }
    ready.pop
    reader, writer = IO.pipe
    pid = Process.fork do
      reader.close
      begin
        klass = cache.fetch(Compiler::Kernel, source, '(sharing fork)')
        state = cache.instance_variable_get(:@state)
        assert(state.pid == Process.pid && !state.equal?(parent_state), 'child retained parent cache state')
        assert(!klass.equal?(parent_class), 'child admitted against inherited cache')
        assert(parent_class.new(source, 1, [7]).call == 7, 'fork invalidated an existing kernel')
        assert(4.times.map { Thread.new { cache.fetch(Compiler::Kernel, source, '(sharing fork)') } }.map(&:value).all? { |item| item.equal?(klass) }, 'child threads did not converge on new cache')
        writer.write('ok')
        writer.close
        exit! 0
      rescue Exception => error
        writer.write("#{error.class}: #{error.message}")
        writer.close
        exit! 1
      end
    end
    writer.close
    release << true
    holder.value
    assert(IO.select([reader], nil, nil, 10), 'fork child blocked on inherited compilation mutex')
    answer = reader.read
    _, status = Process.wait2(pid)
    pid = nil
    assert(answer == 'ok' && status.success?, "fork sharing failed: #{answer}")
    assert(cache.instance_variable_get(:@state).equal?(parent_state) && cache.fetch(Compiler::Kernel, source, '(sharing fork)').equal?(parent_class), 'child reset changed parent cache')
  ensure
    release << true if holder&.alive?
    holder&.join
    reader&.close unless reader&.closed?
    writer&.close unless writer&.closed?
    if pid
      Process.kill('KILL', pid) rescue nil
      Process.wait(pid) rescue nil
    end
  end

  def detached_references
    current = engine([7])
    expressions = [resolve('id + 71'), resolve('upper(label)')]
    kernel = Compiler.build_many(expressions, types: {})
    assert(kernel.pool.any? { |item| item.is_a?(JedNative::Expression) }, 'retention fixture lost its intact fallback expression')
    row, params = [3, +'mixed'], [7]
    kernel.call(current, row, params)
    [kernel.class, [kernel, current, row, params, expressions, kernel.pool, *expressions].map { |value| WeakRef.new(value) }]
  end

  def lifetime
    klass, references = Thread.new { detached_references }.value
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'method cache retained a kernel, fallback AST, engine, pool or input')
    assert(klass.frozen?, 'retained method class became mutable')
  end

  def warmed_apis(database)
    database.execute('CREATE TABLE sharing_t(id i32 PRIMARY KEY, label text)')
    database.execute("INSERT INTO sharing_t VALUES(1,'alpha'),(2,'beta'),(3,NULL)")
    %i[query execute query_prepared execute_prepared].each do |api|
      readers = [false, true].map do |disabled|
        current = database.session
        current.engine.instance_variable_set(:@native_compilation_disabled, disabled)
        current
      end
      [1, 5, 1].each do |literal|
        sql = "SELECT id + #{literal}, label FROM sharing_t WHERE id >= $1 ORDER BY id"
        statements = readers.map { |current| current.prepare(sql) }
        [1, 2, 1].each do |parameter|
          outcomes = readers.each_with_index.map do |current, index|
            result = api.to_s.end_with?('prepared') ? current.public_send(api, statements[index], parameter) : current.public_send(api, sql, parameter)
            [image(result.rows), result.column_names, result.column_types, result.cost]
          end
          assert(outcomes.first == outcomes.last, "shared class changed #{api} values, types or costs")
        end
      end
      readers.each(&:close)
    end

    reader, writer = database.session, database.session
    statement = reader.prepare('SELECT id + $1, label FROM sharing_t ORDER BY id')
    3.times { reader.query_prepared(statement, 1).rows }
    old = reader.query_prepared(statement, 2)
    writer.execute("UPDATE sharing_t SET label='new' WHERE id=1")
    assert(reader.query_prepared(statement, 7).rows.first == [8, 'new'], 'shared class retained an old publication or binding')
    assert(old.rows.first == [3, 'alpha'], 'shared class lost a captured publication or binding')
    reader.revoke(:select, 'sharing_t')
    begin
      reader.query_prepared(statement, 1).rows
      raise 'shared class bypassed permission invalidation'
    rescue JedNative::Error => error
      assert(error.sqlstate == '42501', 'unexpected permission failure')
    end
    reader.reset_privileges
    writer.execute('DROP TABLE sharing_t')
    writer.execute('CREATE TABLE sharing_t(id i16 PRIMARY KEY, label text)')
    writer.execute("INSERT INTO sharing_t VALUES(4,'replacement')")
    assert(reader.query_prepared(statement, 3).rows == [[7, 'replacement']], 'shared class retained a replaced schema')
    reader.close
    writer.close
  end

  def run
    compiler_families
    cache_bounds_and_source
    concurrent_admission
    instance_admission
    fork_reset
    lifetime
    JedNative.memory { |database| warmed_apis(database) }
    Dir.mktmpdir('jed-method-sharing-') do |directory|
      JedNative.create(File.join(directory, 'sharing.jed'), locking: :exclusive) { |database| warmed_apis(database) }
    end
    puts 'ruby-native method sharing: all compiler families, isolated pools, exact costs/errors, bounded source LRU, threads/fork, lifetime and warm APIs passed'
  end
end

NativeCompiledMethodSharingTests.run
