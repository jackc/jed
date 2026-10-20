# frozen_string_literal: true

# Instrumented replay only. These observations are not benchmark timings.
require 'optparse'
require 'rbconfig'
require 'time'
require_relative '../../../binding_compare_lane'

module QueryWalkWarmupStats
  ENGINE = 'native-prepared'
  TARGET = 'three_table_join'
  module_function

  def counters(label)
    # Calls stay outside every workload loop. Collection itself allocates, so
    # these deltas describe an instrumented replay, not the untraced processes.
    zjit = RubyVM::ZJIT.stats
    raise 'ZJIT statistics are unavailable' unless zjit.is_a?(Hash) && !zjit.empty?
    gc = GC.stat
    {
      label: label,
      zjit: zjit.select { |_, value| value.is_a?(Numeric) && value.finite? },
      gc: gc.select { |_, value| value.is_a?(Numeric) && value.finite? },
      gc_total_time_ns: GC.respond_to?(:total_time) ? GC.total_time : nil
    }
  end

  def main
    options = {}
    OptionParser.new do |parser|
      parser.on('--plan PATH') { |value| options[:plan] = File.expand_path(value) }
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
    end.parse!
    abort 'Specify --plan and --report' unless ARGV.empty? && options.values_at(:plan, :report).all?
    raise 'Report already exists' if File.exist?(options.fetch(:report))
    raise 'This replay requires ZJIT without YJIT' unless
      RubyVM::ZJIT.enabled? && !RubyVM::YJIT.enabled?
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE'))
    source_hash = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    plan = JSON.parse(File.read(options.fetch(:plan)))
    raise 'Unexpected repetitions' unless plan.fetch('repetitions') == RubyBindingComparison::REPETITIONS
    jobs = plan.fetch('workloads').map do |job|
      job.merge('validation_iterations' => job.fetch('validation_iterations', plan.fetch('validation_iterations')),
        'kind' => job.fetch('kind', 'explicit_plan'))
    end
    # Match the benchmark's separate validation database and its prior work.
    validation = RubyBindingComparison.validate(ENGINE, plan, jobs)
    observations = []
    replay = []
    RubyBindingComparison.with_session(ENGINE, plan.fetch('rows')) do |session|
      jobs.each do |job|
        run = RubyBindingComparison.runner(ENGINE, session, job)
        params = RubyBindingComparison.parameters(job)
        target = job.fetch('name') == TARGET
        observations << counters('before_warmup') if target
        job.fetch('warmup').times { |i| RubyBindingComparison.drain(ENGINE, run, params[i]) }
        observations << counters('after_warmup') if target
        row_counts = RubyBindingComparison::REPETITIONS.times.map do |batch|
          rows = 0
          job.fetch('iterations').times { |i| rows += RubyBindingComparison.drain(ENGINE, run, params[i]) }
          observations << counters("after_batch_#{batch + 1}") if target
          rows
        end
        replay << {
          name: job.fetch('name'), warmup: job.fetch('warmup'), iterations: job.fetch('iterations'),
          rows_per_sample: row_counts,
          warmup_parameters_sha256: Digest::SHA256.hexdigest(JSON.generate(params.first(job.fetch('warmup')))),
          batch_parameters_sha256: Digest::SHA256.hexdigest(JSON.generate(params.first(job.fetch('iterations'))))
        }
      end
    end
    final_hash = NativeRubySource.digest(source)
    raise 'Source changed during replay' unless final_hash == source_hash
    report = {
      schema: 1, mode: 'adaptive_zjit_warmup_statistics_only', created_at: Time.now.utc.iso8601(6),
      runtime: RUBY_DESCRIPTION, ruby_executable: RbConfig.ruby,
      ruby_executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest,
      yjit: RubyVM::YJIT.enabled?, zjit: RubyVM::ZJIT.enabled?,
      source: source, source_sha256: source_hash, final_source_sha256: final_hash,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      worker_sha256: Digest::SHA256.file(File.expand_path('../../../binding_compare_lane.rb', __dir__)).hexdigest,
      plan_path: options.fetch(:plan), plan_sha256: Digest::SHA256.file(options.fetch(:plan)).hexdigest,
      engine: ENGINE, dataset_rows: plan.fetch('rows'), repetitions: plan.fetch('repetitions'),
      target: TARGET, validation: validation, replay: replay, observations: observations,
      gc_measure_total_time: GC.respond_to?(:measure_total_time) ? GC.measure_total_time : nil,
      notes: [
        'Adaptive follow-up to the original ZJIT prepared three-table regression; original reports remain primary.',
        'All nine jobs retain their original order, fixture, parameter schedule, validation and batch sizes.',
        'Only the target warmup differs between short and long plans. Each batch restarts at parameter index zero.',
        'No elapsed query times are measured. Statistics collection, its allocations and the extra replay frames may affect JIT/GC behavior.',
        'Numeric ZJIT and GC counters are sampled outside target warmup and batch loops; GC.stat time is milliseconds and GC.total_time is nanoseconds.',
        'Counter changes can identify activity in this replay but do not establish the cause of an uninstrumented timing difference.'
      ]
    }
    File.write(options.fetch(:report), JSON.generate(report) + "\n")
    puts "Saved instrumented replay #{options.fetch(:report)}"
  end
end

QueryWalkWarmupStats.main if $PROGRAM_NAME == __FILE__
