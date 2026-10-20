#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only method sharing diagnostic. Run each shape in a fresh bounded
# process. No cache mutation/reset and no dependency on private compiler emitters.
require 'fileutils'
require_relative 'text_diagnostics'
require_relative 'pipeline_diagnostics'
require_relative 'ordering_diagnostics'
require_relative 'aggregate_diagnostics'

module CompiledSharingDiagnostics
  COMMON = CompiledExpressionDiagnostics
  TEXT = CompiledTextDiagnostics
  COLUMNS = TEXT::COLUMNS
  LOG_PATTERN = /\(jed compiled (?:expression|pipeline|ordering|aggregate|relation)\)/
  FAMILIES = %w[scalar pipeline_array pipeline_stream ordering aggregate relation_filter relation_keys].freeze
  module_function

  def now = COMMON.now
  def sharing? = JedNative::CompiledExpression.const_defined?(:METHOD_CACHE, false)

  def method_cache
    return { available: false, entries: 0 } unless sharing?
    state = JedNative::CompiledExpression.const_get(:METHOD_CACHE).instance_variable_get(:@state)
    entries = state.entries.map do |(base, source), klass|
      raise 'invalid cached method entry' unless source.frozen? && klass.frozen? && klass.superclass.equal?(base)
      { family: base.name, source_sha256: Digest::SHA256.hexdigest(source), source_bytes: source.bytesize }
    end
    raise 'process method cache exceeded bound' unless entries.length <= JedNative::CompiledExpression::MAX_METHOD_CACHE_ENTRIES
    { available: true, entries: entries.length, source_bytes: entries.sum { |item| item[:source_bytes] }, keys: entries }
  end

  def memory_snapshot
    base = JedNative::CompiledExpression::Kernel
    { rss_bytes: COMMON.rss_bytes, live_slots: GC.stat(:heap_live_slots), jit: COMMON.jit_stats,
      live_kernels: ObjectSpace.each_object(base).count,
      anonymous_kernel_classes: ObjectSpace.each_object(Class).count { |type| type.name.nil? && type < base },
      method_cache: method_cache }
  end

  def job(name, variant = 0)
    case name
    when 'scalar' then { name: name, sql: "abs($1::i64) + #{7 + variant}", params: [-123] }
    when 'pipeline_array', 'pipeline_stream'
      { name: name, mode: name.end_with?('array') ? :array : :stream, predicate: 'bucket >= $1::i32',
        projections: ['score', "score::i64 * #{3 + variant} + bucket::i64"], params: [2] }
    when 'ordering'
      { name: name, key_types: %w[i32 i32], orders: [{ direction: :desc }, { direction: :asc }], params: [] }
    when 'aggregate'
      { name: name, sql: "sum(score * #{3 + variant}) FILTER (WHERE bucket >= $1::i32)", params: [2] }
    when 'relation_filter'
      { name: name, kind: :filter, sql: "label LIKE 'row#{(5 + variant).to_s.rjust(3, '0')}%'", params: [] }
    when 'relation_keys'
      { name: name, kind: :keys,
        sql: ["CASE WHEN score >= #{20 + variant} THEN bucket ELSE #{3 + variant} END", 'score % 7'], params: [] }
    end
  end

  def resolve(engine, item)
    COMMON.reset(engine, item[:params])
    parse = ->(sql) { engine.compile(JedNative::Parser.new(sql).parse_expression, COLUMNS) }
    case item[:name]
    when 'pipeline_array', 'pipeline_stream'
      { predicate: engine.compile_filter(JedNative::Parser.new(item[:predicate]).parse_expression, COLUMNS),
        projections: item[:projections].map(&parse), types: engine.instance_variable_get(:@types) }
    when 'ordering' then CompiledOrderingDiagnostics.keys(item)
    when 'relation_filter', 'relation_keys' then TEXT.resolve(engine, item)
    else parse.call(item[:sql])
    end
  end

  def build(engine, resolved, item)
    types = engine.instance_variable_get(:@types)
    case item[:name]
    when 'scalar' then JedNative::CompiledExpression.build(resolved, types: types)
    when 'pipeline_array', 'pipeline_stream' then CompiledPipelineDiagnostics.build(resolved, item)
    when 'ordering' then JedNative::CompiledOrdering.prepare(resolved, item[:orders], types: types)&.materialize
    when 'aggregate' then JedNative::CompiledAggregate.prepare(resolved, types: types)&.materialize
    else TEXT.prepare(engine, resolved, item)&.materialize
    end
  end

  def program_for(kernel, item)
    family = case item[:name]
    when 'scalar' then JedNative::CompiledExpression
    when 'pipeline_array', 'pipeline_stream' then JedNative::CompiledPipeline
    when 'ordering' then JedNative::CompiledOrdering
    when 'aggregate' then JedNative::CompiledAggregate
    else JedNative::CompiledRelation
    end
    family::Program.new(kernel.source, kernel.node_count, kernel.pool)
  end

  def fixture(item, size)
    return CompiledOrderingDiagnostics.fixture(item, size) if item[:name] == 'ordering'
    Array.new(size) { |index| [(index * 37) % 997, index % 11, index.even?, "row#{index.to_s.rjust(3, '0')}".freeze].freeze }.freeze
  end

  def invoke(engine, resolved, kernel, rows, item)
    params = item[:params]
    case item[:name]
    when 'scalar' then kernel ? kernel.call(engine, rows.first, params) : engine.evaluate(resolved, rows.first)
    when 'pipeline_array', 'pipeline_stream'
      CompiledPipelineDiagnostics.invoke(engine, resolved, kernel, rows, params, [], item)
    when 'ordering'
      comparator = kernel ? kernel.to_proc : CompiledOrderingDiagnostics.ordinary_comparator(engine, item[:orders])
      rows.sort(&comparator).map { |entry| entry[2] }
    when 'aggregate' then kernel ? kernel.call(engine, rows, params) : engine.compute_aggregate(resolved, rows)
    else kernel ? kernel.call(engine, rows, params) : CompiledRelationDiagnostics.ordinary(engine, resolved, rows, item, nil)
    end
  end

  def outcome(engine, resolved, kernel, rows, item, statement_limit: 0, lifetime_limit: 0)
    COMMON.reset(engine, item[:params], statement_limit)
    engine.lifetime_max_cost = lifetime_limit
    value = invoke(engine, resolved, kernel, rows, item)
    ordinals = value.map { |row| rows.index { |original| original.equal?(row) } } if item[:kind] == :filter
    raise 'filter lost original row identity' if ordinals && ordinals.any?(&:nil?)
    expressions = resolved.is_a?(Array) ? resolved : resolved.is_a?(Hash) ? resolved[:projections] : [resolved]
    { status: 'value', value: TEXT.value_image(value), metadata: TEXT.expression_metadata(expressions),
      retained_ordinals: ordinals, cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  rescue JedNative::Error => error
    { status: 'error', sqlstate: error.sqlstate, message: error.message, cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  ensure
    engine.max_cost = engine.lifetime_max_cost = 0
  end

  def validate(engine, resolved, kernel, rows, item)
    inputs = if item[:name] == 'ordering'
      [[], CompiledOrderingDiagnostics.boundary_entries(item), rows]
    else
      [[], [[nil, nil, nil, nil]], [[7, 3, true, 'row005'], [-8, 0, false, 'é'], [nil, 4, nil, '']], rows]
    end
    inputs.map.with_index do |input, index|
      ordinary = outcome(engine, resolved, nil, input, item)
      compiled = outcome(engine, resolved, kernel, input, item)
      raise "#{item[:name]} semantic mismatch: #{ordinary.inspect} != #{compiled.inspect}" unless ordinary == compiled
      # Keep every small-input charge prefix, but avoid duplicating full fixtures
      # for thousands of prefixes. The full fixture is checked exactly above.
      boundaries = index == inputs.length - 1 ? [] : (1..ordinary[:cost] + 1).map do |limit|
        %i[statement_limit lifetime_limit].to_h do |setting|
          left = outcome(engine, resolved, nil, input, item, **{ setting => limit })
          right = outcome(engine, resolved, kernel, input, item, **{ setting => limit })
          raise "#{item[:name]} budget prefix differs" unless left == right
          [setting, right]
        end.merge(limit: limit)
      end
      { input: input, outcome: compiled, boundaries: boundaries }
    end
  end

  def sample(engine, resolved, kernel, rows, item, count)
    COMMON.reset(engine, item[:params])
    allocated, gc = GC.stat(:total_allocated_objects), GC.stat(:count)
    started = now
    count.times { invoke(engine, resolved, kernel, rows, item) }
    elapsed = now - started
    { elapsed_ns: elapsed, ns_per_call: elapsed.fdiv(count), calls: count, cost: engine.cost,
      allocations: GC.stat(:total_allocated_objects) - allocated, gc_count: GC.stat(:count) - gc }
  end

  def log_entries(path)
    path ? File.readlines(path, chomp: true).select { |line| LOG_PATTERN.match?(line) } : []
  end

  def proof(kernel, item, options, log_start)
    data = CompiledPipelineDiagnostics.proof(kernel)
    sequences = data[:instruction_sequences]
    if options[:zjit_log]
      entries = log_entries(options[:zjit_log]).drop(log_start)
      sequences.each do |sequence|
        sequence[:direct_jit_proof] = entries.any? { |line| line.start_with?("#{sequence[:label]}@#{sequence[:path]}:") }
      end
      data[:zjit_entries] = entries
      data[:zjit_log_start] = log_start
      data[:zjit_log_end] = log_start + entries.length
    end
    hot = sequences.find { |sequence| sequence[:label] == (item[:name] == 'pipeline_stream' ? 'block in call' : 'call') }
    data[:direct_jit_proof] = !!hot[:direct_jit_proof]
    data
  end

  def run_family(name, options)
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_compilation_disabled, true)
    item = job(name)
    resolved = resolve(engine, item)
    rows = fixture(item, options[:rows])
    before_cache = method_cache
    log_start = log_entries(options[:zjit_log]).length
    started = now
    donor = build(engine, resolved, item)
    cold_build_ns = now - started
    raise "#{name} not admitted" unless donor
    raise 'cold source already cached' if before_cache[:keys]&.any? { |key| key[:source_sha256] == Digest::SHA256.hexdigest(donor.source) }
    COMMON.reset(engine, item[:params])
    started = now
    first = invoke(engine, resolved, donor, rows, item)
    first_call_ns = now - started
    result = { name: name, specification: item, cold_build_ns: cold_build_ns, first_call_ns: first_call_ns,
      first_call: { value: TEXT.value_image(first), cost: engine.cost },
      source_bytes: donor.source_bytes, node_count: donor.node_count, pool_entries: donor.pool.length,
      generated_source_sha256: Digest::SHA256.hexdigest(donor.source), cache_before_build: before_cache }
    result[:validation] = validate(engine, resolved, donor, rows, item)
    # The donor's compiled method is proved before any new-pool recipient exists.
    result[:warmup] = (options[:verify_only] ? [] : [1, 10, 100, 1000]).map do |count|
      sample(engine, resolved, donor, rows, item, count).merge(jit: COMMON.jit_stats)
    end
    result[:donor_proof] = proof(donor, item, options, log_start)
    raise "#{name} donor generated method did not JIT" if !options[:verify_only] && options[:jit] != 'none' && !result[:donor_proof][:direct_jit_proof]
    other_item = job(name, 1)
    other_resolved = resolve(engine, other_item)
    started = now
    recipient = build(engine, other_resolved, other_item)
    result[:recipient_build_ns] = now - started
    raise 'new literal pool changed source/admission' unless recipient && recipient.source == donor.source
    identities = { distinct_instances: !donor.equal?(recipient), same_class: donor.class.equal?(recipient.class),
      same_method_owner: donor.method(:call).owner.equal?(recipient.method(:call).owner),
      same_unbound_method: donor.method(:call).unbind == recipient.method(:call).unbind,
      different_pool: donor.pool != recipient.pool, immutable_pool: recipient.pool.frozen?,
      ordering_distinct_procs: name == 'ordering' ? !donor.to_proc.equal?(recipient.to_proc) : nil }
    raise 'method identity did not match source state' unless identities.values_at(:same_class, :same_method_owner, :same_unbound_method).all? { |value| value == sharing? }
    raise 'pool did not vary' unless name == 'ordering' || identities[:different_pool]
    result[:reuse] = identities
    # A fresh receiver with the same method inherits donor machine code. ZJIT's
    # evidence is donor log proof plus identical method, not lack of later logs.
    recipient_start = log_entries(options[:zjit_log]).length
    result[:recipient_before_first_call] = proof(recipient, other_item, options, recipient_start)
    result[:recipient_before_first_call][:inherited_jit_proof] = sharing? && result[:donor_proof][:direct_jit_proof] && identities[:same_unbound_method]
    COMMON.reset(engine, other_item[:params])
    started = now
    invoke(engine, other_resolved, recipient, rows, other_item)
    result[:recipient_first_call_ns] = now - started
    result[:recipient_validation] = validate(engine, other_resolved, recipient, rows, other_item)
    return result.merge(verified: true) if options[:verify_only]
    program = program_for(donor, item)
    result[:materialize_samples] = Array.new(options[:compile_samples]) do
      allocated = GC.stat(:total_allocated_objects)
      started = now
      fresh = program.materialize
      elapsed = now - started
      { elapsed_ns: elapsed, allocations: GC.stat(:total_allocated_objects) - allocated,
        same_class: fresh.class.equal?(donor.class), distinct_instance: !fresh.equal?(donor) }
    end
    result[:new_pool_build_samples] = Array.new(options[:compile_samples]) do |index|
      varied = job(name, index + 2)
      resolved_variant = resolve(engine, varied)
      allocated = GC.stat(:total_allocated_objects)
      started = now
      fresh = build(engine, resolved_variant, varied)
      elapsed = now - started
      allocations = GC.stat(:total_allocated_objects) - allocated
      raise 'pool diversity changed source' unless fresh && fresh.source == donor.source
      ordinary = outcome(engine, resolved_variant, nil, rows, varied)
      actual = outcome(engine, resolved_variant, fresh, rows, varied)
      raise 'new pool values/metadata/cost differ' unless ordinary == actual
      { elapsed_ns: elapsed, allocations: allocations,
        same_class: fresh.class.equal?(donor.class), pool_sha256: Digest::SHA256.hexdigest(JSON.generate(fresh.pool)),
        validated_outcome_sha256: Digest::SHA256.hexdigest(JSON.generate(actual)) }
    end
    sample(engine, resolved, donor, rows, item, options[:warmup])
    sample(engine, other_resolved, recipient, rows, other_item, options[:warmup])
    count = name == 'scalar' ? options[:iterations] * 100 : options[:iterations]
    result[:samples] = Array.new(options[:repetitions]) { sample(engine, resolved, donor, rows, item, count) }
    result[:recipient_samples] = Array.new(options[:repetitions]) { sample(engine, other_resolved, recipient, rows, other_item, count) }
    result[:recipient_proof] = proof(recipient, other_item, options, recipient_start)
    result[:validation_after_timing] = validate(engine, resolved, donor, rows, item)
    raise 'donor changed after new pools' unless result[:validation] == result[:validation_after_timing]
    result[:cache_after] = method_cache
    result
  end

  def churn_job(shape, index)
    return { kind: :filter, sql: "label LIKE 'literal#{index.to_s.rjust(6, '0')}%'", params: [] } if shape == 'pools'
    expression = 'score::i64'
    8.times { |bit| expression = "(#{expression} #{index[bit].zero? ? '+' : '-'} bucket::i64)" }
    { kind: :keys, sql: [expression], params: [] }
  end

  def populate(options)
    engine = JedNative::Engine.new
    rows = [[17, 4, true, 'literal000000']]
    sources, classes, build_samples, validations = {}, {}, [], []
    first = nil
    options[:churn].times do |index|
      item = churn_job(options[:shape], index)
      raise 'first admission unexpectedly materialized' if TEXT.cached_kernel(engine, item)
      started = now
      kernel = TEXT.cached_kernel(engine, item)
      elapsed = now - started
      raise 'second admission rejected' unless kernel
      first ||= kernel
      sources[Digest::SHA256.hexdigest(kernel.source)] = true
      classes[kernel.class.object_id] = true
      resolved = TEXT.resolve(engine, item)
      ordinary = TEXT.outcome(engine, resolved, nil, rows, [], item)
      actual = TEXT.outcome(engine, resolved, kernel, rows, [], item)
      raise 'churn variant values/metadata/cost differ' unless ordinary == actual
      validations << { specification: item, outcome: actual }
      options[:churn_calls].times { kernel.call(engine, rows, []) }
      build_samples << elapsed
    end
    cache = engine.instance_variable_get(:@native_compiled_relations)
    raise 'wrong instance cache bound' unless cache.length == [64, options[:churn]].min
    expected_sources = options[:shape] == 'pools' ? 1 : options[:churn]
    raise 'churn source diversity differs' unless sources.length == expected_sources
    retained_before_revisit = method_cache
    first_job = churn_job(options[:shape], 0)
    first_resolved = TEXT.resolve(engine, first_job)
    ordinary = TEXT.outcome(engine, first_resolved, nil, rows, [], first_job)
    old_result = TEXT.outcome(engine, first_resolved, first, rows, [], first_job)
    raise 'retained evicted kernel changed' unless ordinary == old_result
    raise 'evicted pool unexpectedly retained' if TEXT.cached_kernel(engine, first_job)
    revisited = TEXT.cached_kernel(engine, first_job)
    raise 'evicted pool not readmitted' unless revisited
    raise 'revisit result changed' unless TEXT.outcome(engine, first_resolved, revisited, rows, [], first_job) == ordinary
    expected_class_reuse = sharing? && options[:shape] == 'pools'
    raise 'LRU revisit method identity differs' unless first.class.equal?(revisited.class) == expected_class_reuse
    GC.start(full_mark: true, immediate_sweep: true)
    memory_snapshot.merge(instance_cache_entries: cache.length, distinct_sources: sources.length,
      observed_class_ids: classes.length, second_admission_samples_ns: build_samples,
      validated_variants: validations,
      cache_before_revisit: retained_before_revisit,
      revisit: { fresh_instance: !first.equal?(revisited), same_class: first.class.equal?(revisited.class),
        old_kernel_still_callable: true, outcome: ordinary })
  end

  def population_thread(options)
    thread = Thread.new { populate(options) }
    value = thread.value
    raise 'live execution context escaped thread' unless TEXT.scalar_snapshot?(value)
    thread = nil
    value
  end

  def churn(options)
    GC.start(full_mark: true, immediate_sweep: true)
    before = memory_snapshot
    started = now
    during = population_thread(options)
    elapsed = now - started
    after = Array.new(3) do |index|
      GC.start(full_mark: true, immediate_sweep: true)
      memory_snapshot.merge(collection: index + 1)
    end
    { shape: options[:shape], plans_created: options[:churn], calls_per_plan: options[:churn_calls],
      elapsed_ns: elapsed, before: before, while_engine_retained: during, after_thread_exit_and_gc: after,
      note: 'Actual engine second-use LRU. Completed thread releases stack roots. Global method cache intentionally retains classes; VM code/classes can survive cache eviction. Kernel-instance and method-cache bounds are not a whole-JIT memory bound.' }
  end

  def main
    options = { rows: 128, warmup: 2000, iterations: 100, repetitions: 5, compile_samples: 30, churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
      parser.on('--expected-sharing STATE', %w[before final]) { |value| options[:expected] = value }
      parser.on('--shape NAME', %w[families pools sources]) { |value| options[:shape] = value }
      parser.on('--zjit-log PATH') { |value| options[:zjit_log] = File.expand_path(value) }
      parser.on('--verify-only') { options[:verify_only] = true }
    end.parse!
    abort 'Specify report, jit, shape and expected-sharing' unless ARGV.empty? && options.values_at(:report, :jit, :shape, :expected).all?
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    before = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    jit = defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled? ? 'yjit' : defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled? ? 'zjit' : 'none'
    raise 'wrong JIT runtime' unless jit == options[:jit]
    raise 'unexpected method-sharing state' unless sharing? == (options[:expected] == 'final')
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
      source: source, source_sha256: before, script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      helpers_sha256: %w[source.rb compiled_diagnostics.rb relation_diagnostics.rb text_diagnostics.rb pipeline_diagnostics.rb ordering_diagnostics.rb aggregate_diagnostics.rb].to_h { |file| [file, Digest::SHA256.file(File.join(__dir__, file)).hexdigest] },
      jit: jit, options: options, method_sharing: sharing?,
      compiler_limits: { method_cache_entries: sharing? ? JedNative::CompiledExpression::MAX_METHOD_CACHE_ENTRIES : 0,
        source_bytes: JedNative::CompiledExpression::MAX_SOURCE_BYTES, pool_bytes: JedNative::CompiledExpression::MAX_POOL_BYTES,
        instance_cache_entries: JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES },
      notes: ['Cold build includes template emission and materialization, after resolution; source was not in the process cache. Warm materialization samples reuse an immutable Program and do not include resolution/emission.',
        'Each source variant and all generic controls preserve exact values, metadata and costs. GC stays enabled. First-call timings precede semantic validation; later warmup trajectories follow validation.',
        'Whole-query source ABBA is authoritative for application throughput. Direct runs are single fresh processes, not ABBA; method identity plus donor generated-code proof establishes JIT reuse.',
        'Scalar versus row-loop call units differ. Ordering has no literal pool, and its per-instance comparator Proc must remain distinct.'] }
    if options[:shape] == 'families'
      report[:workloads] = FAMILIES.map { |name| run_family(name, options) }
    else
      report[:churn] = churn(options)
    end
    if options[:zjit_log]
      log = File.binread(options[:zjit_log])
      report[:zjit_log] = { path: options[:zjit_log], snapshot_bytes: log.bytesize,
        snapshot_sha256: Digest::SHA256.hexdigest(log), generated_entries: log_entries(options[:zjit_log]) }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during run' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], shape: options[:shape], jit: jit, source_sha256: before)
  end
end

CompiledSharingDiagnostics.main if $PROGRAM_NAME == __FILE__
