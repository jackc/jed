# frozen_string_literal: true

# Called only after the three direct workers and their ZJIT log have closed.
module LikeDiagnosticSummary
  NAMES = %w[like_prefix_projection like_exact_filter like_suffix_filter like_contains_filter
    like_not_prefix_filter like_accounted_filter like_key_vector].freeze
  COUNTS = { 'rows' => 512, 'warmup' => 2000, 'iterations' => 100, 'scalar_warmup' => 10000,
    'scalar_iterations' => 100000, 'repetitions' => 5, 'compile_samples' => 30, 'churn' => 256, 'churn_calls' => 200 }.freeze
  LIMITS = { 'nodes' => 192, 'depth' => 32, 'pool_entries' => 256, 'pool_bytes' => 32768,
    'source_bytes' => 32768, 'relation_cache_entries' => 64 }.freeze
  module_function

  def median(values)
    sorted = values.sort
    (sorted.fetch((sorted.length - 1) / 2) + sorted.fetch(sorted.length / 2)) / 2.0
  end

  def check_report(raw, bound, source, runtime, root)
    raise 'direct source differs' unless [raw, bound].all? { |report| report.values_at('source_sha256', 'final_source_sha256') == [source, source] }
    raise 'direct worker failed' unless bound['success'] && bound['reason'].nil?
    raise 'direct limits differ' unless bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [536870912, 2147483648, 300.0]
    raise 'direct runtime differs' unless raw.fetch('ruby').start_with?(runtime.fetch(:version)) && raw.values_at('executable', 'jit') == runtime.values_at(:executable, :jit)
    command = bound.fetch('command')
    raise 'direct JIT command differs' unless command.include?(raw.fetch('executable')) &&
      command.include?('--yjit') == (raw.fetch('jit') == 'yjit') && command.include?('--zjit') == (raw.fetch('jit') == 'zjit')
    raise 'direct script changed' unless raw.fetch('script_sha256') == Digest::SHA256.file(File.join(root, 'bench/ruby-native/like_diagnostics.rb')).hexdigest
    raise 'direct helper set differs' unless raw.fetch('helpers_sha256').keys.sort == %w[compiled_diagnostics.rb relation_diagnostics.rb text_diagnostics.rb]
    raw.fetch('helpers_sha256').each do |path, hash|
      raise 'direct helper changed' unless Digest::SHA256.file(File.join(root, 'bench/ruby-native', path)).hexdigest == hash
    end
    raise 'compiler bounds differ' unless raw.fetch('compiler_limits') == LIMITS
    raise 'direct jobs differ' unless raw.fetch('workloads').map { |job| job.fetch('name') } == NAMES
    raise 'fallback controls differ' unless raw.fetch('fallback_admission').length == 6 && raw.fetch('fallback_admission').all? { |job| job.fetch('compiler_rejected') }
  end

  def build(directory, root, baseline, final_source, runtime_info)
    read = ->(name) { JSON.parse(File.read(File.join(directory, name))) }
    baseline_report, baseline_bound = %w[baseline-admission.json baseline-admission-bounded.json].map { |name| read.call(name) }
    check_report(baseline_report, baseline_bound, baseline, runtime_info.fetch('4.0-none'), root)
    raise 'baseline admission was not checked' unless baseline_report.fetch('options').fetch('baseline_admission_only') &&
      baseline_report.fetch('workloads').all? { |job| job.fetch('compiler_rejected') }
    summary = { schema: 1, baseline_source_sha256: baseline, source_sha256: final_source, compiler_limits: LIMITS,
      baseline_rejected_all_like_jobs: true, runtimes: {}, zjit_log_audit: nil,
      statistic: 'Median of five alternating compiled/generic samples in each final-source process. Scalar samples contain 100000 calls; relation samples contain 100 calls over 512 rows. Compile medians sum emission and materialization over 30 samples.',
      caveats: ['These isolated generated phases exclude parsing, resolution, storage, grouping, sorting and public result collection. Generic ratios are not whole-query before/after ratios.',
        'The baseline cannot compile these kernels; whole-query source ABBA establishes optimization gains.',
        'Validation retains exact values, metadata, costs, error-prefix behavior, row identity and finite-memory accounting. Dynamic patterns and unlabelled actual mutable text parameters remain generic fallback controls.',
        'GC stays enabled. First compilation/call are separate observations; warmup checkpoints follow validation and are not pristine cold JIT trajectories.',
        'The bounded engine cache does not bound VM classes or machine code. Completed-thread churn drops its Thread reference before three collections; all observations remain available.',
        'Ruby 4.0.7 and 4.1.0-preview1 are different builds; their comparison does not isolate JIT implementation differences.'] }
    signatures, sources, special_signatures = {}, {}, {}
    runtime_info.each do |runtime, expected_runtime|
      name = "diagnostic-#{runtime}"
      raw, bound = ["#{name}.json", "#{name}-bounded.json"].map { |file| read.call(file) }
      check_report(raw, bound, final_source, expected_runtime, root)
      raise 'direct counts/state differ' unless raw.fetch('options').slice(*COUNTS.keys) == COUNTS && !raw.fetch('options')['verify_only'] && !raw.fetch('options')['baseline_admission_only']
      raise 'fallback generic behavior changed' unless raw.fetch('fallback_admission') == baseline_report.fetch('fallback_admission')
      raise 'cache policy evidence failed' unless raw.fetch('cache_admission').values.all?(true)
      jobs = raw.fetch('workloads').each_with_index.to_h do |job, index|
        job_name = job.fetch('name')
        validation = job.fetch('validation')
        raise 'values/metadata/cost changed during timing' unless validation == job.fetch('validation_after_timing')
        raise 'values/metadata/cost differ across runtime' if signatures[job_name] && signatures[job_name] != validation
        signatures[job_name] = validation
        raise 'copied pool exceeds bound' unless job.fetch('pool_bytes') <= 32768
        generated, evidence = job.values_at('generated_code', 'like_evidence')
        source_hash = generated.fetch('generated_source_sha256')
        raise 'generated source differs across runtime' if sources[job_name] && sources[job_name] != source_hash
        sources[job_name] = source_hash
        raise 'LIKE evidence differs from measured kernel' unless evidence.fetch('generated_source_sha256') == source_hash && evidence.fetch('literal_like_sites') == (index == 6 ? 2 : 1)
        raise 'LIKE special values differ across runtime' if special_signatures[job_name] && special_signatures[job_name] != evidence.fetch('special_cases')
        special_signatures[job_name] = evidence.fetch('special_cases')
        raise 'missing direct JIT proof' unless runtime.end_with?('none') || generated.fetch('direct_jit_proof')
        raise 'YJIT compiled no instructions' if runtime == '4.0-yjit' && !generated.fetch('yjit_compiled_instruction_count').positive?
        raise 'ZJIT generated window missing' if runtime == '4.1-zjit' && generated.fetch('zjit_generated_compilations').empty?
        raise 'direct sample count differs' unless job.fetch('samples').length == 5 && job.fetch('compile_samples').length == 30
        job.fetch('samples').each do |sample|
          left, right = sample.fetch('measurements').values_at('ordinary', 'compiled')
          fields = %w[checksum cost lifetime_cost memory_used owner_bytes iterations]
          raise 'direct timed work differs' unless left.values_at(*fields) == right.values_at(*fields)
        end
        measurements = %w[ordinary compiled].to_h do |kind|
          samples = job.fetch('samples').map { |sample| sample.fetch('measurements').fetch(kind) }
          [kind, { us_per_call: median(samples.map { |sample| sample.fetch('ns_per_call') }) / 1000,
            allocations_per_call: median(samples.map { |sample| sample.fetch('allocations').fdiv(sample.fetch('iterations')) }),
            raw_us_per_call: samples.map { |sample| sample.fetch('ns_per_call') / 1000 } }]
        end
        compile = job.fetch('compile_samples').map { |sample| sample.fetch('prepare_ns') + sample.fetch('materialize_ns') }
        [job_name, measurements.merge(kind: job.fetch('kind'), generic_speedup: measurements.fetch('ordinary').fetch(:us_per_call) / measurements.fetch('compiled').fetch(:us_per_call),
          compile_median_us: median(compile) / 1000, first_compile_us: job.fetch('first_compile_ns') / 1000.0,
          first_call_us: job.fetch('first_call_ns') / 1000.0, generated_code: generated, literal_like_sites: evidence.fetch('literal_like_sites'),
          source_bytes: job.fetch('source_bytes'), pool_entries: job.fetch('pool_entries'), pool_bytes: job.fetch('pool_bytes'))]
      end
      churn = raw.fetch('churn')
      raise 'cache bound differs' unless churn.fetch('retained_limit') == 64 && churn.fetch('while_cache_retained').fetch('retained_kernels') == 64
      raise 'churn source changed with literal data' unless churn.fetch('while_cache_retained').fetch('distinct_generated_sources') == 1
      raise 'retained pool exceeds bound' unless churn.fetch('while_cache_retained').fetch('retained_pool_bytes') <= 64 * 32768
      raise 'retention observations missing' unless churn.fetch('after_completed_thread_exit_and_gc').length == 3
      if runtime == '4.1-zjit'
        info = raw.fetch('zjit_log')
        bytes = File.binread(File.join(directory, "#{name}.log"))
        raise 'ZJIT snapshot prefix differs' unless Digest::SHA256.hexdigest(bytes.byteslice(0, info.fetch('snapshot_bytes'))) == info.fetch('snapshot_sha256')
        logged = bytes.lines(chomp: true).select { |line| /\(jed compiled (?:expression|relation)\)/.match?(line) }
        proof = raw.fetch('workloads').flat_map { |job| job.fetch('generated_code').fetch('zjit_generated_compilations') }
        raise 'ZJIT job windows absent' unless logged.take(proof.length) == proof && logged.take(info.fetch('generated_entries').length) == info.fetch('generated_entries')
        summary[:zjit_log_audit] = { snapshot_bytes: info.fetch('snapshot_bytes'), snapshot_sha256: info.fetch('snapshot_sha256'),
          final_sha256: Digest::SHA256.hexdigest(bytes), per_job_proof_entries: proof.length,
          captured_generated_entries: info.fetch('generated_entries').length, final_generated_entries: logged.length,
          exact_snapshot_prefix_and_all_job_windows_present: true }
      end
      summary[:runtimes][runtime] = { peak_rss_mib: bound.fetch('peak_rss_bytes').fdiv(1024**2), elapsed_seconds: bound.fetch('elapsed_seconds'),
        jobs: jobs, cache_admission: raw.fetch('cache_admission'), churn: churn }
    end
    File.write(File.join(directory, 'diagnostic-summary.json'), JSON.pretty_generate(summary) + "\n")
    summary
  end
end
