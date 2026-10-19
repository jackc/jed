#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit companion to aggregate_diagnostics.rb. Its in-method churn snapshot
# can include dead JIT stack references; inspect again after the frame returns.
require_relative 'aggregate_diagnostics'

module CompiledAggregateRetention
  module_function

  def populate_and_release(options)
    engine = JedNative::Engine.new
    group = [[7, 1, true], [nil, 0, nil], [-3, 9, false]].freeze
    options[:churn].times do |index|
      sql = "sum(score + #{index})"
      raise 'new aggregate cache entry eagerly compiled' unless CompiledAggregateDiagnostics.cached_kernel(engine, sql).nil?
      kernel = CompiledAggregateDiagnostics.cached_kernel(engine, sql)
      raise 'second use did not materialize aggregate' unless kernel
      options[:churn_calls].times { kernel.call(engine, group, []) }
    end
    cache = engine.instance_variable_get(:@native_compiled_aggregates)
    raise 'cache bound differs' unless cache.length == [options[:churn], JedNative::CompiledPlans::MAX_AGGREGATE_CACHE_ENTRIES].min
    GC.start(full_mark: true, immediate_sweep: true)
    snapshot = CompiledAggregateDiagnostics.memory_snapshot.merge(cache_entries: cache.length,
      retained_source_bytes: cache.values.sum(&:source_bytes), retained_pool_entries: cache.values.sum { |kernel| kernel.pool.length })
    # Return numbers and JIT statistics only. Engine, cache, resolved expressions,
    # generated objects, and their classes never leave this helper frame.
    snapshot
  end

  def main
    options = { churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.banner = 'Explicit retention probe: mise run ruby-native:bounded -- ruby --zjit bench/ruby-native/aggregate_retention.rb --report PATH'
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
      parser.on('--churn N', Integer) { |value| options[:churn] = value }
      parser.on('--churn-calls N', Integer) { |value| options[:churn_calls] = value }
    end.parse!
    abort 'Specify --report PATH and positive churn counts' unless ARGV.empty? && options[:report] && options[:churn].positive? && options[:churn_calls].positive?
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    source_hash = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    yjit = !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?)
    zjit = !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?)
    jit = yjit ? 'yjit' : zjit ? 'zjit' : 'none'
    abort "Requested #{options[:jit]}, got #{jit}" if options[:jit] && options[:jit] != jit
    GC.start(full_mark: true, immediate_sweep: true)
    before = CompiledAggregateDiagnostics.memory_snapshot
    while_retained = populate_and_release(options)
    after_return = Array.new(3) do |index|
      GC.start(full_mark: true, immediate_sweep: true)
      CompiledAggregateDiagnostics.memory_snapshot.merge(gc_after_return: index + 1)
    end
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
      jit: jit, options: options, source: source, source_sha256: source_hash,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      helper_sha256: Digest::SHA256.file(File.join(__dir__, 'aggregate_diagnostics.rb')).hexdigest,
      before: before, while_cache_retained: while_retained, after_helper_return_and_gc: after_return,
      notes: ['Same source/pool LRU, second-use admission, 256 generated kernels, and 200 calls per kernel as the main direct diagnostic.',
        'The cache-populating helper returns only scalar snapshots. These collections run after its engine/cache/kernel local variables leave scope.',
        'VM-retained anonymous classes and JIT code may outlive generated instances. This probe does not require RSS or JIT code counters to fall.'] }
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during retention probe' unless source_hash == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: source_hash, jit: jit,
      before_kernels: before[:live_kernels], retained_kernels: while_retained[:live_kernels],
      after_return_kernels: after_return.map { |snapshot| snapshot[:live_kernels] },
      after_return_classes: after_return.map { |snapshot| snapshot[:anonymous_kernel_classes] })
  end
end

CompiledAggregateRetention.main if $PROGRAM_NAME == __FILE__
