# frozen_string_literal: true

# Read retained evidence only; never starts a query or benchmark worker.
require 'json'
require 'digest'
require 'time'

module CheckpointBlockingEvidence
  APIS = %w[prepared query execute execute-prepared].freeze
  BUDGETS = [8 * 1024**2, 64 * 1024].freeze
  STATES = %w[1-before 2-final 3-final 4-before].freeze
  SCRIPT_SHA256 = 'a98ca3b32847a0a99547004f691f9e890677f8072a40da310a3b154f5a2d6533'
  PLAN_SHA256 = '1576fc31520030fbae3f9fd40b765d0b4db7806fbe14742500a607c8023d9f32'
  SIGNATURE_FIELDS = %w[validation_sha256 validation_values_sha256 validation_rows validation_costs
    validation_columns validation_column_types].freeze
  module_function

  def assert(condition, message)
    raise "blocking evidence: #{message}" unless condition
  end

  def median(values)
    ordered = values.sort
    (ordered.fetch((ordered.length - 1) / 2) + ordered.fetch(ordered.length / 2)) / 2.0
  end

  def hash?(value) = value.is_a?(String) && value.match?(/\A[0-9a-f]{64}\z/)
  def count?(value) = value.is_a?(Integer) && value >= 0
  def positive?(value) = value.is_a?(Numeric) && value.finite? && value.positive?

  def check_spill(entry, fixture, job)
    spill = entry.fetch('spill')
    files = spill.fetch('files')
    assert(spill.fetch('created') == files.length, 'scratch file count differs')
    assert(files.map { |file| file.fetch('path') }.uniq.length == files.length, 'duplicate scratch paths')
    fields = %w[write_calls write_bytes read_calls read_bytes]
    files.each do |file|
      assert(%w[JedNative::ScratchFile JedNative::ScratchBuckets].include?(file.fetch('kind')), 'unknown scratch class')
      assert(File.dirname(file.fetch('path')) == fixture.fetch('scratch_directory'), 'scratch escaped private disk fixture')
      assert(file.fetch('removed_after_drain') == true && !File.exist?(file.fetch('path')), 'scratch survived drain')
      assert(fields.all? { |field| count?(file.fetch(field)) }, 'invalid scratch counters')
    end
    fields.each do |field|
      assert(count?(spill.fetch(field)) && spill.fetch(field) == files.sum { |file| file.fetch(field) }, 'scratch totals differ')
    end
    if entry.fetch('work_mem') == BUDGETS.first
      assert(files.empty? && fields.all? { |field| spill.fetch(field).zero? }, 'high work_mem spilled')
    else
      assert(job.fetch('low_work_mem_must_spill') && fields.all? { |field| spill.fetch(field).positive? }, 'low work_mem did not write/read scratch')
    end
    operators = spill.fetch('operators')
    assert(operators.values.all? { |value| count?(value) }, 'invalid operator counters')
    if job['must_use_hash_rows']
      assert(operators.fetch('hash_rows_push') == 1024, 'join did not consume all hash input rows')
    end
    spill.slice('created', *fields, 'operators')
  end

  def check_fixture(fixture, root, fixtures)
    directory = fixture.fetch('scratch_directory')
    parent = File.join(root, '.dev/ruby-native-blocking-workspaces')
    assert(File.dirname(directory) == parent && File.basename(directory).start_with?('checkpoint-'), 'unexpected fixture location')
    assert(fixture.fetch('path') == File.join(directory, 'blocking.jed'), 'unexpected database path')
    assert(fixture.values_at('rows', 'join_rows', 'locking', 'read_only_during_measurement', 'removed_after_run') ==
      [4096, 1024, 'exclusive', true, true], 'fixture dimensions, locking or cleanup differ')
    assert(!File.exist?(directory) && !File.exist?(fixture.fetch('path')), 'private fixture survived worker')
    assert(hash?(fixture.fetch('file_sha256')) && positive?(fixture.fetch('file_bytes')), 'invalid fixture identity')
    filesystem = fixture.fetch('filesystem')
    assert(filesystem.fetch('path') == directory, 'filesystem probe used a different directory')
    assert(!%w[tmpfs ramfs devtmpfs].include?(filesystem.fetch('type')), 'fixture was memory-backed')
    assert(%w[source type mount_point].all? { |key| filesystem.fetch(key).is_a?(String) && !filesystem.fetch(key).empty? } &&
      count?(filesystem.fetch('device')), 'incomplete filesystem provenance')
    identity = fixture.slice('file_sha256', 'file_bytes', 'rows', 'join_rows').merge(
      'filesystem' => filesystem.slice('source', 'type', 'mount_point', 'device'))
    assert(fixtures.empty? || fixtures.first == identity, 'fixture bytes or filesystem changed across workers')
    fixtures << identity if fixtures.empty?
  end

  def check_report(directory, name, source, runtime, root, plan, signatures, fixtures, verify: false)
    raw = JSON.parse(File.read(File.join(directory, "#{name}.json")))
    bound = JSON.parse(File.read(File.join(directory, "#{name}-bounded.json")))
    assert(raw.values_at('schema', 'mode', 'repetitions') == [1, 'file_blocking_checkpoint', 5], 'report schema differs')
    [raw, bound].each do |report|
      assert(report.values_at('source_sha256', 'final_source_sha256') == [source, source], 'source differs or changed')
    end
    assert(raw.fetch('source') == bound.fetch('source'), 'supervisor source path differs')
    assert(bound.fetch('success') && bound.fetch('reason').nil?, 'worker failed')
    assert(bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [536870912, 2147483648, 600.0], 'supervisor limits differ')
    assert(positive?(bound.fetch('peak_rss_bytes')) && bound.fetch('peak_rss_bytes') <= 536870912, 'RSS exceeded guard')
    assert(positive?(bound.fetch('elapsed_seconds')) && bound.fetch('elapsed_seconds') <= 600, 'deadline exceeded')
    assert(raw.fetch('ruby').start_with?(runtime.fetch(:version)) &&
      raw.values_at('executable', 'jit') == runtime.values_at(:executable, :jit), 'runtime identity differs')
    assert(raw.fetch('executable_sha256') == Digest::SHA256.file(runtime.fetch(:executable)).hexdigest, 'runtime executable changed')
    command = bound.fetch('command')
    assert(command.first == runtime.fetch(:executable), 'worker used a different executable')
    assert(command.include?('--yjit') == (runtime.fetch(:jit) == 'yjit') &&
      command.include?('--zjit') == (runtime.fetch(:jit) == 'zjit'), 'JIT command differs')
    if runtime.fetch(:jit) == 'none'
      assert(command.include?('--disable-yjit') && command.include?('--disable-zjit'), 'interpreter JIT-disable flags missing')
    end
    assert(command.include?('--verify-only') == verify && command.include?('bench/ruby-native/blocking_diagnostics.rb'), 'worker command differs')
    options = raw.fetch('options')
    assert(options.values_at('api', 'verify_only', 'jit', 'report') ==
      ['all', verify, runtime.fetch(:jit), File.join(directory, "#{name}.json")] && !options.key?('work_mem'), 'worker options differ')
    assert(raw.fetch('script_sha256') == SCRIPT_SHA256, 'worker script changed')
    assert(raw.values_at('plan_path', 'plan_sha256') == [File.join(root, 'bench/ruby-native/compare/blocking-plan.json'), PLAN_SHA256], 'plan changed')
    assert(raw.fetch('helpers_sha256') == { 'source.rb' => Digest::SHA256.file(File.join(root, 'bench/ruby-native/source.rb')).hexdigest }, 'source helper changed')
    assert(raw.fetch('limits') == { 'address_space_bytes' => 2147483648, 'required_supervisor_rss_bytes' => 536870912,
      'supervisor_timeout_seconds' => '600', 'max_query_memory_bytes' => 0 }, 'worker resource settings differ')
    fixture = raw.fetch('fixture')
    check_fixture(fixture, root, fixtures)
    jobs = plan.fetch('workloads').to_h { |job| [job.fetch('name'), job] }
    expected_keys = BUDGETS.flat_map { |budget| APIS.flat_map { |api| jobs.keys.map { |name| [api, budget, name] } } }
    assert(raw.fetch('workloads').map { |entry| entry.values_at('api', 'work_mem', 'name') } == expected_keys, 'API/budget/query coverage differs')
    entries = raw.fetch('workloads').to_h do |entry|
      job = jobs.fetch(entry.fetch('name'))
      assert(entry.values_at('sql', 'parameters', 'iterations', 'warmup') == job.values_at('sql', 'parameters', 'iterations', 'warmup'), 'SQL or schedule differs')
      signature = entry.slice(*SIGNATURE_FIELDS)
      assert(%w[validation_sha256 validation_values_sha256].all? { |field| hash?(signature.fetch(field)) }, 'invalid answer digest')
      assert(%w[validation_rows validation_costs].all? do |field|
        signature.fetch(field).length == job.fetch('parameters').length && signature.fetch(field).all? { |value| count?(value) }
      end, 'validation answer count differs')
      assert(signature.fetch('validation_columns').length == signature.fetch('validation_column_types').length &&
        !signature.fetch('validation_columns').empty?, 'column metadata dimensions differ')
      key = entry.fetch('name')
      assert(!signatures.key?(key) || signatures.fetch(key) == signature, 'values, metadata or costs differ across sources/APIs/budgets')
      signatures[key] = signature
      if verify
        assert(%w[samples_ns rows_per_sample allocations_per_sample].all? { |field| entry.fetch(field).empty? } &&
          !entry.key?('gc_counts') && entry.fetch('median_ns').nil?, 'verify-only report contains measurements')
      else
        samples = entry.fetch('samples_ns')
        assert(samples.length == 5 && samples.all? { |value| positive?(value) }, 'timing samples incomplete')
        expected_rows = job.fetch('iterations').times.sum do |index|
          signature.fetch('validation_rows')[index % job.fetch('parameters').length]
        end
        assert(entry.fetch('rows_per_sample') == Array.new(5, expected_rows), 'timed drains consumed different rows')
        assert(%w[allocations_per_sample gc_counts].all? do |field|
          entry.fetch(field).length == 5 && entry.fetch(field).all? { |value| count?(value) }
        end, 'allocation/GC samples incomplete')
        assert(entry.fetch('median_ns') == median(samples).fdiv(job.fetch('iterations')), 'reported median differs')
      end
      spill = check_spill(entry, fixture, job)
      [entry.values_at('api', 'work_mem', 'name'), { median_us: entry['median_ns'] && entry.fetch('median_ns') / 1000.0,
        samples_ns: entry.fetch('samples_ns'), rows_per_sample: entry.fetch('rows_per_sample'),
        allocations_per_query: verify ? nil : median(entry.fetch('allocations_per_sample')).fdiv(job.fetch('iterations')),
        spill: spill }]
    end
    { entries: entries, resource: { report: "#{name}.json", source_sha256: source, runtime: raw.fetch('ruby'),
      peak_rss_mib: bound.fetch('peak_rss_bytes').fdiv(1024**2), elapsed_seconds: bound.fetch('elapsed_seconds'),
      timeout_seconds: 600, fixture: fixture, verify_only: verify } }
  end

  def build(directory:, root:, baseline:, final_source:, runtime_info:)
    plan_path = File.join(root, 'bench/ruby-native/compare/blocking-plan.json')
    assert(Digest::SHA256.file(plan_path).hexdigest == PLAN_SHA256, 'current plan changed')
    assert(Digest::SHA256.file(File.join(root, 'bench/ruby-native/blocking_diagnostics.rb')).hexdigest == SCRIPT_SHA256, 'current worker changed')
    plan = JSON.parse(File.read(plan_path))
    assert(plan.values_at('schema', 'rows', 'join_rows', 'work_mem_bytes', 'repetitions') == [1, 4096, 1024, BUDGETS, 5], 'fixture plan differs')
    assert(plan.fetch('workloads').all? { |job| job.values_at('warmup', 'iterations') == [40, 3] }, 'blocking warmup or iterations differ')
    signatures, fixtures, resources = {}, [], {}
    verify = check_report(directory, 'verify-blocking-final', final_source, runtime_info.fetch('4.0-yjit'), root,
      plan, signatures, fixtures, verify: true)
    resources['verify-blocking-final'] = verify.fetch(:resource)
    quartet = STATES.map do |state|
      name = "blocking-4.0-yjit-#{state}"
      source = state.end_with?('before') ? baseline : final_source
      result = check_report(directory, name, source, runtime_info.fetch('4.0-yjit'), root, plan, signatures, fixtures)
      resources[name] = result.fetch(:resource)
      result.fetch(:entries)
    end
    other_runtimes = %w[4.0-none 4.1-zjit].to_h do |runtime|
      name = "blocking-#{runtime}-final"
      result = check_report(directory, name, final_source, runtime_info.fetch(runtime), root, plan, signatures, fixtures)
      resources[name] = result.fetch(:resource)
      [runtime, result.fetch(:entries)]
    end
    tables = APIS.to_h do |api|
      [api, BUDGETS.to_h do |budget|
        [budget.to_s, plan.fetch('workloads').to_h do |job|
          key = [api, budget, job.fetch('name')]
          entries = quartet.map { |report| report.fetch(key) }
          medians = entries.map { |entry| entry.fetch(:median_us) }
          before, final = medians.values_at(0, 3), medians.values_at(1, 2)
          [job.fetch('name'), { before_us: before.sum / 2.0, final_us: final.sum / 2.0,
            speedup: before.sum.fdiv(final.sum), elapsed_change_percent: (final.sum.fdiv(before.sum) - 1) * 100,
            before_medians_us: before, final_medians_us: final,
            final_medians_below_both_before: final.max < before.min,
            final_medians_above_both_before: final.min > before.max,
            before_allocations_per_query: entries.values_at(0, 3).map { |entry| entry.fetch(:allocations_per_query) },
            final_allocations_per_query: entries.values_at(1, 2).map { |entry| entry.fetch(:allocations_per_query) },
            spill: entries.map { |entry| entry.fetch(:spill) } }]
        end]
      end]
    end
    runtimes = other_runtimes.to_h do |runtime, entries|
      [runtime, APIS.to_h do |api|
        [api, BUDGETS.to_h do |budget|
          [budget.to_s, plan.fetch('workloads').to_h { |job| [job.fetch('name'), entries.fetch([api, budget, job.fetch('name')])] }]
        end]
      end]
    end
    { baseline_source_sha256: baseline, final_source_sha256: final_source, worker_sha256: SCRIPT_SHA256,
      plan_sha256: PLAN_SHA256, fixture_identity: fixtures.first, matching_values_metadata_costs: signatures,
      yjit_abba: tables, final_runtimes: runtimes, resources: resources,
      statistic: 'YJIT means of two process medians in original-source/final/final/original-source order; microseconds per complete query. Other runtimes are single-process final-source characterizations, not before/after estimates.',
      caveats: ['Four APIs and both work_mem budgets fully drain the same disk-backed queries; max_query_memory_bytes is unlimited to isolate work_mem.',
        'All timing precedes scratch-I/O and hash-row tracing. Actual scratch writes and reads are positive under 64 KiB and absent under 8 MiB.',
        'Fixtures and scratch files are private, disk-backed and removed after use. Read-only fixture digests match across all workers.',
        'This is a modest blocking-query checkpoint, not cold-OS-cache or whole-engine memory-limit evidence.'] }
  end
end
