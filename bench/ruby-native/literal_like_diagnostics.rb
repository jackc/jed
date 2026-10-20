#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only final-source broader literal LIKE kernels versus the independent evaluator.
# Whole-query source ABBA measurements establish the before/after performance.
require_relative 'text_diagnostics'

module CompiledLiteralLikeDiagnostics
  HELPERS = CompiledTextDiagnostics
  MARKER = '# literal LIKE wildcard specialization'
  JOBS = [
    { name: 'wildcard_underscore_projection', kind: :scalar, sql: "label LIKE 'row_5%'", params: [] },
    { name: 'wildcard_fixed_filter', kind: :filter, sql: "label LIKE 'row___'", params: [] },
    { name: 'wildcard_segments_filter', kind: :filter, sql: "label LIKE 'r%w%5%'", params: [] },
    { name: 'wildcard_not_segments_filter', kind: :filter, sql: "label NOT LIKE 'r%w%5%'", params: [] },
    { name: 'wildcard_accounted_filter', kind: :filter, sql: "flag AND label LIKE 'row_5%'", params: [], memory: true },
    { name: 'wildcard_key_vector', kind: :keys,
      sql: ["CASE WHEN label LIKE 'row__%' THEN score ELSE bucket END", "CASE WHEN label LIKE 'r%w%5%' THEN 1 ELSE 0 END"], params: [] },
    { name: 'simple_prefix_control', kind: :scalar, sql: "label LIKE 'row5%'", params: [], control: true }
  ].freeze
  FALLBACKS = [
    { kind: :filter, sql: "label LIKE $1::text", params: ['row%'] },
    { kind: :scalar, sql: "$1::text LIKE 'row_5%'", params: ['row50'] },
    { kind: :filter, sql: "label ILIKE 'row_5%'", params: [] },
    { kind: :filter, sql: "label LIKE 'row%_5%'", params: [] },
    { kind: :filter, sql: "label LIKE '%a_b%'", params: [] },
    { kind: :filter, sql: "label LIKE 'row\\'", params: [] }
  ].freeze
  module_function

  def memory_snapshot
    cache = JedNative::CompiledExpression.const_get(:METHOD_CACHE).instance_variable_get(:@state).entries
    raise 'global method cache exceeded bound' unless cache.length <= JedNative::CompiledExpression::MAX_METHOD_CACHE_ENTRIES
    HELPERS.memory_snapshot.merge(method_cache_entries: cache.length,
      method_cache_relation_entries: cache.keys.count { |base, _source| base.equal?(JedNative::CompiledRelation::Kernel) },
      method_cache_source_bytes: cache.keys.sum { |_base, source| source.bytesize })
  end

  def emitted_validation(job, measured_hash)
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_compilation_disabled, true)
    resolved = HELPERS.resolve(engine, job)
    program = HELPERS.prepare(engine, resolved, job)
    raise 'LIKE evidence emission rejected' unless program
    source_hash = Digest::SHA256.hexdigest(program.source)
    raise 'evidence differs from measured kernel' if measured_hash && measured_hash != source_hash
    sites = program.source.scan(MARKER).length
    raise 'wildcard admission differs' unless job[:control] ? sites.zero? : sites.positive?
    kernel = program.materialize
    # Include newlines, escaped metacharacters and non-ASCII strings. Keep every
    # exact statement/lifetime budget prefix, including failure metadata.
    rows = [nil, '', 'row_', 'row_5', 'row05', 'row005', 'row125', 'row%5', 'rowé5', 'row😀5', "row\n5", "row5\n", 'érow50', 'row50é', '😀', 'row50'].map do |label|
      [7, 2, true, label]
    end
    inputs = job[:kind] == :scalar ? rows.map { |row| [row] } : [rows]
    cases = inputs.map do |input|
      ordinary = HELPERS.outcome(engine, resolved, nil, input, job[:params], job)
      compiled = HELPERS.outcome(engine, resolved, kernel, input, job[:params], job)
      raise 'LIKE special values differ' unless ordinary == compiled
      boundaries = (1..ordinary.fetch(:cost) + 1).map do |limit|
        %i[statement_limit lifetime_limit].to_h do |setting|
          left = HELPERS.outcome(engine, resolved, nil, input, [], job, **{ setting => limit })
          right = HELPERS.outcome(engine, resolved, kernel, input, [], job, **{ setting => limit })
          raise 'LIKE special-value cost prefix differs' unless left == right
          [setting, right]
        end.merge(limit: limit)
      end
      { input: input, outcome: compiled, boundaries: boundaries }
    end
    { generated_source_sha256: source_hash, wildcard_like_sites: sites, special_cases: cases }
  end

  def fallback_admission
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_compilation_disabled, true)
    rows = [[7, 2, true, 'row50'], [8, 3, true, 'other'], [9, 4, true, nil]]
    FALLBACKS.map do |job|
      resolved = HELPERS.resolve(engine, job)
      raise "unsupported LIKE admitted: #{job[:sql]}" if HELPERS.prepare(engine, resolved, job)
      mutable = +'row%'
      bindings = job[:params].empty? ? [[]] : [nil, 'row%', 'other', 'row50', '😀'].map { |value| [value] }
      outcomes = bindings.map { |params| { params: params, outcome: HELPERS.outcome(engine, resolved, nil, rows, params, job) } }
      mutated = job[:params].empty? ? [] : ['row%', 'other', 'row50'].map do |value|
        mutable.replace(value)
        { binding: mutable.dup, outcome: HELPERS.outcome(engine, resolved, nil, rows, [mutable], job) }
      end
      job.merge(compiler_rejected: true, outcomes: outcomes, mutable_bindings: mutated)
    end
  end

  def cache_admission
    engine = JedNative::Engine.new
    job = { kind: :filter, sql: "label LIKE 'row_5%'", params: [] }
    raise 'first use eagerly compiled' unless HELPERS.cached_kernel(engine, job).nil?
    kernel = HELPERS.cached_kernel(engine, job)
    raise 'second use/reuse differs' unless kernel && HELPERS.cached_kernel(engine, job).equal?(kernel)
    changed = job.merge(sql: "label LIKE 'row_6%'")
    raise 'changed literal compiled on first use' unless HELPERS.cached_kernel(engine, changed).nil?
    other = HELPERS.cached_kernel(engine, changed)
    raise 'LIKE pool missing from key' unless other && !other.equal?(kernel) && other.source == kernel.source && other.pool != kernel.pool
    raise 'literal leaked into source' if kernel.source.include?('row_5') || other.source.include?('row_6')
    raise 'mutable/non-scalar pool' unless [kernel, other].all? do |item|
      item.pool.frozen? && item.pool.all? { |value| value.frozen? && [NilClass, TrueClass, FalseClass, Integer, String].any? { |type| value.instance_of?(type) } }
    end
    oversized = job.merge(sql: "label LIKE '#{'x' * (JedNative::CompiledExpression::MAX_POOL_BYTES + 1)}%'")
    raise 'oversized LIKE pool admitted' unless HELPERS.cached_kernel(engine, oversized).nil?
    engine.instance_variable_set(:@native_text_compilation_disabled, true)
    raise 'text-disabled cache reused LIKE' unless HELPERS.cached_kernel(engine, job).nil?
    engine.instance_variable_set(:@native_text_compilation_disabled, false)
    raise 'LIKE lost after re-enabling' unless HELPERS.cached_kernel(engine, job).equal?(kernel)
    at_bound = job.merge(sql: "label LIKE '#{'a%' * 32}'")
    over_bound = job.merge(sql: "label LIKE '#{'a%' * 33}'")
    raise '64-run pattern unexpectedly rejected' unless HELPERS.prepare(engine, HELPERS.resolve(engine, at_bound), at_bound)
    raise 'over64-run pattern admitted' if HELPERS.prepare(engine, HELPERS.resolve(engine, over_bound), over_bound)
    { first_use_deferred: true, second_use_materialized: true, equal_program_reused: true,
      exact_text_pool_key: true, immutable_scalar_pool: true, text_disabled_rejected: true,
      oversized_text_pool_rejected: true, literal_run_boundary_admitted: true, literal_run_overflow_rejected: true }
  end

  def special_patterns
    patterns = ['é_😀%', 'row\\__%', 'r\\%w%5%', 'é%😀%z', '_%', '_é_', 'a%a', '%%é%%😀%%']
    patterns.map do |pattern|
      engine = JedNative::Engine.new
      engine.instance_variable_set(:@native_compilation_disabled, true)
      job = { kind: :filter, sql: "label LIKE '#{pattern}'", params: [] }
      resolved = HELPERS.resolve(engine, job)
      program = HELPERS.prepare(engine, resolved, job)
      raise "special wildcard pattern rejected: #{pattern}" unless program && program.source.include?(MARKER)
      kernel = program.materialize
      rows = [nil, '', 'a', 'aa', 'é😀z', 'éa😀', 'row_5', 'r%w55'].map { |label| [7, 2, true, label] }
      actual = HELPERS.outcome(engine, resolved, kernel, rows, [], job)
      raise 'Unicode/escape/nonoverlap semantics differ' unless actual == HELPERS.outcome(engine, resolved, nil, rows, [], job)
      # Use a small NULL/non-ASCII input for every exact cost-limit prefix.
      small = rows.values_at(0, 4)
      cost = HELPERS.outcome(engine, resolved, nil, small, [], job).fetch(:cost)
      boundaries = (1..cost + 1).map do |limit|
        %i[statement_limit lifetime_limit].to_h do |setting|
          left = HELPERS.outcome(engine, resolved, nil, small, [], job, **{ setting => limit })
          right = HELPERS.outcome(engine, resolved, kernel, small, [], job, **{ setting => limit })
          raise 'special wildcard budget prefix differs' unless left == right
          [setting, right]
        end.merge(limit: limit)
      end
      { pattern: pattern, output: actual, boundaries: boundaries,
        generated_source_sha256: Digest::SHA256.hexdigest(program.source) }
    end
  end

  def populate_cache(options)
    engine = JedNative::Engine.new
    rows = [[17, 4, true, 'row5']]
    source_hashes = {}
    options[:churn].times do |index|
      job = { kind: :filter, sql: "label LIKE 'literal_#{index.to_s.rjust(6, '0')}%'", params: [] }
      raise 'new cache entry eagerly compiled' unless HELPERS.cached_kernel(engine, job).nil?
      kernel = HELPERS.cached_kernel(engine, job)
      raise 'second use not materialized' unless kernel && kernel.source.include?(MARKER)
      source_hashes[Digest::SHA256.hexdigest(kernel.source)] = true
      options[:churn_calls].times { kernel.call(engine, rows, []) }
    end
    cache = engine.instance_variable_get(:@native_compiled_relations) || {}
    raise 'cache exceeded bound' unless cache.length == [options[:churn], JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES].min
    raise 'literal values changed generated source' unless source_hashes.length <= 1
    raise 'retained pool exceeded bound' unless cache.values.all? do |kernel|
      kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 } <= JedNative::CompiledExpression::MAX_POOL_BYTES
    end
    GC.start(full_mark: true, immediate_sweep: true)
    memory_snapshot.merge(retained_kernels: cache.length, distinct_generated_sources: source_hashes.length,
      retained_source_bytes: cache.values.sum(&:source_bytes), retained_pool_entries: cache.values.sum { |kernel| kernel.pool.length },
      retained_pool_bytes: cache.values.sum { |kernel| kernel.pool.sum { |value| value.is_a?(String) ? value.bytesize : 0 } })
  end

  def population_thread(options)
    worker = Thread.new { populate_cache(options) }
    snapshot = worker.value
    raise 'live object escaped population thread' unless HELPERS.scalar_snapshot?(snapshot)
    worker = nil
    snapshot
  end

  def churn(options)
    GC.start(full_mark: true, immediate_sweep: true)
    before = memory_snapshot
    started = HELPERS.now
    during = population_thread(options)
    elapsed = HELPERS.now - started
    after = Array.new(3) do |index|
      GC.start(full_mark: true, immediate_sweep: true)
      memory_snapshot.merge(collection: index + 1)
    end
    { plans_created: options[:churn], calls_per_plan: options[:churn_calls], retained_limit: 64,
      elapsed_ns: elapsed, before: before, while_cache_retained: during, after_completed_thread_exit_and_gc: after,
      note: 'Actual second-use source/pool LRU, distinct fixed-width literal LIKE patterns. Only scalar snapshots escape a completed thread. The process method cache intentionally retains classes; VM classes/code can additionally outlive kernels or eviction. This is not a whole-JIT memory bound.' }
  end

  def main
    options = { rows: 128, warmup: 2000, iterations: 100, scalar_warmup: 10_000, scalar_iterations: 100_000,
      repetitions: 5, compile_samples: 30, churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
      parser.on('--zjit-log PATH') { |value| options[:zjit_log] = File.expand_path(value) }
      parser.on('--verify-only') { options[:verify_only] = true }
      parser.on('--baseline-admission-only') { options[:baseline_admission_only] = true }
      %i[rows warmup iterations scalar_warmup scalar_iterations repetitions compile_samples churn churn_calls].each do |key|
        parser.on("--#{key.to_s.tr('_', '-')} N", Integer) { |value| options[key] = value }
      end
    end.parse!
    abort 'Specify --report and positive counts; churn may be zero' unless options[:report] && ARGV.empty? &&
      %i[rows warmup iterations scalar_warmup scalar_iterations repetitions compile_samples churn_calls].all? { |key| options[key].positive? } && options[:churn] >= 0
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    before = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    yjit = !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?)
    zjit = !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?)
    jit = yjit ? 'yjit' : zjit ? 'zjit' : 'none'
    abort "Requested #{options[:jit]}, got #{jit}" if options[:jit] && options[:jit] != jit
    if options[:baseline_admission_only]
      results = JOBS.map do |job|
        engine = JedNative::Engine.new
        resolved = HELPERS.resolve(engine, job)
        program = HELPERS.prepare(engine, resolved, job)
        raise 'baseline admission differs' unless !!program == !!job[:control]
        job.merge(compiler_rejected: program.nil?, generated_source_sha256: program && Digest::SHA256.hexdigest(program.source))
      end
    else
      results = JOBS.map { |job| options[:verify_only] ? HELPERS.verify_job(job, options) : HELPERS.run_job(job, options) }
      results.zip(JOBS).each do |result, job|
        result[:wildcard_evidence] = emitted_validation(job, result.dig(:generated_code, :generated_source_sha256))
      end
    end
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
      source: source, source_sha256: before, script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      helpers_sha256: %w[compiled_diagnostics.rb relation_diagnostics.rb text_diagnostics.rb].to_h { |path| [path, Digest::SHA256.file(File.join(__dir__, path)).hexdigest] },
      jit: jit, options: options, workloads: results, fallback_admission: fallback_admission,
      cache_admission: options[:baseline_admission_only] ? nil : cache_admission,
      special_patterns: options[:baseline_admission_only] ? nil : special_patterns,
      churn: options[:verify_only] || options[:baseline_admission_only] ? nil : churn(options),
      compiler_limits: { nodes: JedNative::CompiledExpression::MAX_NODES, depth: JedNative::CompiledExpression::MAX_DEPTH,
        pool_entries: JedNative::CompiledExpression::MAX_POOL, pool_bytes: JedNative::CompiledExpression::MAX_POOL_BYTES,
        source_bytes: JedNative::CompiledExpression::MAX_SOURCE_BYTES, relation_cache_entries: JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES,
        literal_like_runs: defined?(JedNative::CompiledExpression::MAX_LIKE_RUNS) ? JedNative::CompiledExpression::MAX_LIKE_RUNS : nil },
      notes: ['Final-only generated wildcard projections/filter/key phases versus the independent evaluator, plus one existing simple-prefix kernel control. Whole-query source ABBA gives before/after gains; baseline rejects the six wildcard kernels.',
        'Validation preserves values, every collation metadata field, exact costs and all small statement/lifetime failure prefixes, row identity and finite-memory release. Extra special cases include Unicode, newlines and escaped metacharacters.',
        'Actual changing/mutable text parameters are exercised as explicit generic fallback: unlabelled parameter subjects and dynamic patterns are outside static-C literal specialization.',
        'GC remains enabled. Samples alternate ordinary/compiled order. First compilation and invocation precede validation. Repeated compile samples measure emission plus shared-class cache-hit materialization, not fresh Ruby compilation. Warmup checkpoints follow validation and are not pristine cold JIT trajectories.',
        'Marker evidence and extra semantic checks run after all timers and match each timed source hash. Direct YJIT instructions/ZJIT per-job windows establish each generated method compiled.',
        'JSON may be losslessly compacted after execution; every parsed value and budget-prefix record is retained.'] }
    if options[:zjit_log]
      snapshot = File.binread(options[:zjit_log])
      report[:zjit_log] = { path: options[:zjit_log], snapshot_bytes: snapshot.bytesize,
        snapshot_sha256: Digest::SHA256.hexdigest(snapshot), generated_entries: snapshot.lines(chomp: true).select { |line| HELPERS::LOG_PATTERN.match?(line) },
        note: 'Snapshot precedes serialization; audit exact prefix and every job window against the final log.' }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during diagnostic' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: before, jit: jit,
      workloads: results.map { |job| job.slice(:name, :generated_code, :compiler_rejected) })
  end
end

CompiledLiteralLikeDiagnostics.main if $PROGRAM_NAME == __FILE__
