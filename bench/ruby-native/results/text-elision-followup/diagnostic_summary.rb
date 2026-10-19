# frozen_string_literal: true

# Imported by summarize.rb after all six direct diagnostic workers have exited.
module ElisionDiagnosticSummary
  NAMES = %w[c_text_projection c_text_equality_filter c_text_nullable_filter c_text_accounted_filter
    c_text_key_vector escaping_text_projection unlabeled_text_parameter].freeze
  COUNTS = { 'rows' => 512, 'warmup' => 2000, 'iterations' => 100, 'scalar_warmup' => 10000,
    'scalar_iterations' => 100000, 'repetitions' => 5, 'compile_samples' => 30, 'churn' => 256, 'churn_calls' => 200 }.freeze
  module_function

  def median(values)
    sorted = values.sort
    (sorted.fetch((sorted.length - 1) / 2) + sorted.fetch(sorted.length / 2)) / 2.0
  end

  def build(directory, root, baseline, final_source, runtime_info)
    summary = { schema: 1, baseline_source_sha256: baseline, source_sha256: final_source,
      statistic: 'Before/final compiled ratios compare the medians of five samples from separate processes. Each process also alternates generic and compiled measurements. Scalar samples contain 100000 one-row calls; relation samples contain 100 calls over 512 rows. Compile medians sum emission and materialization over 30 samples.',
      runtimes: {}, zjit_log_audits: {}, caveats: [
        'These are isolated generated projections/materialized relation phases, excluding parsing, resolution, storage, grouping, sorting and public result collection.',
        'The independently evaluated generic controls establish values, metadata and exact costs. They are not the baseline for the before/final optimization speedup.',
        'Both scalar paths return a one-element array. Finite-memory account and owner state are reseeded identically before each measured invocation.',
        'Validation retains wrapper class and every label field, Unicode/NULL/empty/prefix text, changing/mutated actual text parameters, statement/lifetime budget prefixes and rejected-row accounting. Constant jobs receive unused validation bindings.',
        'Warmup checkpoints follow validation; first compilation and first call are separate observations including runtime warmup effects.',
        'The source/pool cache bounds engine-retained kernels, not VM-retained classes or machine code. Completed-thread churn drops its Thread reference before three full collections; every observation is retained.',
        'Ruby 4.0.7 and 4.1.0-preview1 are different builds. Cross-runtime comparisons do not isolate the two JIT implementations.' ] }
    signatures, sources = {}, {}
    runtime_info.each do |runtime, expected_runtime|
      states = %w[before final].to_h do |state|
        name = "diagnostic-#{state}-#{runtime}"
        raw = JSON.parse(File.read(File.join(directory, "#{name}.json")))
        bound = JSON.parse(File.read(File.join(directory, "#{name}-bounded.json")))
        source = state == 'before' ? baseline : final_source
        raise 'direct diagnostic source differs' unless [raw, bound].all? { |report| report.values_at('source_sha256', 'final_source_sha256') == [source, source] }
        raise 'direct diagnostic failed' unless bound['success'] && bound['reason'].nil?
        raise 'direct diagnostic bounds differ' unless bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [536870912, 2147483648, 300.0]
        raise 'direct diagnostic runtime differs' unless raw.fetch('ruby').start_with?(expected_runtime.fetch(:version)) &&
          raw.values_at('executable', 'jit') == expected_runtime.values_at(:executable, :jit)
        command = bound.fetch('command')
        raise 'direct diagnostic JIT command differs' unless command.include?(raw.fetch('executable')) &&
          command.include?('--yjit') == (raw.fetch('jit') == 'yjit') && command.include?('--zjit') == (raw.fetch('jit') == 'zjit')
        raise 'direct diagnostic script changed' unless raw.fetch('script_sha256') == Digest::SHA256.file(File.join(root, 'bench/ruby-native/elision_diagnostics.rb')).hexdigest
        raise 'direct diagnostic helper set differs' unless raw.fetch('helpers_sha256').keys.sort == %w[compiled_diagnostics.rb relation_diagnostics.rb text_diagnostics.rb]
        raw.fetch('helpers_sha256').each do |path, hash|
          raise 'direct diagnostic helper changed' unless Digest::SHA256.file(File.join(root, 'bench/ruby-native', path)).hexdigest == hash
        end
        raise 'direct diagnostic admission/state differs' unless raw.fetch('options').fetch('expected_elision') == state && !raw.fetch('options')['verify_only']
        raise 'direct diagnostic counts differ' unless raw.fetch('options').slice(*COUNTS.keys) == COUNTS
        raise 'direct diagnostic jobs differ' unless raw.fetch('workloads').map { |job| job.fetch('name') } == NAMES
        limits = raw.fetch('compiler_limits')
        raise 'compiler bounds differ' unless limits == { 'nodes' => 192, 'depth' => 32, 'pool_entries' => 256,
          'pool_bytes' => 32768, 'source_bytes' => 32768, 'relation_cache_entries' => 64 }
        raise 'oversized copied pool was admitted' unless raw.fetch('cache_admission').fetch('oversized_text_pool_rejected')
        summary[:compiler_limits] = limits
        jobs = raw.fetch('workloads').each_with_index.to_h do |job, index|
          job_name = job.fetch('name')
          raise 'direct values/metadata/costs changed during timing' unless job.fetch('validation') == job.fetch('validation_after_timing')
          raise 'direct values/metadata/costs differ across source/runtime' if signatures[job_name] && signatures[job_name] != job.fetch('validation')
          signatures[job_name] = job.fetch('validation')
          raise 'direct copied pool exceeds bound' unless job.fetch('pool_bytes') <= 32768
          generated = job.fetch('generated_code')
          key = [state, job_name]
          raise 'generated source differs across runtimes' if sources[key] && sources[key] != generated.fetch('generated_source_sha256')
          sources[key] = generated.fetch('generated_source_sha256')
          evidence = job.fetch('elision_evidence')
          raise 'source-marker evidence differs from measured method' unless evidence.fetch('generated_source_sha256') == sources[key]
          expected_sites = state == 'final' && index < 5 ? (index == 4 ? 2 : 1) : 0
          raise 'snapshot template admission differs' unless evidence.fetch('text_snapshot_sites') == expected_sites
          raise 'missing direct generated-method JIT proof' unless runtime.end_with?('none') || generated.fetch('direct_jit_proof')
          raise 'missing compiled YJIT instructions' if runtime == '4.0-yjit' && !generated.fetch('yjit_compiled_instruction_count').positive?
          raise 'missing compiled ZJIT window' if runtime == '4.1-zjit' && generated.fetch('zjit_generated_compilations').empty?
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
            first_call_us: job.fetch('first_call_ns') / 1000.0, generated_code: generated, elision_evidence: evidence,
            source_bytes: job.fetch('source_bytes'), pool_entries: job.fetch('pool_entries'), pool_bytes: job.fetch('pool_bytes'))]
        end
        churn = raw.fetch('churn')
        raise 'direct cache bound differs' unless churn.fetch('retained_limit') == 64 && churn.fetch('while_cache_retained').fetch('retained_kernels') == 64
        raise 'cache retained pool bytes exceed bound' unless churn.fetch('while_cache_retained').fetch('retained_pool_bytes') <= 64 * 32768
        raise 'missing completed-thread retention observations' unless churn.fetch('after_completed_thread_exit_and_gc').length == 3
        if runtime == '4.1-zjit'
          info = raw.fetch('zjit_log')
          bytes = File.binread(File.join(directory, "#{name}.log"))
          raise 'ZJIT log prefix differs' unless Digest::SHA256.hexdigest(bytes.byteslice(0, info.fetch('snapshot_bytes'))) == info.fetch('snapshot_sha256')
          logged = bytes.lines(chomp: true).select { |line| /\(jed compiled (?:expression|relation)\)/.match?(line) }
          proof = raw.fetch('workloads').flat_map { |job| job.fetch('generated_code').fetch('zjit_generated_compilations') }
          raise 'ZJIT per-job proof windows absent' unless logged.take(proof.length) == proof && logged.take(info.fetch('generated_entries').length) == info.fetch('generated_entries')
          summary[:zjit_log_audits][state] = { snapshot_bytes: info.fetch('snapshot_bytes'), snapshot_sha256: info.fetch('snapshot_sha256'),
            final_sha256: Digest::SHA256.hexdigest(bytes), per_job_proof_entries: proof.length,
            captured_generated_entries: info.fetch('generated_entries').length, final_generated_entries: logged.length,
            exact_snapshot_prefix_and_all_job_windows_present: true }
        end
        [state, { peak_rss_mib: bound.fetch('peak_rss_bytes').fdiv(1024**2), elapsed_seconds: bound.fetch('elapsed_seconds'),
          jobs: jobs, cache_admission: raw.fetch('cache_admission'), churn: churn }]
      end
      comparisons = NAMES.to_h do |name|
        before, final = states.values_at('before', 'final').map { |state| state.fetch(:jobs).fetch(name) }
        if NAMES.index(name) >= 5
          raise 'control generated source changed' unless before.fetch(:generated_code).fetch('generated_source_sha256') == final.fetch(:generated_code).fetch('generated_source_sha256')
        end
        [name, { before_compiled_us: before.fetch('compiled').fetch(:us_per_call), final_compiled_us: final.fetch('compiled').fetch(:us_per_call),
          speedup: before.fetch('compiled').fetch(:us_per_call) / final.fetch('compiled').fetch(:us_per_call),
          before_allocations: before.fetch('compiled').fetch(:allocations_per_call), final_allocations: final.fetch('compiled').fetch(:allocations_per_call) }]
      end
      summary[:runtimes][runtime] = { states: states, jobs: comparisons }
    end
    File.write(File.join(directory, 'diagnostic-summary.json'), JSON.pretty_generate(summary) + "\n")
    summary
  end
end
