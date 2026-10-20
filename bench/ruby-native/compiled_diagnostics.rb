#!/usr/bin/env ruby
# frozen_string_literal: true

require 'fileutils'

# Explicit opt-in diagnostic. Run through ruby-native:bounded; this is not a
# whole-query or cross-core benchmark. Engine#evaluate stays the semantic control.
require 'json'
require 'optparse'
require 'rbconfig'
require 'time'
require 'objspace'
require_relative 'source'

module CompiledExpressionDiagnostics
  CLOCK = Process::CLOCK_MONOTONIC
  COLUMNS = [{ name: 'score', table: 'items', type: 'i32' },
             { name: 'bucket', table: 'items', type: 'i32' }].freeze
  JOBS = [
    { name: 'integer_scalar', sql: 'abs($1::i64) * 3 + 7', row: [], params: [-123],
      cases: [[[], [-123]], [[], [0]], [[], [nil]], [[], [-(2**63)]]] },
    { name: 'row_arithmetic', sql: '(score::i64 * 3 + bucket::i64) / (bucket::i64 + 1)',
      row: [9876, 4], params: [], cases: [[[9876, 4], []], [[-9876, 4], []], [[nil, 4], []], [[1, -1], []]] },
    { name: 'conditional', sql: 'CASE WHEN score >= $1::i32 THEN score::i64 * 3 ELSE -score::i64 END',
      row: [9876, 4], params: [5000], cases: [[[9876, 4], [5000]], [[4, 1], [5000]], [[nil, 1], [5000]]] },
    { name: 'null_folds', sql: 'coalesce(nullif($1::i64, 17), 0) + greatest($1::i64, 4)',
      row: [], params: [123], cases: [[[], [123]], [[], [17]], [[], [nil]]] },
    { name: 'predicate', sql: 'score >= $1::i32 AND bucket <> 3', row: [9876, 4], params: [5000],
      cases: [[[9876, 4], [5000]], [[9876, 3], [5000]], [[nil, 4], [5000]], [[9876, nil], [5000]]] }
  ].freeze

  module_function

  def now = Process.clock_gettime(CLOCK, :nanosecond)

  def jit_stats
    values = if defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?
      RubyVM::YJIT.runtime_stats || {}
    elsif defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?
      RubyVM::ZJIT.stats || {}
    else
      {}
    end
    values.select { |key, _| key.to_s.match?(/\A(?:compiled_|compile_time|inline_code|outlined_code|code_region|freed_code|jit_frame_region|zjit_alloc|total_mem)/) }
  end

  def rss_bytes
    File.read('/proc/self/status')[/^VmRSS:\s+(\d+)\s+kB$/, 1].to_i * 1024
  end

  def memory_snapshot
    base = JedNative::CompiledExpression::Kernel
    { rss_bytes: rss_bytes, live_slots: GC.stat(:heap_live_slots), jit: jit_stats,
      live_kernels: ObjectSpace.each_object(base).count,
      anonymous_kernel_classes: ObjectSpace.each_object(Class).count { |type| type.superclass.equal?(base) } }
  end

  def reset(engine, params, limit = 0)
    engine.instance_variable_set(:@params, params)
    engine.instance_variable_set(:@cost, 0)
    engine.instance_variable_set(:@lifetime_cost, 0)
    engine.instance_variable_set(:@scalar_bytes, 0)
    engine.max_cost = limit
  end

  def outcome(engine, expression, kernel, row, params, limit: 0)
    reset(engine, params, limit)
    value = kernel ? kernel.call(engine, row, params) : engine.evaluate(expression, row)
    { status: 'value', value: value, cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  rescue JedNative::Error => error
    { status: 'error', sqlstate: error.sqlstate, message: error.message,
      cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  ensure
    engine.max_cost = 0
  end

  def samples(engine, expression, kernel, row, params, iterations, repetitions)
    Array.new(repetitions) do
      reset(engine, params)
      allocated = GC.stat(:total_allocated_objects)
      gc = GC.stat(:count)
      started = now
      index = 0
      if kernel
        while index < iterations
          kernel.call(engine, row, params)
          index += 1
        end
      else
        while index < iterations
          engine.evaluate(expression, row)
          index += 1
        end
      end
      elapsed = now - started
      { elapsed_ns: elapsed, ns_per_expression: elapsed.fdiv(iterations), iterations: iterations,
        cost: engine.cost, allocations: GC.stat(:total_allocated_objects) - allocated,
        gc_count: GC.stat(:count) - gc }
    end
  end

  def proof(kernel)
    method = kernel.method(:call)
    iseq = RubyVM::InstructionSequence.of(method) if defined?(RubyVM::InstructionSequence)
    result = { source_location: method.source_location, iseq_label: iseq&.label,
               generated_source_sha256: Digest::SHA256.hexdigest(kernel.source) }
    if defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled? && RubyVM::YJIT.respond_to?(:insns_compiled)
      instructions = RubyVM::YJIT.insns_compiled(method)
      result[:yjit_compiled_instruction_count] = instructions.length / 2
      result[:direct_jit_proof] = !instructions.empty?
    end
    result
  end

  def generated_log_entries(path)
    path ? File.readlines(path, chomp: true).grep(/\(jed compiled expression\)/) : []
  end

  def resolve(engine, job)
    reset(engine, job.fetch(:params))
    started = now
    ast = JedNative::Parser.new(job.fetch(:sql)).parse_expression
    parse_ns = now - started
    started = now
    expression = engine.compile(ast, COLUMNS)
    { expression: expression, parse_ns: parse_ns, resolve_ns: now - started }
  end

  def validate_cases(engine, expression, kernel, job)
    job[:cases].map do |row, params|
      ordinary = outcome(engine, expression, nil, row, params)
      compiled = outcome(engine, expression, kernel, row, params)
      raise "#{job[:name]} value/error/cost mismatch: #{ordinary.inspect} != #{compiled.inspect}" unless ordinary == compiled

      # Every prefix boundary matters, including the exact charge of an error.
      boundaries = (1..[ordinary[:cost], 24].min).map do |limit|
        left = outcome(engine, expression, nil, row, params, limit: limit)
        right = outcome(engine, expression, kernel, row, params, limit: limit)
        raise "#{job[:name]} cost limit #{limit} mismatch: #{left.inspect} != #{right.inspect}" unless left == right
        { limit: limit, outcome: right }
      end
      { row: row, params: params, outcome: compiled, cost_boundaries: boundaries }
    end
  end

  def run_job(job, options)
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_compilation_disabled, true)
    resolved = resolve(engine, job)
    expression = resolved.fetch(:expression)
    types = engine.instance_variable_get(:@types)
    before = jit_stats
    generated_before = generated_log_entries(options[:zjit_log]).length
    started = now
    kernel = JedNative::CompiledExpression.build(expression, types: types)
    compile_ns = now - started
    result = job.slice(:name, :sql).merge(resolved.reject { |key, _| key == :expression },
      admitted: !kernel.nil?, first_compile_ns: compile_ns, jit_before_compile: before)
    return result unless kernel

    reset(engine, job[:params])
    started = now
    value = kernel.call(engine, job[:row], job[:params])
    result[:first_call_ns] = now - started
    result[:first_call] = { value: value, cost: engine.cost }
    result[:jit_after_first_call] = jit_stats
    result[:source_bytes] = kernel.source_bytes
    result[:node_count] = kernel.node_count
    result[:pool_entries] = kernel.pool.length
    result[:validation] = validate_cases(engine, expression, kernel, job)
    result[:compile_samples_ns] = Array.new(options[:compile_samples]) do
      started = now
      duplicate = JedNative::CompiledExpression.build(expression, types: types)
      elapsed = now - started
      raise 'repeat compilation changed admission' unless duplicate
      elapsed
    end
    samples(engine, expression, nil, job[:row], job[:params], options[:warmup], 1)
    samples(engine, expression, kernel, job[:row], job[:params], options[:warmup], 1)
    result[:jit_after_warmup] = jit_stats
    result[:interpreter] = samples(engine, expression, nil, job[:row], job[:params], options[:iterations], options[:repetitions])
    result[:compiled] = samples(engine, expression, kernel, job[:row], job[:params], options[:iterations], options[:repetitions])
    result[:jit_after_timing] = jit_stats
    result[:validation_after_timing] = validate_cases(engine, expression, kernel, job)
    raise "#{job[:name]} warmed behavior changed" unless result[:validation] == result[:validation_after_timing]
    result[:generated_code] = proof(kernel)
    if options[:zjit_log]
      # Only this job's kernel is invoked between these log snapshots. Repeat
      # source-compilation samples create kernels but never execute them.
      count = generated_log_entries(options[:zjit_log]).length - generated_before
      result[:generated_code][:zjit_generated_compilations] = count
      result[:generated_code][:direct_jit_proof] = count.positive?
    end
    result
  end

  # This deliberately tests generator-object lifetime independently of the
  # engine's plan cache. Only 64 kernels are retained, and no global methods or
  # constants are installed. Engine cache behavior needs its own host tests.
  def churn(options)
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_compilation_disabled, true)
    kernels = []
    retained = 64
    GC.start
    before = memory_snapshot
    started = now
    options[:churn].times do |index|
      job = { sql: "$1::i64 + #{index}", params: [7] }
      expression = resolve(engine, job).fetch(:expression)
      kernel = JedNative::CompiledExpression.build(expression, types: engine.instance_variable_get(:@types))
      raise 'churn expression not admitted' unless kernel
      reset(engine, [7])
      options[:churn_calls].times { kernel.call(engine, [], [7]) }
      kernels << kernel
      kernels.shift if kernels.length > retained
    end
    elapsed = now - started
    GC.start
    during = memory_snapshot.merge(retained_kernels: kernels.length,
      retained_source_bytes: kernels.sum(&:source_bytes), retained_pool_entries: kernels.sum { |item| item.pool.length })
    kernels.clear
    kernel = expression = engine = nil
    GC.start
    { plans_created: options[:churn], calls_per_plan: options[:churn_calls], retained_limit: retained,
      elapsed_ns: elapsed, before: before, after_eviction_and_gc: during,
      after_release_and_gc: memory_snapshot,
      note: 'Generator lifetime probe with a diagnostic-owned bounded array, not a proof of the engine cache bound. JIT code reclamation is runtime policy; RSS need not fall after GC.' }
  end

  def main
    options = { warmup: 10_000, iterations: 100_000, repetitions: 5, compile_samples: 30, churn: 1000, churn_calls: 1 }
    OptionParser.new do |parser|
      parser.banner = 'Explicit diagnostic: mise run ruby-native:bounded -- ruby --yjit bench/ruby-native/compiled_diagnostics.rb --report PATH'
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--warmup N', Integer) { |value| options[:warmup] = value }
      parser.on('--iterations N', Integer) { |value| options[:iterations] = value }
      parser.on('--repetitions N', Integer) { |value| options[:repetitions] = value }
      parser.on('--compile-samples N', Integer) { |value| options[:compile_samples] = value }
      parser.on('--churn N', Integer) { |value| options[:churn] = value }
      parser.on('--churn-calls N', Integer) { |value| options[:churn_calls] = value }
      parser.on('--zjit-log PATH') { |value| options[:zjit_log] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:expected_jit] = value }
    end.parse!
    abort 'Specify --report PATH and positive counts; --churn may be zero' unless options[:report] && ARGV.empty? &&
      %i[warmup iterations repetitions compile_samples churn_calls].all? { |key| options[key].positive? } && options[:churn] >= 0

    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    before = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    compiler_path = File.join(source, 'lib/jed_native/compiled_expression.rb')
    require compiler_path if !defined?(JedNative::CompiledExpression) && File.file?(compiler_path)
    abort 'This source has no runtime expression compiler' unless defined?(JedNative::CompiledExpression)
    yjit = !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?)
    zjit = !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?)
    actual_jit = yjit ? 'yjit' : zjit ? 'zjit' : 'none'
    abort "Requested #{options[:expected_jit]}, got #{actual_jit}" if options[:expected_jit] && options[:expected_jit] != actual_jit
    results = JOBS.map { |job| run_job(job, options) }
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION,
      executable: RbConfig.ruby, source: source, source_sha256: before,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest, options: options,
      jit: actual_jit, workloads: results, churn: churn(options),
      notes: ['Direct expression diagnostic, not full-query performance. GC remains enabled.',
        'Semantic compilation and Ruby source compilation are separately measured; JIT compile_time_ns is cumulative runtime work.',
        'Kernel validation compares values, SQLSTATE/message, statement/lifetime cost, and cost-limit prefixes with Engine#evaluate.',
        'Timing samples exclude validation, JIT introspection, and explicit GC. Timings use one fixed representative row/parameter set per expression.'] }
    if options[:zjit_log]
      generated = generated_log_entries(options[:zjit_log])
      report[:zjit_generated_method_proof] = { path: options[:zjit_log], sha256: Digest::SHA256.file(options[:zjit_log]).hexdigest,
        generated_compilations: generated.length, generated_locations: generated.uniq, direct_jit_proof: !generated.empty? }
    elsif zjit
      report[:zjit_generated_method_proof] = { direct_jit_proof: false,
        note: 'Global JIT counters do not prove a particular generated method compiled. Rerun with --zjit-log-compiled-iseqs=PATH and --zjit-log PATH.' }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during diagnostic' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: before, jit: actual_jit,
      workloads: results.map { |job| { name: job[:name], admitted: job[:admitted], generated_code: job[:generated_code] } })
  end
end

CompiledExpressionDiagnostics.main if $PROGRAM_NAME == __FILE__
