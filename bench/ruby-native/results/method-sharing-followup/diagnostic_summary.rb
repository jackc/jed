# frozen_string_literal: true

# Independent retained-artifact checks. Call only when all18 workers have exited.
module SharingDiagnosticSummary
  NAMES = %w[scalar pipeline_array pipeline_stream ordering aggregate relation_filter relation_keys].freeze
  COUNTS = { 'rows' => 128, 'warmup' => 2000, 'iterations' => 100, 'repetitions' => 5,
    'compile_samples' => 30, 'churn' => 256, 'churn_calls' => 200 }.freeze
  module_function

  def median(values)
    values = values.sort
    (values.fetch((values.length - 1) / 2) + values.fetch(values.length / 2)) / 2.0
  end

  def samples(items, count_key = 'calls')
    { us: median(items.map { |item| item.fetch('ns_per_call', item.fetch('elapsed_ns')) }) / 1000,
      allocations: median(items.map { |item| item.fetch('allocations').fdiv(item.fetch(count_key, 1)) }) }
  end

  def load_report(directory, root, state, runtime, shape, source, identity)
    name = "diagnostic-#{state}-#{runtime}-#{shape}"
    raw = JSON.parse(File.read(File.join(directory, "#{name}.json")))
    bounded = JSON.parse(File.read(File.join(directory, "#{name}-bounded.json")))
    raise 'source differs' unless [raw, bounded].all? { |item| item.values_at('source_sha256', 'final_source_sha256') == [source, source] }
    raise 'bounded diagnostic failed' unless bounded.fetch('success') && bounded['reason'].nil?
    raise 'diagnostic bounds differ' unless bounded.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [536870912, 2147483648, 300.0]
    raise 'diagnostic runtime differs' unless raw.fetch('ruby').start_with?(identity.fetch(:version)) && raw.values_at('executable', 'jit') == identity.values_at(:executable, :jit)
    command = bounded.fetch('command')
    raise 'diagnostic runtime flags differ' unless command.include?(identity.fetch(:executable)) && command.include?('--yjit') == (identity[:jit] == 'yjit') && command.include?('--zjit') == (identity[:jit] == 'zjit')
    raise 'diagnostic script changed' unless raw.fetch('script_sha256') == Digest::SHA256.file(File.join(root, 'bench/ruby-native/sharing_diagnostics.rb')).hexdigest
    raw.fetch('helpers_sha256').each { |path, hash| raise 'helper changed' unless Digest::SHA256.file(File.join(root, 'bench/ruby-native', path)).hexdigest == hash }
    options = raw.fetch('options')
    raise 'diagnostic options differ' unless options.slice(*COUNTS.keys) == COUNTS && options.values_at('shape', 'expected') == [shape, state] && !options['verify_only']
    shared = state == 'final'
    raise 'method sharing state differs' unless raw.fetch('method_sharing') == shared
    raise 'compiler limits differ' unless raw.fetch('compiler_limits') == { 'method_cache_entries' => shared ? 64 : 0, 'source_bytes' => 32768, 'pool_bytes' => 32768, 'instance_cache_entries' => 64 }
    log_audit = nil
    if identity[:jit] == 'zjit'
      info = raw.fetch('zjit_log')
      bytes = File.binread(File.join(directory, "#{name}.log"))
      raise 'ZJIT snapshot differs' unless Digest::SHA256.hexdigest(bytes.byteslice(0, info.fetch('snapshot_bytes'))) == info.fetch('snapshot_sha256')
      lines = bytes.lines(chomp: true).select { |line| /\(jed compiled (?:expression|pipeline|ordering|aggregate|relation)\)/.match?(line) }
      raise 'ZJIT generated snapshot differs' unless lines.take(info.fetch('generated_entries').length) == info.fetch('generated_entries')
      raw.fetch('workloads', []).each do |job|
        %w[donor_proof recipient_proof].each do |field|
          proof = job.fetch(field)
          raise 'ZJIT proof window differs' unless lines[proof.fetch('zjit_log_start')...proof.fetch('zjit_log_end')] == proof.fetch('zjit_entries')
        end
      end
      log_audit = { snapshot_bytes: info.fetch('snapshot_bytes'), snapshot_sha256: info.fetch('snapshot_sha256'),
        final_sha256: Digest::SHA256.hexdigest(bytes), final_generated_entries: lines.length,
        snapshot_and_windows_match: true }
    end
    [raw, { peak_rss_bytes: bounded.fetch('peak_rss_bytes'), elapsed_seconds: bounded.fetch('elapsed_seconds'), zjit_log: log_audit }]
  end

  def build(directory, root, baseline, final_source, identities)
    result = { baseline_source_sha256: baseline, source_sha256: final_source, runtimes: {},
      statistic: 'Single fresh before/final process per runtime and diagnostic shape. Medians of30 warm materializations/new-pool builds and five steady samples; cold/first-call numbers are individual observations, not ABBA estimates.',
      caveats: ['Each source has separate fresh processes for family evidence, one-source literal-pool churn, and distinct-source churn.',
        'Global sharing intentionally retains up to64 classes. Kernel instances must be distinguished from cached classes and VM-retained code. Cache bounds do not bound all JIT memory.',
        'Donor JIT evidence precedes recipient creation; identical Class/method owner/UnboundMethod establishes recipient reuse of compiled method. ZJIT need not emit a new log line for that recipient.',
        'Churn total elapsed includes independent oracle checks and method invocations. Its second-admission samples include fresh SQL parsing/resolution inside the actual cache helper; they are not materialization-only timings.',
        'Ordering has no literal pool; its per-instance comparator Proc remains distinct. Whole-query source ABBA is authoritative for application throughput.'] }
    semantics, emitted, variants = {}, {}, {}
    identities.each do |runtime, identity|
      states = %w[before final].to_h do |state|
        source = state == 'before' ? baseline : final_source
        raw, metadata = load_report(directory, root, state, runtime, 'families', source, identity)
        raise 'family list differs' unless raw.fetch('workloads').map { |job| job.fetch('name') } == NAMES
        jobs = raw.fetch('workloads').to_h do |job|
          name = job.fetch('name')
          shared = state == 'final'
          validation = job.values_at('validation', 'recipient_validation')
          raise 'post-timing semantic change' unless job.fetch('validation') == job.fetch('validation_after_timing')
          raise 'cross-source/runtime semantic mismatch' if semantics[name] && semantics[name] != validation
          semantics[name] = validation
          raise 'emitted source changed' if emitted[name] && emitted[name] != job.fetch('generated_source_sha256')
          emitted[name] = job.fetch('generated_source_sha256')
          reuse = job.fetch('reuse')
          raise 'instance/method reuse differs' unless reuse.fetch('distinct_instances') && reuse.values_at('same_class', 'same_method_owner', 'same_unbound_method').all? { |value| value == shared }
          raise 'pool diversity/immutability differs' unless reuse.fetch('immutable_pool') && (name == 'ordering' ? reuse.fetch('ordering_distinct_procs') : reuse.fetch('different_pool'))
          if identity[:jit] != 'none'
            raise 'donor did not JIT' unless job.fetch('donor_proof').fetch('direct_jit_proof')
            raise 'receiver did not inherit donor method' unless job.fetch('recipient_before_first_call').fetch('inherited_jit_proof') == shared
            raise 'baseline receiver did not independently JIT' unless shared || job.fetch('recipient_proof').fetch('direct_jit_proof')
          end
          raise 'sample counts differ' unless job.fetch('samples').length == 5 && job.fetch('recipient_samples').length == 5 && job.fetch('materialize_samples').length == 30 && job.fetch('new_pool_build_samples').length == 30
          expected_calls = name == 'scalar' ? 10000 : 100
          %w[samples recipient_samples].each do |field|
            cost = field == 'samples' ? job.fetch('first_call').fetch('cost') : job.fetch('recipient_validation').last.fetch('outcome').fetch('cost')
            job.fetch(field).each do |sample|
              raise 'timed call count/cost differs' unless sample.values_at('calls', 'cost') == [expected_calls, cost * expected_calls]
              raise 'timed per-call arithmetic differs' unless sample.fetch('ns_per_call') == sample.fetch('elapsed_ns').fdiv(expected_calls)
            end
          end
          job.fetch('materialize_samples').each { |sample| raise 'warm materialization sharing differs' unless sample.fetch('distinct_instance') && sample.fetch('same_class') == shared }
          pool_checks = job.fetch('new_pool_build_samples').map do |sample|
            raise 'new pool build sharing differs' unless sample.fetch('same_class') == shared
            sample.values_at('pool_sha256', 'validated_outcome_sha256')
          end
          raise 'varied pool outcomes changed' if variants[name] && variants[name] != pool_checks
          variants[name] = pool_checks
          [name, { source_sha256: job.fetch('generated_source_sha256'), source_bytes: job.fetch('source_bytes'),
            cold_build_us: job.fetch('cold_build_ns') / 1000.0, first_call_us: job.fetch('first_call_ns') / 1000.0,
            recipient_build_us: job.fetch('recipient_build_ns') / 1000.0, recipient_first_call_us: job.fetch('recipient_first_call_ns') / 1000.0,
            materialize: samples(job.fetch('materialize_samples')), new_pool_build: samples(job.fetch('new_pool_build_samples')),
            steady: samples(job.fetch('samples')), recipient_steady: samples(job.fetch('recipient_samples')),
            reuse: reuse, donor_proof: job.fetch('donor_proof'), receiver_inherited_jit: job.fetch('recipient_before_first_call').fetch('inherited_jit_proof') }]
        end
        churns = %w[pools sources].to_h do |shape|
          churn_raw, churn_metadata = load_report(directory, root, state, runtime, shape, source, identity)
          churn = churn_raw.fetch('churn')
          during = churn.fetch('while_engine_retained')
          raise 'churn instance/source counts differ' unless during.values_at('instance_cache_entries', 'distinct_sources') == [64, shape == 'pools' ? 1 : 256]
          raise 'churn method count differs' unless during.fetch('method_cache').fetch('entries') == (state == 'before' ? 0 : shape == 'pools' ? 1 : 64)
          raise 'churn revisit differs' unless during.fetch('revisit').values_at('fresh_instance', 'same_class', 'old_kernel_still_callable') == [true, state == 'final' && shape == 'pools', true]
          checks = during.fetch('validated_variants')
          raise 'churn variants not all checked' unless checks.length == 256
          key = "churn-#{shape}"
          raise 'churn outputs differ' if semantics[key] && semantics[key] != checks
          semantics[key] = checks
          raise 'churn GC observations absent' unless churn.fetch('after_thread_exit_and_gc').length == 3
          [shape, churn_metadata.merge(observations: churn)]
        end
        [state, metadata.merge(jobs: jobs, churn: churns)]
      end
      comparisons = NAMES.to_h do |name|
        before, final = states.values_at('before', 'final').map { |state| state.fetch(:jobs).fetch(name) }
        [name, { materialize_speedup: before[:materialize][:us] / final[:materialize][:us],
          new_pool_build_speedup: before[:new_pool_build][:us] / final[:new_pool_build][:us],
          steady_speedup: before[:steady][:us] / final[:steady][:us] }]
      end
      result[:runtimes][runtime] = { states: states, comparisons: comparisons }
    end
    File.write(File.join(directory, 'diagnostic-summary.json'), JSON.pretty_generate(result) + "\n")
    result
  end
end
