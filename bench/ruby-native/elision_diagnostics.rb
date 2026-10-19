#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit follow-up using the unchanged text diagnostic's controls/validation.
# Run this identical script against the archived wrapping and final sources.
require_relative 'text_diagnostics'

module CompiledTextElisionDiagnostics
  JOBS = [
    { name: 'c_text_projection', kind: :scalar, sql: "label >= 'row5'", params: [], elision_target: true },
    { name: 'c_text_equality_filter', kind: :filter, sql: "label = 'row500'", params: [], elision_target: true },
    { name: 'c_text_nullable_filter', kind: :filter, sql: "flag AND label >= 'row5'", params: [], elision_target: true },
    { name: 'c_text_accounted_filter', kind: :filter, sql: "flag AND label >= 'row5'", params: [], memory: true, elision_target: true },
    { name: 'c_text_key_vector', kind: :keys,
      sql: ["CASE WHEN label >= 'row5' THEN score ELSE bucket END", "CASE WHEN label = '' THEN 1 ELSE 0 END"], params: [], elision_target: true },
    { name: 'escaping_text_projection', kind: :scalar, sql: 'label::text', params: [], elision_target: false },
    { name: 'unlabeled_text_parameter', kind: :scalar, sql: 'label >= $1::text', params: ['row5'], elision_target: false }
  ].freeze
  MARKER = '# C text snapshots'
  module_function

  # Check emitted source after all timings, so additional resolution/emission
  # cannot warm a later job's first-compilation measurement.
  def emitted_evidence(job, expected_hash)
    engine = JedNative::Engine.new
    engine.instance_variable_set(:@native_compilation_disabled, true)
    resolved = CompiledTextDiagnostics.resolve(engine, job)
    program = CompiledTextDiagnostics.prepare(engine, resolved, job)
    raise 'evidence emission was rejected' unless program
    hash = Digest::SHA256.hexdigest(program.source)
    raise 'evidence source differs from timed kernel' if expected_hash && hash != expected_hash
    { generated_source_sha256: hash, text_snapshot_sites: program.source.scan(MARKER).length }
  end

  def main
    options = { rows: 512, warmup: 2000, iterations: 100, scalar_warmup: 10_000, scalar_iterations: 100_000,
      repetitions: 5, compile_samples: 30, churn: 256, churn_calls: 200 }
    OptionParser.new do |parser|
      parser.banner = 'Explicit diagnostic: elision_diagnostics.rb --report PATH --expected-elision before|final'
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
      parser.on('--zjit-log PATH') { |value| options[:zjit_log] = File.expand_path(value) }
      parser.on('--expected-elision STATE', %w[before final]) { |value| options[:expected_elision] = value }
      parser.on('--verify-only') { options[:verify_only] = true }
      %i[rows warmup iterations scalar_warmup scalar_iterations repetitions compile_samples churn churn_calls].each do |key|
        parser.on("--#{key.to_s.tr('_', '-')} N", Integer) { |value| options[key] = value }
      end
    end.parse!
    abort 'Specify --report, --expected-elision and positive counts' unless options[:report] && options[:expected_elision] && ARGV.empty? &&
      %i[rows warmup iterations scalar_warmup scalar_iterations repetitions compile_samples churn_calls].all? { |key| options[key].positive? } && options[:churn] >= 0
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(__dir__, '../../impl/ruby-native')))
    before = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    yjit = !!(defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?)
    zjit = !!(defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?)
    jit = yjit ? 'yjit' : zjit ? 'zjit' : 'none'
    abort "Requested #{options[:jit]}, got #{jit}" if options[:jit] && options[:jit] != jit
    results = JOBS.map do |job|
      options[:verify_only] ? CompiledTextDiagnostics.verify_job(job, options) : CompiledTextDiagnostics.run_job(job, options)
    end
    results.zip(JOBS).each do |result, job|
      result[:elision_evidence] = emitted_evidence(job, result.dig(:generated_code, :generated_source_sha256))
      expected = options[:expected_elision] == 'final' && job[:elision_target]
      raise "#{job[:name]} snapshot admission differs" unless result[:elision_evidence][:text_snapshot_sites].positive? == expected
    end
    report = { schema: 1, created_at: Time.now.utc.iso8601, ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
      source: source, source_sha256: before, script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      helpers_sha256: %w[compiled_diagnostics.rb relation_diagnostics.rb text_diagnostics.rb].to_h { |path| [path, Digest::SHA256.file(File.join(__dir__, path)).hexdigest] },
      jit: jit, options: options, workloads: results, cache_admission: CompiledTextDiagnostics.cache_admission,
      churn: options[:verify_only] ? nil : CompiledTextDiagnostics.churn(options),
      compiler_limits: { nodes: JedNative::CompiledExpression::MAX_NODES, depth: JedNative::CompiledExpression::MAX_DEPTH,
        pool_entries: JedNative::CompiledExpression::MAX_POOL, pool_bytes: JedNative::CompiledExpression::MAX_POOL_BYTES,
        source_bytes: JedNative::CompiledExpression::MAX_SOURCE_BYTES,
        relation_cache_entries: JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES },
      notes: ['Identical script and generic controls run against archived before and final source; no engine toggle is used.',
        'Five targets have immediate C-labeled column/literal comparison children. The text cast projection must retain escaping metadata; the unlabeled parameter comparison remains a control and exercises actual changing/mutable bindings.',
        'All text_diagnostics semantic cases and statement/lifetime budget prefixes are retained, including wrapper class, every label field, row identity and finite-memory release. Extra bindings in constant jobs are unused direct-call context.',
        'Source-marker evidence is emitted and compared with the timed source hash only after all job timers finish. It establishes the new template was selected; JIT proof independently checks each generated method.',
        'GC stays enabled. Scalar jobs return a one-element projection array in both modes; relation jobs use select/map controls. Warmup checkpoints follow validation and are not pristine cold JIT trajectories.',
        'JSON whitespace may be compacted after execution with compact.rb; every parsed value and all cost-prefix records are retained, with canonical semantic hashes recorded separately.'] }
    if options[:zjit_log]
      snapshot = File.binread(options[:zjit_log])
      report[:zjit_log] = { path: options[:zjit_log], snapshot_bytes: snapshot.bytesize,
        snapshot_sha256: Digest::SHA256.hexdigest(snapshot), generated_entries: snapshot.lines(chomp: true).select { |line| CompiledTextDiagnostics::LOG_PATTERN.match?(line) },
        note: 'Snapshot precedes serialization; audit its exact byte prefix and each job window against the final log.' }
    elsif zjit
      report[:zjit_log] = { direct_jit_proof: false, note: 'Use --zjit-log-compiled-iseqs=PATH and --zjit-log PATH for direct proof.' }
    end
    report[:final_source_sha256] = NativeRubySource.digest(source)
    raise 'source changed during diagnostic' unless before == report[:final_source_sha256]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], source_sha256: before, jit: jit,
      workloads: results.map { |job| job.slice(:name, :generated_code, :elision_evidence) })
  end
end

CompiledTextElisionDiagnostics.main if $PROGRAM_NAME == __FILE__
