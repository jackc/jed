#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only diagnostic for the existing general executor's two independent
# relation phases. Whole-query measurements also include grouping and storage.
require_relative 'compiled_diagnostics'

module CompiledRelationDiagnostics
  COLUMNS = [{ name: 'score', table: 'items', type: 'i32' },
             { name: 'bucket', table: 'items', type: 'i32' },
             { name: 'flag', table: 'items', type: 'boolean' },
             { name: 'label', table: 'items', type: 'text' }].freeze
  MASK = [true, false, true, true].freeze
  JOBS = [
    { name: 'parameter_filter', kind: :filter, sql: 'bucket >= $1::i32', params: [4] },
    { name: 'nullable_filter', kind: :filter, sql: 'flag AND score * 3 + bucket >= $1::i32', params: [100] },
    { name: 'accounted_filter', kind: :filter, sql: 'flag AND score * 3 + bucket >= $1::i32', params: [100], memory: true },
    { name: 'plain_key', kind: :keys, sql: ['bucket'], params: [] },
    { name: 'computed_key', kind: :keys, sql: ['score % 100'], params: [] },
    { name: 'nullable_key_vector', kind: :keys,
      sql: ['bucket % 3', 'CASE WHEN score < $1::i32 THEN NULL ELSE score % 7 END', 'flag'], params: [100] }
  ].freeze
  LOG_MARKER = '(jed compiled relation)'
  module_function

  def now = CompiledExpressionDiagnostics.now
  def jit_stats = CompiledExpressionDiagnostics.jit_stats

  def reset(engine, params, statement_limit: 0, lifetime_limit: 0)
    CompiledExpressionDiagnostics.reset(engine, params, statement_limit)
    engine.lifetime_max_cost = lifetime_limit
  end

  def resolve(engine, job)
    reset(engine, job[:params])
    if job[:kind] == :filter
      engine.compile_filter(JedNative::Parser.new(job[:sql]).parse_expression, COLUMNS)
    else
      job[:sql].map { |sql| engine.compile(JedNative::Parser.new(sql).parse_expression, COLUMNS) }
    end
  end

  def prepare(engine, resolved, job)
    types = engine.instance_variable_get(:@types)
    if job[:kind] == :filter
      JedNative::CompiledRelation.prepare_filter(resolved, types: types, memory: !!job[:memory])
    else
      JedNative::CompiledRelation.prepare_keys(resolved, types: types)
    end
  end

  def fixture(size)
    Array.new(size) do |index|
      [index % 5 == 0 ? nil : ((index * 37) % 997) - 498,
       index % 7 == 0 ? nil : index % 11, [nil, false, true][index % 3],
       index % 3 == 0 ? nil : "row#{index}".freeze].freeze
    end.freeze
  end

  def memory_state(engine, rows, job)
    unless job[:memory]
      engine.instance_variable_set(:@query_memory, nil)
      return
    end
    bytes = rows.sum { |row| engine.query_memory_row_bytes(row, COLUMNS, MASK) }
    account = JedNative::QueryMemoryAccount.new(bytes)
    owner = JedNative::QueryMemoryAccount::Owner.new(bytes)
    raise 'fixture memory reservation failed' unless account.reserve(bytes)
    engine.instance_variable_set(:@query_memory, account)
    { bytes: bytes, account: account, owner: owner }
  end

  def reseed_memory(state)
    return unless state
    state[:account].release(state[:account].used)
    raise 'fixture memory reservation failed' unless state[:account].reserve(state[:bytes])
    state[:owner].bytes = state[:bytes]
  end

  def ordinary(engine, resolved, rows, job, state)
    if job[:kind] == :filter
      if job[:memory]
        rows.select do |row|
          accepted = engine.evaluate(resolved, row) == true
          engine.query_memory_release(state[:owner], engine.query_memory_row_bytes(row, COLUMNS, MASK)) unless accepted
          accepted
        end
      else
        rows.select { |row| engine.evaluate(resolved, row) == true }
      end
    else
      rows.map { |row| resolved.map { |expression| engine.evaluate(expression, row) } }
    end
  end

  def invoke(engine, resolved, kernel, rows, params, job, state)
    if kernel
      kernel.call(engine, rows, params, state && state[:owner], COLUMNS, MASK)
    else
      ordinary(engine, resolved, rows, job, state)
    end
  end

  def outcome(engine, resolved, kernel, rows, params, job, statement_limit: 0, lifetime_limit: 0)
    reset(engine, params, statement_limit: statement_limit, lifetime_limit: lifetime_limit)
    state = memory_state(engine, rows, job)
    result = invoke(engine, resolved, kernel, rows, params, job, state)
    if job[:kind] == :filter
      ordinals = rows.each_with_index.to_h { |row, index| [row.object_id, index] }
      identity = result.map { |row| ordinals.fetch(row.object_id) }
      raise 'filter changed input row order or duplicated a row' unless identity == identity.sort.uniq
    end
    { status: 'value', rows: result, retained_row_ordinals: identity,
      cost: engine.cost, lifetime_cost: engine.lifetime_cost,
      memory_used: state && state[:account].used, owner_bytes: state && state[:owner].bytes }
  rescue JedNative::Error => error
    { status: 'error', sqlstate: error.sqlstate, message: error.message,
      cost: engine.cost, lifetime_cost: engine.lifetime_cost,
      memory_used: state && state[:account].used, owner_bytes: state && state[:owner].bytes }
  ensure
    engine.max_cost = engine.lifetime_max_cost = 0
    engine.instance_variable_set(:@query_memory, nil)
  end

  def validate(engine, resolved, kernel, rows, job)
    inputs = [[], [[nil, nil, nil, nil]],
      [[17, 5, true, 'first'], [nil, 7, true, nil], [-31, 3, false, 'third'],
       [8, 4, nil, 'fourth'], [-17, 9, true, 'fifth'], [0, 4, true, nil]],
      [[-(2**31), 5, false, 'lower'], [2**31 - 1, 9, true, 'upper'], [nil, 0, nil, nil]],
      [[2**31 - 1, 0, nil, 'rejected'], [7, 8, true, 'accepted']],
      [[7, 8, false, 'released'], [2**31 - 1, 9, true, 'overflow']]]
    params = [job[:params]]
    params += [[nil], [-1000], [1000]] unless job[:params].empty?
    cases = inputs.product(params).map do |input, binding|
      left = outcome(engine, resolved, nil, input, binding, job)
      right = outcome(engine, resolved, kernel, input, binding, job)
      raise "#{job[:name]} value/error/cost/memory differs: #{left.inspect} != #{right.inspect}" unless left == right
      boundaries = (1..left[:cost] + 1).map do |limit|
        %i[statement_limit lifetime_limit].to_h do |setting|
          expected = outcome(engine, resolved, nil, input, binding, job, **{ setting => limit })
          actual = outcome(engine, resolved, kernel, input, binding, job, **{ setting => limit })
          raise "#{job[:name]} #{setting}=#{limit} differs" unless expected == actual
          [setting, actual]
        end.merge(limit: limit)
      end
      { rows: input, params: binding, outcome: right, boundaries: boundaries }
    end
    left = outcome(engine, resolved, nil, rows, job[:params], job)
    right = outcome(engine, resolved, kernel, rows, job[:params], job)
    raise "#{job[:name]} full fixture differs" unless left == right
    { cases: cases, input_sha256: Digest::SHA256.hexdigest(JSON.generate(rows)),
      fixture: right.reject { |key, _| %i[rows retained_row_ordinals].include?(key) }.merge(
        rows: right[:rows].length, output_sha256: Digest::SHA256.hexdigest(JSON.generate(right[:rows])),
        retained_row_ordinals_sha256: Digest::SHA256.hexdigest(JSON.generate(right[:retained_row_ordinals]))) }
  end

  def sample(engine, resolved, kernel, rows, job, iterations)
    reset(engine, job[:params])
    state = memory_state(engine, rows, job)
    allocated, gc = GC.stat(:total_allocated_objects), GC.stat(:count)
    checksum = index = 0
    started = now
    while index < iterations
      reseed_memory(state)
      output = invoke(engine, resolved, kernel, rows, job[:params], job, state)
      checksum += output.length
      value = output.first&.first
      checksum += value if value.is_a?(Integer)
      index += 1
    end
    elapsed = now - started
    { elapsed_ns: elapsed, ns_per_relation: elapsed.fdiv(iterations), iterations: iterations,
      checksum: checksum, cost: engine.cost, lifetime_cost: engine.lifetime_cost,
      memory_used: state && state[:account].used, owner_bytes: state && state[:owner].bytes,
      allocations: GC.stat(:total_allocated_objects) - allocated, gc_count: GC.stat(:count) - gc }
  ensure
    engine.instance_variable_set(:@query_memory, nil)
  end

  def generated_log_entries(path)
    path ? File.readlines(path, chomp: true).select { |line| line.include?(LOG_MARKER) } : []
  end

  def proof(kernel, options, log_start)
    result = CompiledExpressionDiagnostics.proof(kernel)
    sequence = RubyVM::InstructionSequence.of(kernel.method(:call))
    result.merge!(iseq_path: sequence.path, first_lineno: sequence.first_lineno)
    if options[:zjit_log]
      entries = generated_log_entries(options[:zjit_log]).drop(log_start)
      location = "#{sequence.label}@#{sequence.path}:"
      result[:zjit_generated_compilations] = entries
      result[:direct_jit_proof] = entries.any? { |line| line.start_with?(location) }
      result[:note] = 'Only this job\'s relation kernel is called between log snapshots; repeat materializations are never invoked.'
    end
    result
  end

  def run_job(job, options)
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_relation_disabled, true)
    resolved = resolve(engine, job)
    rows = fixture(options[:rows])
    log_start = generated_log_entries(options[:zjit_log]).length
    started = now
    program = prepare(engine, resolved, job)
    prepare_ns = now - started
    raise "#{job[:name]} not admitted" unless program
    started = now
    kernel = program.materialize
    materialize_ns = now - started
    reset(engine, job[:params])
    state = memory_state(engine, rows, job)
    started = now
    first = invoke(engine, resolved, kernel, rows, job[:params], job, state)
    first_call_ns = now - started
    result = job.merge(first_prepare_ns: prepare_ns, first_materialize_ns: materialize_ns,
      first_compile_ns: prepare_ns + materialize_ns, first_call_ns: first_call_ns,
      first_result: { rows: first.length, cost: engine.cost, lifetime_cost: engine.lifetime_cost,
        memory_used: state && state[:account].used, owner_bytes: state && state[:owner].bytes },
      source_bytes: program.source.bytesize, node_count: program.node_count, pool_entries: program.pool.length,
      jit_after_first_call: jit_stats)
    result[:validation] = validate(engine, resolved, kernel, rows, job)
    result[:compile_samples] = Array.new(options[:compile_samples]) do
      started = now
      duplicate = prepare(engine, resolved, job)
      emitted = now
      raise 'repeat admission differs' unless duplicate && duplicate.materialize
      { prepare_ns: emitted - started, materialize_ns: now - emitted }
    end
    result[:warmup] = [1, 10, 100, options[:warmup]].uniq.map do |iterations|
      { iterations: iterations, ordinary: sample(engine, resolved, nil, rows, job, iterations),
        compiled: sample(engine, resolved, kernel, rows, job, iterations), jit: jit_stats }
    end
    result[:samples] = Array.new(options[:repetitions]) do |index|
      order = index.even? ? %i[ordinary compiled] : %i[compiled ordinary]
      measurements = order.to_h do |name|
        [name, sample(engine, resolved, name == :compiled ? kernel : nil, rows, job, options[:iterations])]
      end
      %i[checksum cost lifetime_cost memory_used owner_bytes].each do |key|
        raise "timed #{key} differs" unless measurements[:ordinary][key] == measurements[:compiled][key]
      end
      { order: order, measurements: measurements }
    end
    result[:validation_after_timing] = validate(engine, resolved, kernel, rows, job)
    raise 'warmed behavior changed' unless result[:validation] == result[:validation_after_timing]
    result[:jit_after_timing] = jit_stats
    result[:generated_code] = proof(kernel, options, log_start)
    result
  end

  def memory_snapshot
    base = JedNative::CompiledRelation::Kernel
    { rss_bytes: CompiledExpressionDiagnostics.rss_bytes, live_slots: GC.stat(:heap_live_slots), jit: jit_stats,
      live_kernels: ObjectSpace.each_object(base).count,
      anonymous_kernel_classes: ObjectSpace.each_object(Class).count { |type| type.superclass.equal?(base) } }
  end

  def cached_kernel(engine, job)
    resolved = resolve(engine, job)
    if job[:kind] == :filter
      engine.native_compiled_relation_filter(resolved, memory: !!job[:memory])
    else
      engine.native_compiled_relation_keys(resolved)
    end
  end

  def cache_admission
    engine = JedNative::Engine.new
    filter = { kind: :filter, sql: 'score >= 1', params: [] }
    changed = filter.merge(sql: 'score >= 2')
    raise 'first use eagerly compiled' unless cached_kernel(engine, filter).nil?
    kernel = cached_kernel(engine, filter)
    raise 'second use/reuse differs' unless kernel && cached_kernel(engine, filter).equal?(kernel)
    raise 'different literal compiled on first use' unless cached_kernel(engine, changed).nil?
    other = cached_kernel(engine, changed)
    raise 'immutable pool not part of cache key' unless other && !other.equal?(kernel) && other.source == kernel.source && other.pool != kernel.pool
    accounted = filter.merge(memory: true)
    raise 'memory variant shared ordinary code' unless cached_kernel(engine, accounted).nil?
    memory_kernel = cached_kernel(engine, accounted)
    raise 'memory variant was not separately compiled' unless memory_kernel && memory_kernel.source != kernel.source
    text = { kind: :filter, sql: "label >= 'row5'", params: [] }
    mixed_keys = { kind: :keys, sql: ['score % 100', 'label'], params: [] }
    raise 'unsupported text predicate admitted' unless cached_kernel(engine, text).nil?
    raise 'partially unsupported key vector admitted' unless cached_kernel(engine, mixed_keys).nil?
    raise 'scalar pool mutable' unless [kernel, other, memory_kernel].all? do |item|
      item.pool.frozen? && item.pool.all? { |value| value.frozen? && [NilClass, TrueClass, FalseClass, Integer, String].any? { |type| value.is_a?(type) } }
    end
    { first_use_deferred: true, second_use_materialized: true, equal_source_and_pool_reused: true,
      changed_pool_separate: true, memory_variant_separate: true, immutable_scalar_pool: true,
      unsupported_text_predicate_rejected: true, unsupported_key_vector_rejected: true,
      entries: engine.instance_variable_get(:@native_compiled_relations).length }
  end

  def populate_cache(options)
    engine = JedNative::Engine.new
    rows = [[7, 1, true, 'first'], [nil, 0, nil, nil], [-3, 9, false, 'third']].freeze
    options[:churn].times do |index|
      job = if index.even?
        { kind: :filter, sql: "score >= #{index}", params: [] }
      else
        { kind: :keys, sql: ["score + #{index}", 'bucket'], params: [] }
      end
      raise 'new cache entry eagerly compiled' unless cached_kernel(engine, job).nil?
      kernel = cached_kernel(engine, job)
      raise 'second-use compilation failed' unless kernel
      options[:churn_calls].times { kernel.call(engine, rows, []) }
    end
    cache = engine.instance_variable_get(:@native_compiled_relations) || {}
    raise 'relation cache exceeded its bound' unless cache.length == [options[:churn], JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES].min
    GC.start(full_mark: true, immediate_sweep: true)
    memory_snapshot.merge(retained_kernels: cache.length, retained_source_bytes: cache.values.sum(&:source_bytes),
      retained_pool_entries: cache.values.sum { |kernel| kernel.pool.length })
  end

  def churn(options)
    GC.start(full_mark: true, immediate_sweep: true)
    before = memory_snapshot
    started = now
    during = populate_cache(options)
    elapsed = now - started
    after_return = Array.new(3) do |index|
      GC.start(full_mark: true, immediate_sweep: true)
      memory_snapshot.merge(gc_after_return: index + 1)
    end
    { plans_created: options[:churn], calls_per_plan: options[:churn_calls],
      retained_limit: JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES, elapsed_ns: elapsed,
      before: before, while_cache_retained: during, after_helper_return_and_gc: after_return,
      note: 'Uses the actual source/pool LRU and second-use admission. Cache population returns only scalar snapshots before final GC, removing its engine/cache/kernel locals from scope. VM classes and machine code may outlive generated instances.' }
  end

  def main
    options = { rows: 512, warmup: 2000, iterations: 100, repetitions: 5, compile_samples: 30, churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.banner = 'Explicit diagnostic: mise run ruby-native:bounded -- ruby --yjit bench/ruby-native/relation_diagnostics.rb --report PATH'
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
      parser.on('--zjit-log PATH') { |value| options[:zjit_log] = File.expand_path(value) }
      %i[rows warmup iterations repetitions compile_samples churn churn_calls].each do |key|
        parser.on("--#{key.to_s.tr('_', '-')} N", Integer) { |value| options[key] = value }
      end
    end.parse!
    abort 'Specify --report PATH and positive counts; churn may be zero' unless options[:report] && ARGV.empty? &&
      %i[rows warmup iterations repetitions compile_samples churn_calls].all? { |key| options[key].positive? } && options[:churn] >= 0
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    before = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    abort 'Source has no relation compiler' unless defined?(JedNative::CompiledRelation)
    yjit = !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?)
    zjit = !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?)
    jit = yjit ? 'yjit' : zjit ? 'zjit' : 'none'
    abort "Requested #{options[:jit]}, got #{jit}" if options[:jit] && options[:jit] != jit
    results = JOBS.map { |job| run_job(job, options) }
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION,
      executable: RbConfig.ruby, source: source, source_sha256: before,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      helper_sha256: Digest::SHA256.file(File.join(__dir__, 'compiled_diagnostics.rb')).hexdigest,
      options: options, jit: jit, workloads: results, cache_admission: cache_admission, churn: churn(options),
      compiler_limits: { nodes: JedNative::CompiledRelation::MAX_NODES, depth: JedNative::CompiledRelation::MAX_DEPTH,
        pool_entries: JedNative::CompiledRelation::MAX_POOL, source_bytes: JedNative::CompiledRelation::MAX_SOURCE_BYTES,
        relation_cache_entries: JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES },
      notes: ['Isolated general relation filtering and row-major grouping-key projection over existing row arrays. Excludes SQL parsing, scanning, grouping, sorting, output projection and storage.',
        'Controls are the original Array#select predicate evaluation and nested Array#map key evaluation. Filter and key phases remain independent; no interleaving or charge hoisting is benchmarked.',
        'Every small-case statement/lifetime cost boundary, value/error, filter row identity/order, and rejected-row memory release is checked before and after timing.',
        'The finite-memory job reseeds the same account/owner before each invocation in both timed modes. Fixture byte sizing and account construction are outside timing.',
        'GC remains enabled. Timed samples exclude compilation, validation, JIT introspection and explicit GC.',
        'Compilation and first invocation are separate from warm timings. Warmup checkpoints follow validation, so they are not pristine cold-method JIT trajectories. JIT counters are whole-process values.'] }
    if options[:zjit_log]
      snapshot = File.binread(options[:zjit_log])
      report[:zjit_log] = { path: options[:zjit_log], snapshot_bytes: snapshot.bytesize,
        snapshot_sha256: Digest::SHA256.hexdigest(snapshot),
        generated_entries: snapshot.lines(chomp: true).select { |line| line.include?(LOG_MARKER) },
        note: 'Snapshot precedes report serialization; later JIT activity can append entries. Validate this exact byte prefix and use the artifact manifest for the final complete-log hash.' }
    elsif zjit
      report[:zjit_log] = { direct_jit_proof: false,
        note: 'Global counters do not prove generated methods compiled. Use --zjit-log-compiled-iseqs=PATH and --zjit-log PATH.' }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during diagnostic' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: before, jit: jit,
      workloads: results.map { |job| { name: job[:name], generated_code: job[:generated_code] } })
  end
end

CompiledRelationDiagnostics.main if $PROGRAM_NAME == __FILE__
