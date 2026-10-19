#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit companion probe: distinguish kernel ownership from temporary JIT /
# native-stack liveness after nested helper returns and completed-thread exit.
require_relative 'relation_diagnostics'

module CompiledRelationRetention
  module_function

  def scalar_snapshot?(value)
    case value
    when Hash then value.all? { |key, item| scalar_snapshot?(key) && scalar_snapshot?(item) }
    when Array then value.all? { |item| scalar_snapshot?(item) }
    when NilClass, TrueClass, FalseClass, Numeric, String, Symbol then true
    else false
    end
  end

  def collect
    Array.new(3) do |index|
      GC.start(full_mark: true, immediate_sweep: true)
      CompiledRelationDiagnostics.memory_snapshot.merge(collection: index + 1)
    end
  end

  def completed_thread(options)
    worker = Thread.new { CompiledRelationDiagnostics.populate_cache(options) }
    snapshot = worker.value
    raise 'live object escaped the population thread' unless scalar_snapshot?(snapshot)
    worker = nil
    snapshot
  end

  def main
    options = { churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.banner = 'Explicit probe: mise run ruby-native:bounded -- ruby --zjit bench/ruby-native/relation_retention.rb --report PATH'
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
    end.parse!
    abort 'Specify --report PATH' unless options[:report] && ARGV.empty?
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    source_hash = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    jit = if defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?
      'yjit'
    elsif defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?
      'zjit'
    else
      'none'
    end
    abort "Requested #{options[:jit]}, got #{jit}" if options[:jit] && jit != options[:jit]
    before = collect.last
    nested = CompiledRelationDiagnostics.churn(options)
    raise 'live object escaped churn' unless scalar_snapshot?(nested)
    after_outer_return = collect
    threaded = completed_thread(options)
    after_thread_exit = collect
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
      jit: jit, options: options, source: source, source_sha256: source_hash,
      script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      helper_sha256: Digest::SHA256.file(File.join(__dir__, 'relation_diagnostics.rb')).hexdigest,
      before: before, nested_churn: nested, after_outer_churn_return_and_gc: after_outer_return,
      while_thread_cache_retained: threaded, after_completed_thread_exit_and_gc: after_thread_exit,
      scalar_snapshots_only: true,
      notes: ['Each phase creates 256 generated relation kernels and calls each 200 times through the actual 64-entry source/pool LRU.',
        'The first phase collects after the complete existing churn helper returns. The second uses a completed thread and releases its Thread object before collection, removing temporary VM/native-stack roots.',
        'Only recursively verified scalar snapshots escape helpers. Class and machine-code retention is runtime policy; zero instances does not imply immediate JIT code reclamation.'] }
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during retention probe' unless source_hash == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], jit: jit,
      nested_after_return: nested[:after_helper_return_and_gc].map { |entry| entry[:live_kernels] },
      after_outer_return: after_outer_return.map { |entry| entry[:live_kernels] },
      after_thread_exit: after_thread_exit.map { |entry| entry[:live_kernels] },
      final_classes: after_thread_exit.last[:anonymous_kernel_classes])
  end
end

CompiledRelationRetention.main if $PROGRAM_NAME == __FILE__
