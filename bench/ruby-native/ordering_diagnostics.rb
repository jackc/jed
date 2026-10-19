#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only comparator diagnostic. Run through ruby-native:bounded; whole
# query/storage measurements belong to compare/ordering-plan.json.
require_relative 'compiled_diagnostics'

module CompiledOrderingDiagnostics
  JOBS = [
    { name: 'integer_desc_id_asc', key_types: %w[i32 i32],
      orders: [{ direction: :desc }, { direction: :asc }] },
    { name: 'nullable_integer_boolean_integer', key_types: %w[i32 boolean i32],
      orders: [{ direction: :asc, nulls: :first }, { direction: :desc, nulls: :last },
               { direction: :desc, nulls: :first }] }
  ].freeze
  LOG_MARKER = '(jed compiled ordering)'
  module_function

  def now = CompiledExpressionDiagnostics.now
  def jit_stats = CompiledExpressionDiagnostics.jit_stats

  def keys(job)
    job[:key_types].each_with_index.map { |type, index| JedNative::Expression.new(kind: :column, type: type, value: index) }
  end

  # Exact original SortedSelect comparator, including its stable input ordinal.
  def ordinary_comparator(engine, orders)
    lambda do |left, right|
      comparison = 0
      orders.each_with_index do |order, index|
        comparison = engine.order_compare(left[1][index], right[1][index], order)
        break unless comparison.zero?
      end
      comparison.zero? ? left[2] <=> right[2] : comparison
    end
  end

  def entry(values, ordinal)
    # The payload is deliberately not another key vector. Sorting must only
    # read key_values, then the ordinal when every key ties.
    [{ payload: ordinal }.freeze, values.freeze, ordinal].freeze
  end

  def fixture(job, size)
    Array.new(size) do |index|
      values = if job[:key_types].length == 2
        [(index * 37) % 97, index]
      else
        [index % 4 == 0 ? nil : index % 11, [nil, false, true][index % 3],
         index % 7 == 0 ? nil : (index * 37) % 23]
      end
      entry(values, index)
    end.freeze
  end

  def boundary_entries(job)
    domains = job[:key_types].map do |type|
      type == 'boolean' ? [nil, false, true] : [nil, -(2**31), -1, 0, 1, 2**31 - 1]
    end
    values = domains.shift.product(*domains)
    # Duplicate complete key vectors with different ordinals exercise the
    # tie-break even in the unique-id workload.
    values += [values.first.dup, values.last.dup]
    values.each_with_index.map { |row, index| entry(row, index) }.freeze
  end

  def validate(engine, ordinary, compiled, job, input)
    cases = boundary_entries(job)
    comparisons = []
    cases.each do |left|
      cases.each do |right|
        expected = ordinary.call(left, right)
        actual = compiled.call(left, right)
        raise "#{job[:name]} pair differs: #{left.inspect} / #{right.inspect}" unless actual == expected
        comparisons << actual
      end
    end
    answers = [[], cases, cases.reverse, input].map do |entries|
      expected = entries.sort(&ordinary)
      actual = entries.sort(&compiled)
      raise "#{job[:name]} sorted answer differs" unless actual == expected
      actual.map { |item| item[2] }
    end
    raise 'sorting unexpectedly accrued execution cost' unless engine.cost.zero? && engine.lifetime_cost.zero?
    { entry_cases: cases.length, checked_pairs: comparisons.length,
      comparison_sha256: Digest::SHA256.hexdigest(JSON.generate(comparisons)),
      sorted_ordinals_sha256: Digest::SHA256.hexdigest(JSON.generate(answers)),
      input_sha256: Digest::SHA256.hexdigest(JSON.generate(input)),
      cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  end

  def pair_sample(comparator, pairs, iterations)
    allocated, gc = GC.stat(:total_allocated_objects), GC.stat(:count)
    checksum = index = 0
    started = now
    while index < iterations
      pair = pairs[index % pairs.length]
      checksum += comparator.call(pair[0], pair[1])
      index += 1
    end
    elapsed = now - started
    { elapsed_ns: elapsed, ns_per_comparison: elapsed.fdiv(iterations), iterations: iterations,
      checksum: checksum, allocations: GC.stat(:total_allocated_objects) - allocated,
      gc_count: GC.stat(:count) - gc }
  end

  def sort_sample(comparator, input, iterations)
    allocated, gc = GC.stat(:total_allocated_objects), GC.stat(:count)
    checksum = index = 0
    started = now
    while index < iterations
      sorted = input.sort(&comparator)
      checksum += sorted.first[2] + sorted.last[2]
      index += 1
    end
    elapsed = now - started
    { elapsed_ns: elapsed, ns_per_sort: elapsed.fdiv(iterations), iterations: iterations,
      checksum: checksum, allocations: GC.stat(:total_allocated_objects) - allocated,
      gc_count: GC.stat(:count) - gc }
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
      result[:note] = 'Only this job\'s ordering kernel is invoked between log snapshots; repeat materializations are never called.'
    end
    result
  end

  def run_job(job, options)
    engine = JedNative::Engine.new
    resolved = keys(job)
    ordinary = ordinary_comparator(engine, job[:orders])
    input = fixture(job, options[:rows])
    cases = boundary_entries(job)
    # Exercise first-key decisions, later-key decisions and complete ties in
    # the direct-call timing instead of comparing only disjoint key ranges.
    pairs = cases.each_with_index.flat_map do |item, index|
      [index, index + 1, index + 6, index * 37 + 11].map { |other| [item, cases[other % cases.length]] }
    end
    log_start = generated_log_entries(options[:zjit_log]).length
    started = now
    program = JedNative::CompiledOrdering.prepare(resolved, job[:orders], types: {})
    prepare_ns = now - started
    raise "#{job[:name]} not admitted" unless program
    started = now
    kernel = program.materialize
    materialize_ns = now - started
    raise "#{job[:name]} not materialized" unless kernel
    started = now
    first = kernel.call(*pairs.first)
    first_call_ns = now - started
    result = job.merge(first_prepare_ns: prepare_ns, first_materialize_ns: materialize_ns,
      first_compile_ns: prepare_ns + materialize_ns, first_call_ns: first_call_ns, first_result: first,
      source_bytes: program.source.bytesize, node_count: program.node_count, pool_entries: program.pool.length,
      jit_after_first_call: jit_stats)
    result[:validation] = validate(engine, ordinary, kernel, job, input)
    result[:compile_samples] = Array.new(options[:compile_samples]) do
      started = now
      duplicate = JedNative::CompiledOrdering.prepare(resolved, job[:orders], types: {})
      emitted = now
      raise 'repeat admission differs' unless duplicate && duplicate.materialize
      { prepare_ns: emitted - started, materialize_ns: now - emitted }
    end
    [ordinary, kernel].each do |comparator|
      pair_sample(comparator, pairs, options[:warmup])
      sort_sample(comparator, input, options[:sort_warmup])
    end
    result[:jit_after_warmup] = jit_stats
    # Alternate order between repetitions, retaining every raw measurement.
    result[:samples] = Array.new(options[:repetitions]) do |index|
      order = index.even? ? %i[ordinary compiled] : %i[compiled ordinary]
      samples = order.to_h do |name|
        comparator = name == :ordinary ? ordinary : kernel
        [name, { pairs: pair_sample(comparator, pairs, options[:iterations]),
          full_sort: sort_sample(comparator, input, options[:sort_iterations]) }]
      end
      %i[pairs full_sort].each do |shape|
        raise 'timed checksums differ' unless samples[:ordinary][shape][:checksum] == samples[:compiled][shape][:checksum]
      end
      { order: order, measurements: samples }
    end
    result[:validation_after_timing] = validate(engine, ordinary, kernel, job, input)
    raise 'warmed behavior changed' unless result[:validation] == result[:validation_after_timing]
    result[:jit_after_timing] = jit_stats
    result[:generated_code] = proof(kernel, options, log_start)
    result
  end

  def memory_snapshot
    base = JedNative::CompiledOrdering::Kernel
    { rss_bytes: CompiledExpressionDiagnostics.rss_bytes, live_slots: GC.stat(:heap_live_slots), jit: jit_stats,
      live_kernels: ObjectSpace.each_object(base).count,
      anonymous_kernel_classes: ObjectSpace.each_object(Class).count { |type| type.superclass.equal?(base) } }
  end

  def churn(options)
    retained = []
    limit = 64
    left, right = entry([7, true, 4], 0), entry([7, false, 4], 1)
    resolved = keys(JOBS.last)
    GC.start
    before = memory_snapshot
    started = now
    options[:churn].times do |index|
      orders = JOBS.last[:orders].map.with_index do |order, slot|
        order.merge(direction: (index + slot).even? ? :asc : :desc, nulls: index % 3 == 0 ? :first : :last)
      end
      kernel = JedNative::CompiledOrdering.build(resolved, orders, types: {})
      raise 'churn comparator not admitted' unless kernel
      options[:churn_calls].times { kernel.call(left, right) }
      # Exercise the cached method-to-proc cycle that the actual sort retains.
      [right, left].sort(&kernel)
      retained << kernel
      retained.shift if retained.length > limit
    end
    elapsed = now - started
    GC.start
    during = memory_snapshot.merge(retained_kernels: retained.length,
      retained_source_bytes: retained.sum(&:source_bytes), retained_pool_entries: retained.sum { |kernel| kernel.pool.length })
    retained.clear
    kernel = nil
    GC.start
    { plans_created: options[:churn], calls_per_plan: options[:churn_calls], retained_limit: limit,
      elapsed_ns: elapsed, before: before, after_eviction_and_gc: during, after_release_and_gc: memory_snapshot,
      note: 'Generator lifetime with a bounded diagnostic-owned array, not proof of engine cache eviction. JIT class/code retention is Ruby policy; RSS need not fall after GC.' }
  end

  def main
    options = { rows: 1000, warmup: 10_000, iterations: 100_000, sort_warmup: 50, sort_iterations: 100,
                repetitions: 5, compile_samples: 30, churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.banner = 'Explicit diagnostic: mise run ruby-native:bounded -- ruby --yjit bench/ruby-native/ordering_diagnostics.rb --report PATH'
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
      parser.on('--zjit-log PATH') { |value| options[:zjit_log] = File.expand_path(value) }
      %i[rows warmup iterations sort_warmup sort_iterations repetitions compile_samples churn churn_calls].each do |key|
        parser.on("--#{key.to_s.tr('_', '-')} N", Integer) { |value| options[key] = value }
      end
    end.parse!
    abort 'Specify --report PATH and positive counts; churn may be zero' unless options[:report] && ARGV.empty? &&
      %i[rows warmup iterations sort_warmup sort_iterations repetitions compile_samples churn_calls].all? { |key| options[key].positive? } && options[:churn] >= 0
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    before = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    abort 'Source has no ordering compiler' unless defined?(JedNative::CompiledOrdering)
    yjit = !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?)
    zjit = !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?)
    jit = yjit ? 'yjit' : zjit ? 'zjit' : 'none'
    abort "Requested #{options[:jit]}, got #{jit}" if options[:jit] && options[:jit] != jit
    results = JOBS.map { |job| run_job(job, options) }
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION,
      executable: RbConfig.ruby, source: source, source_sha256: before,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      helper_sha256: Digest::SHA256.file(File.join(__dir__, 'compiled_diagnostics.rb')).hexdigest,
      options: options, jit: jit, workloads: results, churn: churn(options),
      notes: ['Direct comparator calls and full sorts of precomputed keys; excludes SQL parsing, row scanning, key evaluation and projection.',
        'Control is the original SortedSelect lambda. Sort calls use each comparator\'s to_proc, as the executor does.',
        'Validation checks every boundary-entry pair, complete sorted orders, stable ordinal ties and zero added cost before/after timing.',
        'GC remains enabled. Timed samples exclude source compilation, validation, JIT introspection and explicit GC.',
        'Compilation and first invocation are separate from warmed timings; runtime JIT counters are cumulative whole-process values.'] }
    if options[:zjit_log]
      report[:zjit_log] = { path: options[:zjit_log], sha256: Digest::SHA256.file(options[:zjit_log]).hexdigest,
        generated_entries: generated_log_entries(options[:zjit_log]) }
    elsif zjit
      report[:zjit_log] = { direct_jit_proof: false,
        note: 'Global counters do not prove this comparator compiled. Use --zjit-log-compiled-iseqs=PATH and --zjit-log PATH.' }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during diagnostic' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: before, jit: jit,
      workloads: results.map { |job| { name: job[:name], generated_code: job[:generated_code] } })
  end
end

CompiledOrderingDiagnostics.main if $PROGRAM_NAME == __FILE__
