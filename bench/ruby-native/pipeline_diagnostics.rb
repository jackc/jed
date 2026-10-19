#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit-only experiment: isolate a fused row loop from storage/API costs.
# Whole-query and cross-core measurements live in compare/pipeline-plan.json.
require_relative 'compiled_diagnostics'

module CompiledPipelineDiagnostics
  COLUMNS = CompiledExpressionDiagnostics::COLUMNS
  JOBS = [
    { name: 'stream_plain', mode: :stream, projections: %w[score bucket], params: [] },
    { name: 'stream_arithmetic', mode: :stream, predicate: 'bucket >= $1::i32',
      projections: ['score', '(score::i64 * 3 + bucket::i64) / (bucket::i64 + 1)'], params: [2] },
    { name: 'stream_case', mode: :stream,
      projections: ['score', 'CASE WHEN score >= $1::i32 THEN score::i64 * 3 + bucket::i64 ELSE -score::i64 END'], params: [5000] },
    { name: 'stream_offset_limit', mode: :stream, predicate: 'bucket >= $1::i32',
      projections: ['score', 'score * 3 + bucket'], params: [2], offset: 500, limit: 20 },
    { name: 'array_arithmetic', mode: :array, predicate: 'bucket >= $1::i32',
      projections: ['score', '(score::i64 * 3 + bucket::i64) / (bucket::i64 + 1)'], params: [2] }
  ].freeze
  module_function

  def now = CompiledExpressionDiagnostics.now
  def reset(engine, params, limit = 0) = CompiledExpressionDiagnostics.reset(engine, params, limit)
  def jit_stats = CompiledExpressionDiagnostics.jit_stats

  def compile(engine, job)
    engine.instance_variable_set(:@params, job[:params])
    resolver = ->(sql) { engine.compile(JedNative::Parser.new(sql).parse_expression, COLUMNS) }
    predicate = engine.compile_filter(JedNative::Parser.new(job[:predicate]).parse_expression, COLUMNS) if job[:predicate]
    projections = job[:projections].map(&resolver)
    types = engine.instance_variable_get(:@types)
    { predicate: predicate, projections: projections, types: types,
      predicate_kernel: predicate && JedNative::CompiledExpression.build(predicate, types: types),
      projection_kernel: JedNative::CompiledExpression.build_many(projections, types: types) }
  end

  def build(resolved, job)
    JedNative::CompiledPipeline.build(resolved[:predicate], resolved[:projections], types: resolved[:types],
      mode: job[:mode], offset: job.key?(:offset), limit: job.key?(:limit))
  end

  # This is the former executor loop with precompiled expression calls. The
  # comparison removes only loop specialization/fusion, not expression codegen.
  def expression_loop(engine, resolved, input, params, output, job)
    predicate = resolved[:predicate]
    predicate_kernel = resolved[:predicate_kernel]
    projection_kernel = resolved[:projection_kernel]
    projections = resolved[:projections]
    if job[:mode] == :array
      index = 0
      while index < input.length
        row = input[index]
        index += 1
        next if predicate && (predicate_kernel ? predicate_kernel.call(engine, row, params) : engine.evaluate(predicate, row)) != true
        if projection_kernel
          values = projection_kernel.call(engine, row, params)
        else
          values = Array.new(projections.length)
          column = 0
          while column < values.length
            values[column] = engine.evaluate(projections[column], row)
            column += 1
          end
        end
        output << values
      end
      return output
    end
    offset, limit = job.fetch(:offset, 0), job[:limit]
    return output if limit == 0
    input.each do |row|
      engine.charge
      next if predicate && (predicate_kernel ? predicate_kernel.call(engine, row, params) : engine.evaluate(predicate, row)) != true
      if offset.positive?
        offset -= 1
        next
      end
      values = if projection_kernel
        projection_kernel.call(engine, row, params)
      else
        projections.map { |expression| engine.evaluate(expression, row) }
      end
      engine.charge
      output << values
      break if limit && output.length >= limit
    end
    output
  end

  def invoke(engine, resolved, kernel, input, params, output, job)
    if kernel
      kernel.call(engine, input, params, output, job.fetch(:offset, 0), job[:limit])
    else
      expression_loop(engine, resolved, input, params, output, job)
    end
  end

  def outcome(engine, resolved, kernel, input, params, job, limit = 0)
    reset(engine, params, limit)
    output = []
    invoke(engine, resolved, kernel, input, params, output, job)
    { status: 'value', rows: output, cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  rescue JedNative::Error => error
    { status: 'error', rows: output, sqlstate: error.sqlstate, message: error.message,
      cost: engine.cost, lifetime_cost: engine.lifetime_cost }
  ensure
    engine.max_cost = 0
  end

  def validate(engine, resolved, kernel, job)
    inputs = [[], [[9876, 4], [nil, 4], [-9876, 0], [7, nil]], [[1, -1], [2**31 - 1, 9]]]
    # Reach the projection after OFFSET and admit a zero divisor through the
    # arithmetic filter. These errors must also survive warmed JIT execution.
    inputs << Array.new(job[:offset]) { [7, 4] } + [[2**31 - 1, 9]] if job[:offset]
    parameters = [job[:params], job[:params].map { nil }]
    parameters << [-2] if job[:name].end_with?('_arithmetic')
    results = inputs.product(parameters).map do |input, params|
      expected = outcome(engine, resolved, nil, input, params, job)
      actual = outcome(engine, resolved, kernel, input, params, job)
      raise "#{job[:name]} semantics differ: #{expected.inspect} != #{actual.inspect}" unless expected == actual
      limits = (1..[expected[:cost], 64].min).to_a
      limits += [expected[:cost] - 1, expected[:cost], expected[:cost] + 1].select(&:positive?)
      boundaries = limits.uniq.sort.map do |limit|
        left = outcome(engine, resolved, nil, input, params, job, limit)
        right = outcome(engine, resolved, kernel, input, params, job, limit)
        raise "#{job[:name]} cost prefix #{limit} differs" unless left == right
        { limit: limit, outcome: right }
      end
      { input: input, params: params, outcome: actual, cost_prefixes: boundaries }
    end
    if job[:name].end_with?('_arithmetic')
      raise 'division-by-zero probe did not execute' unless results.any? { |item| item[:outcome][:sqlstate] == '22012' }
    elsif job[:offset]
      raise 'post-offset overflow probe did not execute' unless results.any? { |item| item[:outcome][:sqlstate] == '22003' }
    end
    results
  end

  def samples(engine, resolved, kernel, input, job, iterations, repetitions)
    params = job[:params]
    Array.new(repetitions) do
      reset(engine, params)
      allocated, gc = GC.stat(:total_allocated_objects), GC.stat(:count)
      started = now
      iterations.times { invoke(engine, resolved, kernel, input, params, [], job) }
      elapsed = now - started
      { elapsed_ns: elapsed, ns_per_query: elapsed.fdiv(iterations), iterations: iterations,
        cost: engine.cost, allocations: GC.stat(:total_allocated_objects) - allocated,
        gc_count: GC.stat(:count) - gc }
    end
  end

  def proof(kernel)
    method = kernel.method(:call)
    iseq = RubyVM::InstructionSequence.of(method)
    sequences = [iseq]
    iseq.each_child { |child| sequences << child }
    records = sequences.map do |sequence|
      item = { label: sequence.label, path: sequence.path, first_lineno: sequence.first_lineno }
      if defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled? && RubyVM::YJIT.respond_to?(:insns_compiled)
        instructions = RubyVM::YJIT.insns_compiled(sequence)
        item[:compiled_instruction_count] = instructions.length / 2
        item[:direct_jit_proof] = !instructions.empty?
      end
      item
    end
    { source_location: method.source_location, source_sha256: Digest::SHA256.hexdigest(kernel.source),
      instruction_sequences: records,
      direct_jit_proof: records.all? { |record| record[:direct_jit_proof] } }
  end

  def generated_log_entries(path)
    path ? File.readlines(path, chomp: true).grep(/\(jed compiled pipeline\)/) : []
  end

  def run_job(job, options)
    engine = JedNative::Engine.new
    resolved = compile(engine, job)
    logged_before = generated_log_entries(options[:zjit_log]).length
    started = now
    kernel = build(resolved, job)
    compile_ns = now - started
    raise "#{job[:name]} not admitted" unless kernel
    input = Array.new(1000) { |index| [(index * 37) % 10_000, index % 10] }
    reset(engine, job[:params])
    started = now
    first_output = invoke(engine, resolved, kernel, input, job[:params], [], job)
    first_call_ns = now - started
    result = job.merge(first_compile_ns: compile_ns, first_call_ns: first_call_ns,
      first_output_rows: first_output.length, first_cost: engine.cost,
      source_bytes: kernel.source_bytes, node_count: kernel.node_count, pool_entries: kernel.pool.length)
    result[:validation] = validate(engine, resolved, kernel, job)
    result[:compile_samples_ns] = Array.new(options[:compile_samples]) do
      started = now
      raise 'repeat admission differs' unless build(resolved, job)
      now - started
    end
    samples(engine, resolved, nil, input, job, options[:warmup], 1)
    samples(engine, resolved, kernel, input, job, options[:warmup], 1)
    result[:jit_after_warmup] = jit_stats
    result[:expression_kernel_loop] = samples(engine, resolved, nil, input, job, options[:iterations], options[:repetitions])
    result[:pipeline] = samples(engine, resolved, kernel, input, job, options[:iterations], options[:repetitions])
    result[:validation_after_timing] = validate(engine, resolved, kernel, job)
    raise 'warmed behavior differs' unless result[:validation] == result[:validation_after_timing]
    result[:generated_code] = proof(kernel)
    if options[:zjit_log]
      generated = generated_log_entries(options[:zjit_log]).drop(logged_before)
      records = result[:generated_code][:instruction_sequences]
      records.each do |record|
        # ZJIT logs the first instruction's source line; ISeq.first_lineno is
        # the block opening line. Only this job's pipeline is invoked between
        # snapshots, so label/path identify its method or hot loop directly.
        location = "#{record[:label]}@#{record[:path]}:"
        record[:zjit_compiled] = generated.any? { |entry| entry.start_with?(location) }
        record[:direct_jit_proof] = record[:zjit_compiled]
      end
      result[:generated_code][:all_instruction_sequences_logged] = records.all? { |record| record[:direct_jit_proof] }
      hot_loop = records.find { |record| record[:label] == (job[:mode] == :stream ? 'block in call' : 'call') }
      result[:generated_code][:direct_jit_proof] = hot_loop.fetch(:direct_jit_proof)
      result[:generated_code][:zjit_generated_compilations] = generated
      result[:generated_code][:note] = 'Direct proof concerns the generated hot loop. A missing standalone wrapper entry is not proof of its separate compilation.'
    end
    result
  end

  def memory_snapshot
    base = JedNative::CompiledPipeline::Kernel
    { rss_bytes: CompiledExpressionDiagnostics.rss_bytes, live_slots: GC.stat(:heap_live_slots), jit: jit_stats,
      live_kernels: ObjectSpace.each_object(base).count,
      anonymous_kernel_classes: ObjectSpace.each_object(Class).count { |type| type.superclass.equal?(base) } }
  end

  def churn(options)
    engine = JedNative::Engine.new
    kernels = []
    input = [[7, 4]]
    retained = 64
    GC.start
    before = memory_snapshot
    started = now
    options[:churn].times do |index|
      job = { mode: :stream, projections: ["score::i64 + #{index}"], params: [] }
      resolved = compile(engine, job)
      kernel = build(resolved, job)
      reset(engine, [])
      options[:churn_calls].times { kernel.call(engine, input, [], []) }
      kernels << kernel
      kernels.shift if kernels.length > retained
    end
    elapsed = now - started
    GC.start
    during = memory_snapshot.merge(retained_kernels: kernels.length, retained_source_bytes: kernels.sum(&:source_bytes))
    kernels.clear
    kernel = resolved = engine = nil
    GC.start
    { plans_created: options[:churn], calls_per_plan: options[:churn_calls], retained_limit: retained,
      elapsed_ns: elapsed, before: before, after_eviction_and_gc: during, after_release_and_gc: memory_snapshot,
      note: 'Pipeline generator object lifetime, not an engine cache-bound proof. Whole-process JIT/RSS counters include diagnostic control-expression methods. JIT code/class retention is controlled by Ruby.' }
  end

  def main
    options = { warmup: 300, iterations: 200, repetitions: 5, compile_samples: 30, churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
      parser.on('--zjit-log PATH') { |value| options[:zjit_log] = File.expand_path(value) }
      %i[warmup iterations repetitions compile_samples churn churn_calls].each do |key|
        parser.on("--#{key.to_s.tr('_', '-')} N", Integer) { |value| options[key] = value }
      end
    end.parse!
    abort 'Specify --report PATH and positive counts; churn may be zero' unless options[:report] && ARGV.empty? &&
      %i[warmup iterations repetitions compile_samples churn_calls].all? { |key| options[key].positive? } && options[:churn] >= 0
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    before = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    abort 'Source has no pipeline compiler' unless defined?(JedNative::CompiledPipeline)
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
      notes: ['Direct 1000-row loops, no storage/planner/API work. Baseline uses already-compiled expression methods.',
        'GC remains enabled. Compilation/first-call and warmed-loop measurements are separate.',
        'Both method and stream block JIT admission are inspected after warmed correctness checks.',
        'Validation compares partial output, values, SQLSTATE/message, statement/lifetime costs, and cost-limit prefixes.'] }
    if options[:zjit_log]
      report[:zjit_log] = { path: options[:zjit_log], sha256: Digest::SHA256.file(options[:zjit_log]).hexdigest,
        generated_entries: generated_log_entries(options[:zjit_log]) }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during diagnostic' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: before, jit: jit,
      workloads: results.map { |job| { name: job[:name], generated_code: job[:generated_code] } })
  end
end

CompiledPipelineDiagnostics.main if $PROGRAM_NAME == __FILE__
