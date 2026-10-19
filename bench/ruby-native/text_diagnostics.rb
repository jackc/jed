#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only C-text expression/loop diagnostic. Whole-query measurements use
# compare/text-plan.json. No task includes this script unless requested.
require_relative 'relation_diagnostics'

module CompiledTextDiagnostics
  COLUMNS = CompiledRelationDiagnostics::COLUMNS
  MASK = CompiledRelationDiagnostics::MASK
  JOBS = [
    { name: 'text_parameter_projection', kind: :scalar, sql: 'label >= $1::text', params: ['row5'] },
    { name: 'text_identity_projection', kind: :scalar, sql: '$1::text', params: ['row5'] },
    { name: 'text_equality_filter', kind: :filter, sql: 'label = $1::text', params: ['row500'] },
    { name: 'nullable_text_filter', kind: :filter, sql: 'flag AND label >= $1::text', params: ['row5'] },
    { name: 'accounted_text_filter', kind: :filter, sql: 'flag AND label >= $1::text', params: ['row5'], memory: true },
    { name: 'text_predicate_key_vector', kind: :keys,
      sql: ['CASE WHEN label >= $1::text THEN score ELSE bucket END', "CASE WHEN label = '' THEN 1 ELSE 0 END"], params: ['row5'] }
  ].freeze
  LOG_PATTERN = /\(jed compiled (?:expression|relation)\)/
  module_function

  def now = CompiledExpressionDiagnostics.now
  def jit_stats = CompiledExpressionDiagnostics.jit_stats
  def reset(engine, params, **limits) = CompiledRelationDiagnostics.reset(engine, params, **limits)

  def resolve(engine, job)
    reset(engine, job[:params])
    sql = job[:sql]
    if job[:kind] == :keys
      sql.map { |part| engine.compile(JedNative::Parser.new(part).parse_expression, COLUMNS) }
    else
      ast = JedNative::Parser.new(sql).parse_expression
      job[:kind] == :filter ? engine.compile_filter(ast, COLUMNS) : engine.compile(ast, COLUMNS)
    end
  end

  def prepare(engine, resolved, job, text: true)
    types = engine.instance_variable_get(:@types)
    case job[:kind]
    when :scalar then JedNative::CompiledExpression.prepare_many([resolved], types: types, text: text)
    when :filter then JedNative::CompiledRelation.prepare_filter(resolved, types: types, memory: !!job[:memory], text: text)
    when :keys then JedNative::CompiledRelation.prepare_keys(resolved, types: types, text: text)
    end
  end

  def fixture(size)
    Array.new(size) do |index|
      label = case index % 13
      when 0 then nil
      when 1 then ''
      when 2 then 'é'
      when 3 then '豈'
      when 4 then '😀'
      else "row#{index}"
      end
      [index % 5 == 0 ? nil : ((index * 37) % 997) - 498,
       index % 7 == 0 ? nil : index % 11, [nil, false, true][index % 3], label&.freeze].freeze
    end.freeze
  end

  def label_image(label)
    return nil unless label
    label.to_h.transform_values { |value| value.respond_to?(:sort_key) ? value.class.name : value }
  end

  # String equality ignores CollatedText metadata. Preserve the wrapper class
  # and every label field in validation, without retaining its source objects.
  def value_image(value)
    case value
    when Array then value.map { |item| value_image(item) }
    when JedNative::CollatedText
      { ruby_class: value.class.name, text: value.to_s, collation: label_image(value.collation_label) }
    when String then { ruby_class: value.class.name, text: value }
    else value
    end
  end

  def expression_metadata(resolved)
    (resolved.is_a?(Array) ? resolved : [resolved]).map do |expression|
      { type: expression.type, collation: label_image(expression.instance_variable_get(:@native_collation)) }
    end
  end

  def memory_state(engine, rows, job) = CompiledRelationDiagnostics.memory_state(engine, rows, job)
  def reseed_memory(state) = CompiledRelationDiagnostics.reseed_memory(state)

  def invoke(engine, resolved, kernel, rows, params, job, state)
    if job[:kind] == :scalar
      kernel ? kernel.call(engine, rows.first, params) : [engine.evaluate(resolved, rows.first)]
    elsif kernel
      kernel.call(engine, rows, params, state && state[:owner], COLUMNS, MASK)
    else
      CompiledRelationDiagnostics.ordinary(engine, resolved, rows, job, state)
    end
  end

  def outcome(engine, resolved, kernel, rows, params, job, **limits)
    reset(engine, params, **limits)
    state = memory_state(engine, rows, job)
    result = invoke(engine, resolved, kernel, rows, params, job, state)
    if job[:kind] == :filter
      ordinals = rows.each_with_index.to_h { |row, index| [row.object_id, index] }
      identity = result.map { |row| ordinals.fetch(row.object_id) }
      raise 'filter changed input row identity/order' unless identity == identity.sort.uniq
    end
    { status: 'value', value: value_image(result), metadata: expression_metadata(resolved),
      retained_row_ordinals: identity, cost: engine.cost, lifetime_cost: engine.lifetime_cost,
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
    inputs = [
      [[nil, nil, nil, nil]],
      [[7, 2, true, 'row5'], [-8, 3, false, 'row50'], [nil, 4, nil, 'row4']],
      [[7, 2, true, ''], [8, 3, true, 'é'], [9, 4, true, '豈'], [10, 5, true, '😀']],
      [[7, 2, true, "O'Brien"], [8, 3, true, "line\nbreak"], [9, 4, true, 'a' * 33]]
    ]
    inputs.unshift([]) unless job[:kind] == :scalar
    if job[:kind] == :scalar
      inputs = inputs.flat_map { |input| input.map { |row| [row] } }
    end
    bindings = [nil, '', 'row5', 'row50', 'é', '豈', '😀', 'a' * 33].map { |value| [value] }
    cases = inputs.product(bindings).map do |input, params|
      ordinary = outcome(engine, resolved, nil, input, params, job)
      compiled = outcome(engine, resolved, kernel, input, params, job)
      raise "#{job[:name]} value/metadata/cost differs: #{ordinary.inspect} != #{compiled.inspect}" unless ordinary == compiled
      boundaries = (1..ordinary[:cost] + 1).map do |limit|
        %i[statement_limit lifetime_limit].to_h do |setting|
          left = outcome(engine, resolved, nil, input, params, job, **{ setting => limit })
          right = outcome(engine, resolved, kernel, input, params, job, **{ setting => limit })
          raise "#{job[:name]} #{setting}=#{limit} differs" unless left == right
          [setting, right]
        end.merge(limit: limit)
      end
      { input: input, params: params, outcome: compiled, boundaries: boundaries }
    end
    # The same parameter object may change between calls. No captured parameter
    # string or cached comparison result may survive into its next invocation.
    mutable = +'row5'
    changing = ['row5', 'row1', '😀'].map do |text|
      mutable.replace(text)
      ordinary = outcome(engine, resolved, nil, rows, [mutable], job)
      compiled = outcome(engine, resolved, kernel, rows, [mutable], job)
      raise 'mutated text binding differs' unless ordinary == compiled
      { binding: mutable.dup, output_sha256: Digest::SHA256.hexdigest(JSON.generate(compiled)) }
    end
    ordinary = outcome(engine, resolved, nil, rows, job[:params], job)
    compiled = outcome(engine, resolved, kernel, rows, job[:params], job)
    raise 'full fixture differs' unless ordinary == compiled
    { cases: cases, mutable_bindings: changing, input_sha256: Digest::SHA256.hexdigest(JSON.generate(rows)),
      fixture: compiled.reject { |key, _| %i[value retained_row_ordinals].include?(key) }.merge(
        output_sha256: Digest::SHA256.hexdigest(JSON.generate(compiled[:value])),
        retained_row_ordinals_sha256: Digest::SHA256.hexdigest(JSON.generate(compiled[:retained_row_ordinals]))) }
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
      index += 1
    end
    elapsed = now - started
    { elapsed_ns: elapsed, ns_per_call: elapsed.fdiv(iterations), iterations: iterations,
      checksum: checksum, cost: engine.cost, lifetime_cost: engine.lifetime_cost,
      memory_used: state && state[:account].used, owner_bytes: state && state[:owner].bytes,
      allocations: GC.stat(:total_allocated_objects) - allocated, gc_count: GC.stat(:count) - gc }
  ensure
    engine.instance_variable_set(:@query_memory, nil)
  end

  def generated_log_entries(path)
    path ? File.readlines(path, chomp: true).select { |line| LOG_PATTERN.match?(line) } : []
  end

  def proof(kernel, options, log_start)
    result = CompiledExpressionDiagnostics.proof(kernel)
    sequence = RubyVM::InstructionSequence.of(kernel.method(:call))
    result.merge!(iseq_path: sequence.path, first_lineno: sequence.first_lineno)
    if options[:zjit_log]
      entries = generated_log_entries(options[:zjit_log]).drop(log_start)
      result[:zjit_generated_compilations] = entries
      result[:direct_jit_proof] = entries.any? { |line| line.start_with?("#{sequence.label}@#{sequence.path}:") }
      result[:note] = 'Only this job\'s kernel is invoked between log snapshots; repeat materializations are never invoked.'
    end
    result
  end

  def run_job(job, options)
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_compilation_disabled, true)
    resolved = resolve(engine, job)
    rows = job[:kind] == :scalar ? [[73, 4, true, 'row50']] : fixture(options[:rows])
    raise 'text-disabled compiler admitted text job' if prepare(engine, resolved, job, text: false)
    log_start = generated_log_entries(options[:zjit_log]).length
    started = now
    program = prepare(engine, resolved, job)
    emitted = now
    raise "#{job[:name]} not admitted" unless program
    kernel = program.materialize
    materialized = now
    raise 'copied text pool exceeded its bound' if kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 } > JedNative::CompiledExpression::MAX_POOL_BYTES
    reset(engine, job[:params])
    state = memory_state(engine, rows, job)
    started_call = now
    first = invoke(engine, resolved, kernel, rows, job[:params], job, state)
    first_call_ns = now - started_call
    result = job.merge(first_prepare_ns: emitted - started, first_materialize_ns: materialized - emitted,
      first_compile_ns: materialized - started, first_call_ns: first_call_ns,
      first_result: { value_sha256: Digest::SHA256.hexdigest(JSON.generate(value_image(first))), cost: engine.cost,
        lifetime_cost: engine.lifetime_cost, memory_used: state && state[:account].used, owner_bytes: state && state[:owner].bytes },
      source_bytes: kernel.source_bytes, node_count: kernel.node_count, pool_entries: kernel.pool.length,
      pool_bytes: kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 },
      jit_after_first_call: jit_stats)
    result[:validation] = validate(engine, resolved, kernel, rows, job)
    result[:compile_samples] = Array.new(options[:compile_samples]) do
      started = now
      duplicate = prepare(engine, resolved, job)
      emitted = now
      raise 'repeat admission differs' unless duplicate && duplicate.materialize
      { prepare_ns: emitted - started, materialize_ns: now - emitted }
    end
    warmup = options[job[:kind] == :scalar ? :scalar_warmup : :warmup]
    iterations = options[job[:kind] == :scalar ? :scalar_iterations : :iterations]
    result[:warmup] = [1, 10, 100, warmup].uniq.map do |count|
      { iterations: count, ordinary: sample(engine, resolved, nil, rows, job, count),
        compiled: sample(engine, resolved, kernel, rows, job, count), jit: jit_stats }
    end
    result[:samples] = Array.new(options[:repetitions]) do |index|
      order = index.even? ? %i[ordinary compiled] : %i[compiled ordinary]
      measurements = order.to_h do |kind|
        [kind, sample(engine, resolved, kind == :compiled ? kernel : nil, rows, job, iterations)]
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

  def verify_job(job, options)
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_compilation_disabled, true)
    resolved = resolve(engine, job)
    raise 'text-disabled compiler admitted text job' if prepare(engine, resolved, job, text: false)
    program = prepare(engine, resolved, job)
    raise "#{job[:name]} not admitted" unless program
    kernel = program.materialize
    raise 'copied text pool exceeded its bound' if kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 } > JedNative::CompiledExpression::MAX_POOL_BYTES
    rows = job[:kind] == :scalar ? [[73, 4, true, 'row50']] : fixture(options[:rows])
    job.merge(validation: validate(engine, resolved, kernel, rows, job),
      source_bytes: kernel.source_bytes, node_count: kernel.node_count, pool_entries: kernel.pool.length,
      pool_bytes: kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 })
  end

  def cached_kernel(engine, job)
    resolved = resolve(engine, job)
    job[:kind] == :filter ? engine.native_compiled_relation_filter(resolved) : engine.native_compiled_relation_keys(resolved)
  end

  def cache_admission
    engine = JedNative::Engine.new
    job = { kind: :filter, sql: "label >= 'row5'", params: [] }
    raise 'first use eagerly compiled' unless cached_kernel(engine, job).nil?
    kernel = cached_kernel(engine, job)
    raise 'second use/reuse differs' unless kernel && cached_kernel(engine, job).equal?(kernel)
    changed = job.merge(sql: "label >= 'row6'")
    raise 'changed literal compiled on first use' unless cached_kernel(engine, changed).nil?
    other = cached_kernel(engine, changed)
    raise 'text literal pool not in key' unless other && !other.equal?(kernel) && other.source == kernel.source && other.pool != kernel.pool
    raise 'text leaked into source' if kernel.source.include?('row5') || other.source.include?('row6')
    raise 'mutable/non-scalar pool' unless [kernel, other].all? do |item|
      item.pool.frozen? && item.pool.all? { |value| value.frozen? && [NilClass, TrueClass, FalseClass, Integer, String].any? { |type| value.instance_of?(type) } }
    end
    unsupported = ["label LIKE 'row%'", "lower(label) >= 'row5'", "label || 'x' >= 'row5'"]
    unsupported.each do |sql|
      raise "unsupported predicate admitted: #{sql}" unless cached_kernel(engine, job.merge(sql: sql)).nil?
    end
    oversized = "label >= '#{'x' * (JedNative::CompiledExpression::MAX_POOL_BYTES + 1)}'"
    raise 'oversized text pool admitted' unless cached_kernel(engine, job.merge(sql: oversized)).nil?
    engine.instance_variable_set(:@native_text_compilation_disabled, true)
    raise 'text-disabled cache reused text kernel' unless cached_kernel(engine, job).nil?
    engine.instance_variable_set(:@native_text_compilation_disabled, false)
    raise 'text kernel lost after re-enabling' unless cached_kernel(engine, job).equal?(kernel)
    { first_use_deferred: true, second_use_materialized: true, equal_program_reused: true,
      exact_text_pool_key: true, immutable_scalar_pool: true, text_disabled_rejected: true,
      oversized_text_pool_rejected: true, unsupported_predicates: unsupported }
  end

  def memory_snapshot
    bases = [JedNative::CompiledExpression::Kernel, JedNative::CompiledRelation::Kernel]
    { rss_bytes: CompiledExpressionDiagnostics.rss_bytes, live_slots: GC.stat(:heap_live_slots), jit: jit_stats,
      live_kernels: ObjectSpace.each_object(JedNative::CompiledExpression::Kernel).count,
      anonymous_kernel_classes: ObjectSpace.each_object(Class).count { |type| bases.include?(type.superclass) } }
  end

  def populate_cache(options)
    engine = JedNative::Engine.new
    rows = [[17, 4, true, 'row5']]
    options[:churn].times do |index|
      job = { kind: :filter, sql: "label >= 'literal#{index}'", params: [] }
      raise 'new cache entry eagerly compiled' unless cached_kernel(engine, job).nil?
      kernel = cached_kernel(engine, job)
      raise 'second use not materialized' unless kernel
      options[:churn_calls].times { kernel.call(engine, rows, []) }
    end
    cache = engine.instance_variable_get(:@native_compiled_relations) || {}
    raise 'cache exceeded its bound' unless cache.length == [options[:churn], JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES].min
    raise 'retained pool exceeded its bound' unless cache.values.all? do |kernel|
      kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 } <= JedNative::CompiledExpression::MAX_POOL_BYTES
    end
    GC.start(full_mark: true, immediate_sweep: true)
    memory_snapshot.merge(retained_kernels: cache.length, retained_source_bytes: cache.values.sum(&:source_bytes),
      retained_pool_entries: cache.values.sum { |kernel| kernel.pool.length },
      retained_pool_bytes: cache.values.sum { |kernel| kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 } })
  end

  def scalar_snapshot?(value)
    case value
    when Hash then value.all? { |key, item| scalar_snapshot?(key) && scalar_snapshot?(item) }
    when Array then value.all? { |item| scalar_snapshot?(item) }
    when NilClass, TrueClass, FalseClass, Numeric, String, Symbol then true
    else false
    end
  end

  def population_thread(options)
    worker = Thread.new { populate_cache(options) }
    snapshot = worker.value
    raise 'live object escaped population thread' unless scalar_snapshot?(snapshot)
    worker = nil
    snapshot
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
    { plans_created: options[:churn], calls_per_plan: options[:churn_calls],
      retained_limit: JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES, elapsed_ns: elapsed,
      before: before, while_cache_retained: during, after_completed_thread_exit_and_gc: after,
      note: 'Actual source/pool LRU and second-use admission. Only verified scalar snapshots escape a completed thread; its reference is released before GC. VM classes and code can outlive kernel instances; this is not an overall JIT memory bound.' }
  end

  def main
    options = { rows: 512, warmup: 2000, iterations: 100, scalar_warmup: 10_000, scalar_iterations: 100_000,
      repetitions: 5, compile_samples: 30, churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.banner = 'Explicit diagnostic: mise run ruby-native:bounded -- ruby --yjit bench/ruby-native/text_diagnostics.rb --report PATH'
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
      parser.on('--zjit-log PATH') { |value| options[:zjit_log] = File.expand_path(value) }
      parser.on('--verify-only') { options[:verify_only] = true }
      %i[rows warmup iterations scalar_warmup scalar_iterations repetitions compile_samples churn churn_calls].each do |key|
        parser.on("--#{key.to_s.tr('_', '-')} N", Integer) { |value| options[key] = value }
      end
    end.parse!
    abort 'Specify --report PATH and positive counts; churn may be zero' unless options[:report] && ARGV.empty? &&
      %i[rows warmup iterations scalar_warmup scalar_iterations repetitions compile_samples churn_calls].all? { |key| options[key].positive? } && options[:churn] >= 0
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    before = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    yjit = !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?)
    zjit = !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?)
    jit = yjit ? 'yjit' : zjit ? 'zjit' : 'none'
    abort "Requested #{options[:jit]}, got #{jit}" if options[:jit] && options[:jit] != jit
    results = JOBS.map { |job| options[:verify_only] ? verify_job(job, options) : run_job(job, options) }
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
      source: source, source_sha256: before, script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      helpers_sha256: %w[compiled_diagnostics.rb relation_diagnostics.rb].to_h { |path| [path, Digest::SHA256.file(File.join(__dir__, path)).hexdigest] },
      jit: jit, options: options, workloads: results, cache_admission: cache_admission,
      churn: options[:verify_only] ? nil : churn(options),
      compiler_limits: { nodes: JedNative::CompiledExpression::MAX_NODES, depth: JedNative::CompiledExpression::MAX_DEPTH,
        pool_entries: JedNative::CompiledExpression::MAX_POOL, pool_bytes: JedNative::CompiledExpression::MAX_POOL_BYTES,
        source_bytes: JedNative::CompiledExpression::MAX_SOURCE_BYTES,
        relation_cache_entries: JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES },
      notes: ['Isolated C-text projection, filter and key-vector calls; not full-query performance. Generic controls call the existing evaluator, including collation and scalar work.',
        'Projection calls return one-element arrays in both modes. Relation jobs use the existing select/map control. Both modes reseed identical finite-memory state before each invocation.',
        'Validation includes wrapper class and every collation-label field, real changing text bindings, NULL/empty/prefix/Unicode values, all small statement/lifetime cost prefixes, and original filtered row identities.',
        'GC stays enabled. Samples alternate ordinary/compiled order and exclude validation, compilation and JIT introspection.',
        'Compilation and first invocation precede validation; warmup checkpoints follow validation and are not pristine cold-method JIT trajectories. JIT statistics are whole-process counters.'] }
    if options[:zjit_log]
      snapshot = File.binread(options[:zjit_log])
      report[:zjit_log] = { path: options[:zjit_log], snapshot_bytes: snapshot.bytesize,
        snapshot_sha256: Digest::SHA256.hexdigest(snapshot), generated_entries: snapshot.lines(chomp: true).select { |line| LOG_PATTERN.match?(line) },
        note: 'Snapshot precedes serialization. Audit this exact byte prefix and all per-job windows against the final log; later JIT activity can append entries.' }
    elsif zjit
      report[:zjit_log] = { direct_jit_proof: false, note: 'Use both --zjit-log-compiled-iseqs=PATH and --zjit-log PATH for direct generated-method proof.' }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during diagnostic' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: before, jit: jit,
      workloads: results.map { |job| { name: job[:name], generated_code: job[:generated_code] } })
  end
end

CompiledTextDiagnostics.main if $PROGRAM_NAME == __FILE__
