#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only aggregate diagnostic. Run through ruby-native:bounded. The input
# groups already exist; whole-query measurements include grouping and storage.
require_relative 'compiled_diagnostics'

module CompiledAggregateDiagnostics
  COLUMNS = [{ name: 'score', table: 'items', type: 'i32' },
             { name: 'bucket', table: 'items', type: 'i32' },
             { name: 'flag', table: 'items', type: 'boolean' }].freeze
  JOBS = [
    { name: 'count_star', sql: 'count(*)', params: [], groups: 1 },
    { name: 'nullable_integer_sum', sql: 'sum(score)', params: [], groups: 1 },
    { name: 'computed_filtered_sum',
      sql: 'sum(score * 3 + bucket) FILTER (WHERE bucket >= $1::i32 AND flag)', params: [4], groups: 1 },
    { name: 'nullable_boolean_groups', sql: 'bool_and(flag)', params: [], groups: 16 },
    { name: 'nullable_bit_groups', sql: 'bit_xor(score)', params: [], groups: 16 }
  ].freeze
  LOG_MARKER = '(jed compiled aggregate)'
  module_function

  def now = CompiledExpressionDiagnostics.now
  def jit_stats = CompiledExpressionDiagnostics.jit_stats

  def reset(engine, params, statement_limit: 0, lifetime_limit: 0)
    CompiledExpressionDiagnostics.reset(engine, params, statement_limit)
    engine.lifetime_max_cost = lifetime_limit
  end

  def resolve(engine, sql, params)
    reset(engine, params)
    ast = JedNative::Parser.new(sql).parse_expression
    engine.compile(ast, COLUMNS)
  end

  def fixture(job, size)
    groups = Array.new(job[:groups]) { [] }
    size.times do |index|
      row = [index % 5 == 0 ? nil : ((index * 37) % 997) - 498,
             index % 11, [nil, false, true][index % 3]].freeze
      groups[index % groups.length] << row
    end
    groups.each(&:freeze).freeze
  end

  def validation_cases(job)
    cases = [
      { name: 'empty', rows: [], params: job[:params] },
      { name: 'nulls', rows: [[nil, nil, nil], [nil, 4, nil]], params: job[:params] },
      { name: 'mixed', rows: [[17, 5, true], [nil, 7, true], [-31, 3, false],
                             [8, 4, nil], [-17, 9, true], [0, 4, true]], params: job[:params] },
      { name: 'boundaries', rows: [[-(2**31), 5, false], [2**31 - 1, 9, true],
                                  [nil, 0, nil], [-1, 4, true], [0, 4, false]], params: job[:params] }
    ]
    if job[:name] == 'computed_filtered_sum'
      [-1, 4, 100, nil].each do |value|
        cases << { name: "binding_#{value.inspect}", rows: cases[2][:rows], params: [value] }
      end
      # An overflowing argument on a rejected row must not run. Input failures
      # on accepted rows retain their precise expression charge prefix.
      cases << { name: 'rejected_overflow', rows: [[2**31 - 1, 0, false], [7, 8, true]], params: [4] }
      cases << { name: 'accepted_overflow', rows: [[7, 8, true], [2**31 - 1, 9, true]], params: [4] }
    end
    cases
  end

  def outcome(engine, expression, kernel, rows, params, statement_limit: 0, lifetime_limit: 0)
    reset(engine, params, statement_limit: statement_limit, lifetime_limit: lifetime_limit)
    value = kernel ? kernel.call(engine, rows, params) : engine.compute_aggregate(expression, rows)
    { status: 'value', value: value, type: expression.type,
      cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  rescue JedNative::Error => error
    { status: 'error', sqlstate: error.sqlstate, message: error.message,
      cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  ensure
    engine.max_cost = engine.lifetime_max_cost = 0
  end

  def validate(engine, expression, kernel, job, groups)
    results = validation_cases(job).map do |test|
      ordinary = outcome(engine, expression, nil, test[:rows], test[:params])
      compiled = outcome(engine, expression, kernel, test[:rows], test[:params])
      raise "#{job[:name]} #{test[:name]} value/error/cost differs" unless ordinary == compiled
      boundaries = (1..ordinary[:cost] + 1).map do |limit|
        %i[statement_limit lifetime_limit].to_h do |setting|
          options = { setting => limit }
          left = outcome(engine, expression, nil, test[:rows], test[:params], **options)
          right = outcome(engine, expression, kernel, test[:rows], test[:params], **options)
          raise "#{job[:name]} #{test[:name]} #{setting}=#{limit} differs: #{left.inspect} != #{right.inspect}" unless left == right
          [setting, right]
        end.merge(limit: limit)
      end
      { name: test[:name], rows: test[:rows], params: test[:params], outcome: compiled, boundaries: boundaries }
    end
    fixture_results = groups.map do |group|
      left = outcome(engine, expression, nil, group, job[:params])
      right = outcome(engine, expression, kernel, group, job[:params])
      raise "#{job[:name]} fixture result differs" unless left == right
      right
    end
    { cases: results, groups: fixture_results,
      input_sha256: Digest::SHA256.hexdigest(JSON.generate(groups)),
      output_sha256: Digest::SHA256.hexdigest(JSON.generate(fixture_results)) }
  end

  def sample(engine, expression, kernel, groups, params, iterations)
    reset(engine, params)
    allocated, gc = GC.stat(:total_allocated_objects), GC.stat(:count)
    checksum = index = 0
    started = now
    while index < iterations
      group_index = 0
      while group_index < groups.length
        group = groups[group_index]
        value = kernel ? kernel.call(engine, group, params) : engine.compute_aggregate(expression, group)
        checksum += value.is_a?(Integer) ? value : value == true ? 1 : 0
        group_index += 1
      end
      index += 1
    end
    elapsed = now - started
    calls = iterations * groups.length
    { elapsed_ns: elapsed, ns_per_group_vector: elapsed.fdiv(iterations), ns_per_aggregate_call: elapsed.fdiv(calls),
      iterations: iterations, aggregate_calls: calls, checksum: checksum,
      cost: engine.cost, lifetime_cost: engine.lifetime_cost,
      allocations: GC.stat(:total_allocated_objects) - allocated, gc_count: GC.stat(:count) - gc }
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
      result[:note] = 'Only this job\'s aggregate kernel is called between log snapshots; repeat materializations are never invoked.'
    end
    result
  end

  def run_job(job, options)
    engine = JedNative::Engine.new
    # The control is the current generic compute_aggregate, including its prior
    # computed-argument expression tier. Only the new aggregate tier is disabled.
    engine.instance_variable_set(:@native_aggregate_disabled, true)
    engine.instance_variable_set(:@native_compilation_immediate, true)
    expression = resolve(engine, job[:sql], job[:params])
    engine.native_compiled_aggregate_arguments(expression)
    groups = fixture(job, options[:rows])
    types = engine.instance_variable_get(:@types)
    log_start = generated_log_entries(options[:zjit_log]).length
    started = now
    program = JedNative::CompiledAggregate.prepare(expression, types: types)
    prepare_ns = now - started
    raise "#{job[:name]} not admitted" unless program
    started = now
    kernel = program.materialize
    materialize_ns = now - started
    raise "#{job[:name]} not materialized" unless kernel
    reset(engine, job[:params])
    started = now
    first = kernel.call(engine, groups.first, job[:params])
    first_call_ns = now - started
    result = job.merge(first_prepare_ns: prepare_ns, first_materialize_ns: materialize_ns,
      first_compile_ns: prepare_ns + materialize_ns, first_call_ns: first_call_ns,
      first_result: { value: first, cost: engine.cost, lifetime_cost: engine.lifetime_cost },
      source_bytes: program.source.bytesize, node_count: program.node_count, pool_entries: program.pool.length,
      jit_after_first_call: jit_stats,
      control_argument_kernel: !!expression.instance_variable_get(:@native_argument_kernel))
    result[:validation] = validate(engine, expression, kernel, job, groups)
    result[:compile_samples] = Array.new(options[:compile_samples]) do
      started = now
      duplicate = JedNative::CompiledAggregate.prepare(expression, types: types)
      emitted = now
      raise 'repeat admission differs' unless duplicate && duplicate.materialize
      { prepare_ns: emitted - started, materialize_ns: now - emitted }
    end
    result[:warmup] = [1, 10, 100, options[:warmup]].uniq.map do |iterations|
      { iterations: iterations,
        ordinary: sample(engine, expression, nil, groups, job[:params], iterations),
        compiled: sample(engine, expression, kernel, groups, job[:params], iterations),
        jit: jit_stats }
    end
    result[:jit_after_warmup] = jit_stats
    result[:samples] = Array.new(options[:repetitions]) do |index|
      order = index.even? ? %i[ordinary compiled] : %i[compiled ordinary]
      measurements = order.to_h do |name|
        selected = name == :compiled ? kernel : nil
        [name, sample(engine, expression, selected, groups, job[:params], options[:iterations])]
      end
      %i[checksum cost lifetime_cost aggregate_calls].each do |key|
        raise "timed #{key} differs" unless measurements[:ordinary][key] == measurements[:compiled][key]
      end
      { order: order, measurements: measurements }
    end
    result[:validation_after_timing] = validate(engine, expression, kernel, job, groups)
    raise 'warmed behavior changed' unless result[:validation] == result[:validation_after_timing]
    result[:jit_after_timing] = jit_stats
    result[:generated_code] = proof(kernel, options, log_start)
    result
  end

  def memory_snapshot
    base = JedNative::CompiledAggregate::Kernel
    { rss_bytes: CompiledExpressionDiagnostics.rss_bytes, live_slots: GC.stat(:heap_live_slots), jit: jit_stats,
      live_kernels: ObjectSpace.each_object(base).count,
      anonymous_kernel_classes: ObjectSpace.each_object(Class).count { |type| type.superclass.equal?(base) } }
  end

  def cached_kernel(engine, sql)
    expression = resolve(engine, sql, [])
    engine.native_compiled_aggregate(expression)
  end

  def cache_admission
    engine = JedNative::Engine.new
    first = cached_kernel(engine, 'sum(score + 1)')
    second = cached_kernel(engine, 'sum(score + 1)')
    reused = cached_kernel(engine, 'sum(score + 1)')
    changed_first = cached_kernel(engine, 'sum(score + 2)')
    changed_second = cached_kernel(engine, 'sum(score + 2)')
    raise 'aggregate cache did not defer materialization to second use' unless first.nil? && second && second.equal?(reused)
    raise 'aggregate cache ignored its immutable pool' unless changed_first.nil? && changed_second && !second.equal?(changed_second)
    raise 'literal diagnostic did not isolate the pool key' unless second.source == changed_second.source && second.pool != changed_second.pool
    cache = engine.instance_variable_get(:@native_compiled_aggregates)
    raise 'unexpected aggregate cache entries' unless cache.length == 2
    raise 'aggregate pool is mutable' unless [second, changed_second].all? do |kernel|
      kernel.pool.frozen? && kernel.pool.all? { |item| item.frozen? && [NilClass, TrueClass, FalseClass, Integer, String].any? { |type| item.is_a?(type) } }
    end
    { first_use_deferred: true, second_use_materialized: true, equal_source_and_pool_reused: true,
      changed_pool_separate: true, immutable_scalar_pool: true, entries: cache.length }
  end

  def churn(options)
    GC.start
    before = memory_snapshot
    engine = JedNative::Engine.new
    group = [[7, 1, true], [nil, 0, nil], [-3, 9, false]].freeze
    limit = JedNative::CompiledPlans::MAX_AGGREGATE_CACHE_ENTRIES
    started = now
    options[:churn].times do |index|
      sql = "sum(score + #{index})"
      raise 'new cache entry eagerly compiled' unless cached_kernel(engine, sql).nil?
      kernel = cached_kernel(engine, sql)
      raise 'reused cache entry did not compile' unless kernel
      options[:churn_calls].times { kernel.call(engine, group, []) }
    end
    elapsed = now - started
    cache = engine.instance_variable_get(:@native_compiled_aggregates) || {}
    expected = [options[:churn], limit].min
    raise 'aggregate cache exceeded its bound' unless cache.length == expected && cache.values.all?
    GC.start
    during = memory_snapshot.merge(retained_kernels: cache.length,
      retained_source_bytes: cache.values.sum(&:source_bytes), retained_pool_entries: cache.values.sum { |kernel| kernel.pool.length })
    cache = kernel = engine = nil
    GC.start
    { plans_created: options[:churn], calls_per_plan: options[:churn_calls], retained_limit: limit,
      elapsed_ns: elapsed, before: before, after_eviction_and_gc: during, after_release_and_gc: memory_snapshot,
      note: 'Uses the engine\'s actual source/pool LRU and second-use admission. Live generated objects and whole-process JIT counters have different lifetimes; RSS need not fall after GC.' }
  end

  def main
    options = { rows: 512, warmup: 2000, iterations: 100, repetitions: 5, compile_samples: 30, churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.banner = 'Explicit diagnostic: mise run ruby-native:bounded -- ruby --yjit bench/ruby-native/aggregate_diagnostics.rb --report PATH'
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
    abort 'Source has no aggregate compiler' unless defined?(JedNative::CompiledAggregate)
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
      compiler_limits: { nodes: JedNative::CompiledAggregate::MAX_NODES, depth: JedNative::CompiledAggregate::MAX_DEPTH,
        pool_entries: JedNative::CompiledAggregate::MAX_POOL, source_bytes: JedNative::CompiledAggregate::MAX_SOURCE_BYTES,
        aggregate_cache_entries: JedNative::CompiledPlans::MAX_AGGREGATE_CACHE_ENTRIES },
      notes: ['Direct aggregates over pre-existing groups; excludes SQL parsing, scanning, grouping, projection and storage.',
        'The generic control calls current compute_aggregate with only the new aggregate tier disabled. Existing computed argument kernels are warmed before measurement.',
        'Generated call retains separate input evaluation and reduction phases. Values/types/errors/exact costs and every small-case statement/lifetime cost boundary are checked before and after timing.',
        'GC remains enabled. Timed samples exclude compilation, validation, JIT introspection and explicit GC.',
        'Compilation and first invocation are separate from warm timings. Warmup checkpoints follow validation, so they do not represent pristine cold-method JIT trajectories.',
        'Group vectors are pre-partitioned fixtures, not measurements of the GROUP BY operator. JIT counters are cumulative whole-process values.'] }
    if options[:zjit_log]
      report[:zjit_log] = { path: options[:zjit_log], sha256: Digest::SHA256.file(options[:zjit_log]).hexdigest,
        generated_entries: generated_log_entries(options[:zjit_log]) }
    elsif zjit
      report[:zjit_log] = { direct_jit_proof: false,
        note: 'Global counters do not prove the generated aggregate compiled. Use --zjit-log-compiled-iseqs=PATH and --zjit-log PATH.' }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during diagnostic' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: before, jit: jit,
      workloads: results.map { |job| { name: job[:name], generated_code: job[:generated_code] } })
  end
end

CompiledAggregateDiagnostics.main if $PROGRAM_NAME == __FILE__
