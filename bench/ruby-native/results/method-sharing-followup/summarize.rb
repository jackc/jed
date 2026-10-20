# frozen_string_literal: true

# Run only after every measured/verification artifact is complete. This checks
# retained evidence before writing the derived summary and complete manifest.
require 'json'
require 'digest'
require_relative '../../source'
require_relative 'diagnostic_summary'
require_relative 'compact'
require_relative 'correctness_summary'

BASELINE = '3e749562c6b764681d6917604cf02aa73b3194cfdbb8c5e756616588de91dc88'
RUNTIMES = %w[4.0-none 4.0-yjit 4.1-zjit].freeze
NATIVE_LANES = %w[native-prepared native-query native-eager].freeze
STATES = %w[1-before 2-final 3-final 4-before].freeze
ROOT = File.expand_path('../../../..', __dir__)
PLAN_PATH = File.join(ROOT, 'bench/ruby-native/compare/sharing-plan.json')
PLAN = JSON.parse(File.read(PLAN_PATH))
JOBS = PLAN.fetch('workloads').to_h { |job| [job.fetch('name'), job] }
PLAN_HASH = Digest::SHA256.file(PLAN_PATH).hexdigest
RUNTIME_INFO = {
  '4.0-none' => { executable: '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', version: 'ruby 4.0.7 ', jit: 'none' },
  '4.0-yjit' => { executable: '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', version: 'ruby 4.0.7 ', jit: 'yjit' },
  '4.1-zjit' => { executable: File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby'), version: 'ruby 4.1.0preview1 ', jit: 'zjit' }
}.freeze

def read(name) = JSON.parse(File.read(File.join(__dir__, name)))
def median(values)
  sorted = values.sort
  (sorted.fetch((sorted.length - 1) / 2) + sorted.fetch(sorted.length / 2)) / 2.0
end

def source_matches!(report, source)
  raise 'source differs or changed' unless report['source_sha256'] == source && report['final_source_sha256'] == source
end

def bounded!(report, source, timeout: nil, expected_exit_one: false)
  source_matches!(report, source)
  if expected_exit_one
    raise 'expected completed failure audit exited differently' unless report['success'] == false && report['reason'] == 'worker exited 1'
  else
    raise 'bounded worker failed' unless report['success'] && report['reason'].nil?
  end
  raise 'bounded limits differ' unless report.values_at('memory_bytes', 'address_space_bytes') == [536870912, 2147483648]
  raise 'bounded timeout differs' if timeout && report['timeout_seconds'] != timeout
end

def named_files_hash(paths)
  digest = Digest::SHA256.new
  paths.sort.each do |path|
    digest << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path)
  end
  digest.hexdigest
end

def runtime_matches!(runtime, description, executable, jit, command: nil)
  expected = RUNTIME_INFO.fetch(runtime)
  raise 'runtime identity differs' unless description.start_with?(expected.fetch(:version)) && executable == expected.fetch(:executable) && jit == expected.fetch(:jit)
  if command
    raise 'runtime command differs' unless command.include?(executable) && command.include?('--yjit') == (jit == 'yjit') && command.include?('--zjit') == (jit == 'zjit')
  end
end

def binding_report!(filename, source, lanes, disabled, answers, schedules, names: JOBS.keys)
  report = read(filename)
  runtime = filename[/\d\.\d-(?:none|yjit|zjit)/]
  raise 'orchestrator runtime differs' unless report.fetch('options').values_at('ruby', 'jit') == RUNTIME_INFO.fetch(runtime).values_at(:executable, :jit)
  raise "measurement failed: #{filename}" unless report.fetch('errors').empty?
  provenance = report.fetch('source_sha256')
  raise 'source/harness changed' unless provenance == report.fetch('source_sha256_after')
  raise 'wrong native source' unless provenance.fetch('native') == source
  raise 'plan changed' unless provenance.fetch('selected_plan') == PLAN_HASH
  raise 'explicit plan selection differs' unless report.fetch('options').fetch('plan') == PLAN_PATH
  raise 'worker changed' unless provenance.fetch('worker') == Digest::SHA256.file(File.join(ROOT, 'bench/ruby-native/binding_compare_lane.rb')).hexdigest
  raise 'wrong lanes' unless report.fetch('lanes').keys.sort == lanes.sort
  report.fetch('lanes').each do |lane, entry|
    raise "lane failed: #{filename} #{lane}" unless entry['success'] && entry['reason'].nil?
    raise 'lane resource limits differ' unless entry.values_at('memory_limit_bytes', 'address_space_bytes') == [536870912, 2147483648]
    result = entry.fetch('result')
    raise 'worker plan differs' unless result.values_at('plan_path', 'plan_sha256') == [PLAN_PATH, PLAN_HASH]
    raise 'worker hash differs' unless result.fetch('script_sha256') == provenance.fetch('worker')
    jit = result.fetch('jit')
    raise 'invalid JIT state' unless jit.values.all? { |value| value == true || value == false } && !(jit['yjit'] && jit['zjit'])
    actual_jit = jit['yjit'] ? 'yjit' : jit['zjit'] ? 'zjit' : 'none'
    runtime_matches!(runtime, result.fetch('runtime'), result.fetch('ruby_executable'), actual_jit, command: entry.fetch('command'))
    raise 'runtime executable changed' unless result.fetch('ruby_executable_sha256') == Digest::SHA256.file(result.fetch('ruby_executable')).hexdigest
    raise 'wrong lane engine' unless result.fetch('engine') == lane
    raise 'worker source changed' unless result['source_sha256'] == result['final_source_sha256']
    source_matches!(result, source) unless lane == 'wrapper'
    raise 'wrong diagnostic switch' unless result['text_compilation_disabled'] == disabled && entry.fetch('command').include?('--disable-text') == disabled
    raise 'unrelated compiler disabled' if %w[pipeline_disabled ordering_disabled aggregate_disabled relation_disabled].any? { |flag| result[flag] }
    raise 'fixture/repetitions differ' unless result.values_at('dataset_rows', 'repetitions') == [1000, 5]
    raise 'wrong jobs' unless result.fetch('workloads').map { |job| job.fetch('name') }.sort == names.sort
    result.fetch('workloads').each do |job|
      name = job.fetch('name')
      expected = JOBS.fetch(name)
      raise 'SQL/schedule differs' unless job.values_at('sql', 'warmup', 'iterations') == expected.values_at('sql', 'warmup', 'iterations')
      params = Array.new(32) { |i| expected.fetch('span').zero? ? [] : [expected.fetch('offset') + (i * expected.fetch('step')) % expected.fetch('span')] }
      raise 'validation parameter cycle differs' unless job.fetch('validation_parameters') == params
      raise 'incomplete measured samples' unless job.fetch('samples_ns').length == 5 && job.fetch('rows_per_sample').length == 5
      raise 'reported median differs' unless job.fetch('median_ns') == median(job.fetch('samples_ns')).fdiv(job.fetch('iterations'))
      signature = job.slice('validation_sha256', 'answer_sha256', 'validation_rows', 'validation_costs', 'validation_columns', 'validation_column_types')
      raise "values/metadata/exact costs differ: #{name}" if answers[name] && answers[name] != signature
      answers[name] = signature
      schedule = job.slice('warmup', 'iterations', 'rows_per_sample')
      raise 'consumed row counts differ' if schedules[name] && schedules[name] != schedule
      schedules[name] = schedule
    end
  end
  report
end

summary = { statistic: 'Mean of two per-process medians per ABBA state; microseconds per complete query. Raw samples and paired medians remain available. These are observations, not confidence intervals.',
  baseline_source_sha256: BASELINE, runtime_identities: RUNTIME_INFO, controls: {}, validation: {} }
answers, schedules = {}, {}
source = read('control-4.0-none-2-final.json').fetch('source_sha256').fetch('native')
raise 'current source differs from final measured source' unless NativeRubySource.digest(File.join(ROOT, 'impl/ruby-native')) == source
summary[:source_sha256] = source
raise 'wrong plan fixture/counts' unless JOBS.length == 7 && PLAN.values_at('rows', 'repetitions', 'validation_iterations') == [1000, 5, 32]
correctness = SharingCorrectnessEvidence.build(source, Digest::SHA256.file(File.join(ROOT, 'impl/ruby-native/bin/conformance.rb')).hexdigest)
provenance = correctness.fetch(:provenance).merge('baseline_commit' => 'fc12f42798420fabc91dd6ee1274b961647071f8',
  'baseline_source_sha256' => BASELINE, 'final_source_sha256' => source, 'plan_sha256' => PLAN_HASH)
File.write(File.join(__dir__, 'source.json'), JSON.pretty_generate(provenance) + "\n")
summary[:validation][:source_provenance] = provenance
summary[:validation][:correctness] = correctness.fetch(:summary)

RUNTIMES.each do |runtime|
  reports = STATES.map do |state|
    binding_report!("control-#{runtime}-#{state}.json", state.end_with?('before') ? BASELINE : source,
      NATIVE_LANES, false, answers, schedules)
  end
  summary[:controls][runtime] = NATIVE_LANES.to_h do |lane|
    [lane, JOBS.keys.to_h do |name|
      medians = reports.map { |report| report.fetch('lanes').fetch(lane).fetch('result').fetch('workloads').find { |job| job['name'] == name }.fetch('median_ns') }
      before, final = medians.values_at(0, 3), medians.values_at(1, 2)
      [name, { before_us: before.sum / 2000.0, final_us: final.sum / 2000.0,
        speedup: before.sum.fdiv(final.sum), elapsed_change_percent: (final.sum.fdiv(before.sum) - 1) * 100,
        before_medians_ns: before, final_medians_ns: final,
        final_medians_below_both_before: final.max < before.min,
        final_medians_above_both_before: final.min > before.max }]
    end]
  end
end
summary[:validation][:matching_values_metadata_costs] = answers
summary[:validation][:schedules] = schedules

isolated_names = %w[scalar arithmetic_projection text_like_fallback]
isolated = STATES.map do |state|
  binding_report!("isolated-control-4.0-yjit-#{state}.json", state.end_with?('before') ? BASELINE : source,
    NATIVE_LANES, false, answers, schedules, names: isolated_names)
end
summary[:isolated_yjit_controls] = NATIVE_LANES.to_h do |lane|
  [lane, isolated_names.to_h do |name|
    medians = isolated.map { |report| report.fetch('lanes').fetch(lane).fetch('result').fetch('workloads').find { |job| job['name'] == name }.fetch('median_ns') }
    before, final = medians.values_at(0, 3), medians.values_at(1, 2)
    [name, { before_us: before.sum / 2000.0, final_us: final.sum / 2000.0,
      speedup: before.sum.fdiv(final.sum), elapsed_change_percent: (final.sum.fdiv(before.sum) - 1) * 100,
      before_medians_ns: before, final_medians_ns: final,
      final_medians_below_both_before: final.max < before.min,
      final_medians_above_both_before: final.min > before.max }]
  end]
end

diagnostics = SharingDiagnosticSummary.build(__dir__, ROOT, BASELINE, source, RUNTIME_INFO)
summary[:diagnostics] = { summary: 'diagnostic-summary.json', all_five_families_seven_consumers_verified: true,
  note: 'Direct diagnostic observations are single before/final processes. Global generated-method sharing and exact pool isolation are asserted separately from timing.' }
summary[:validation][:run_journals] = { 'controls' => 12, 'diagnostics' => 18, 'isolated-controls' => 4 }.to_h do |phase, count|
  journal = read("run-#{phase}.json")
  raise 'runner changed' unless journal.fetch('script_sha256') == Digest::SHA256.file(File.join(__dir__, 'run.rb')).hexdigest
  raise 'runner plan/source options differ' unless journal.fetch('plan_sha256') == PLAN_HASH && journal.fetch('options').fetch('final_hash') == source
  raise 'runner phase/count differs' unless journal.fetch('phase') == phase && journal.fetch('commands').length == count
  if phase == 'isolated-controls'
    raise 'isolated driver changed' unless journal.fetch('options').fetch('isolation_driver_sha256') == Digest::SHA256.file(File.join(__dir__, 'isolated_controls.rb')).hexdigest
    raise 'isolated workload set differs' unless journal.fetch('commands').all? do |entry|
      command = entry.fetch('command')
      command.fetch(command.index('--workloads') + 1) == isolated_names.join(',')
    end
  end
  journal.fetch('commands').each do |entry|
    expected = entry.fetch('name').include?('before') ? BASELINE : source
    raise 'runner failed or source changed' unless entry.fetch('success') && entry.values_at('source_sha256', 'final_source_sha256') == [expected, expected]
    raise 'runner omitted explicit mise task' unless entry.fetch('command').take(2) == %w[mise run]
  end
  [phase, { commands: count, all_successful: true, runner_sha256: journal.fetch('script_sha256') }]
end
%w[focused-interpreter-bounded.json focused-yjit-bounded.json].each do |name|
  report = read(name)
  bounded!(report, source)
  raise 'focused tests failed' unless report.fetch('stdout').include?('failures') || report.fetch('stdout').include?('method sharing')
end

File.write(File.join(__dir__, 'summary.json'), JSON.pretty_generate(summary) + "\n")
ledger = SharingEvidenceJSON.run(__dir__)
ledger.fetch('files').each do |name, record|
  path = File.join(__dir__, name)
  raise 'compacted bytes differ' unless File.size(path) == record.fetch('compact_bytes') && Digest::SHA256.file(path).hexdigest == record.fetch('compact_file_sha256')
  raise 'compacted semantics differ' unless Digest::SHA256.hexdigest(JSON.generate(JSON.parse(File.read(path)))) == record.fetch('parsed_json_sha256')
end
manifest = Dir[File.join(__dir__, '*')].sort.filter_map do |path|
  next unless File.file?(path) && File.basename(path) != 'manifest.json'
  [File.basename(path), { bytes: File.size(path), sha256: Digest::SHA256.file(path).hexdigest }]
end.to_h
File.write(File.join(__dir__, 'manifest.json'), JSON.pretty_generate(manifest) + "\n")
puts 'Method-sharing source ABBA, all five generated families, exact values/metadata/costs, JIT method reuse, bounded cache churn and zero-failure corpus/host evidence verified. JSON losslessly compacted.'
