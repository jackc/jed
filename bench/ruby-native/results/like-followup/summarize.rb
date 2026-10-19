# frozen_string_literal: true

# Run only after every measured/verification artifact is complete. This checks
# retained evidence before writing the derived summary and complete manifest.
require 'json'
require 'digest'
require_relative '../../source'
require_relative 'diagnostic_summary'
require_relative 'compact'
require_relative 'correctness_summary'

BASELINE = '7724a4418421e56948d036dc04e54008df97b19d2609cf8b65a7ca0f63a2fe02'
RUNTIMES = %w[4.0-none 4.0-yjit 4.1-zjit].freeze
NATIVE_LANES = %w[native-prepared native-query native-eager].freeze
STATES = %w[1-before 2-final 3-final 4-before].freeze
ROOT = File.expand_path('../../../..', __dir__)
PLAN_PATH = File.join(ROOT, 'bench/ruby-native/compare/like-plan.json')
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
  raise 'plan changed' unless provenance.fetch('plans').fetch('like-plan.json') == PLAN_HASH
  raise 'worker changed' unless provenance.fetch('worker') == Digest::SHA256.file(File.join(ROOT, 'bench/ruby-native/binding_compare_lane.rb')).hexdigest
  raise 'wrong lanes' unless report.fetch('lanes').keys.sort == lanes.sort
  report.fetch('lanes').each do |lane, entry|
    raise "lane failed: #{filename} #{lane}" unless entry['success'] && entry['reason'].nil?
    raise 'lane resource limits differ' unless entry.values_at('memory_limit_bytes', 'address_space_bytes') == [536870912, 2147483648]
    result = entry.fetch('result')
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

summary = { statistic: 'Mean of two per-process medians per ABBA state; microseconds per complete query. Raw samples and paired medians remain available; this is not a confidence interval.',
  baseline_source_sha256: BASELINE, runtime_identities: RUNTIME_INFO, controls: {}, binding: {}, cross_core: {}, validation: {} }
answers, schedules = {}, {}
source = read('control-4.0-none-2-final.json').fetch('source_sha256').fetch('native')
raise 'current source differs from final measured source' unless NativeRubySource.digest(File.join(ROOT, 'impl/ruby-native')) == source
summary[:source_sha256] = source
raise 'wrong plan size/fixture' unless JOBS.length == 9 && PLAN.values_at('rows', 'repetitions', 'validation_iterations') == [1000, 5, 32]
correctness = LikeCorrectnessEvidence.build(source, Digest::SHA256.file(File.join(ROOT, 'impl/ruby-native/bin/conformance.rb')).hexdigest)
provenance = correctness.fetch(:provenance).merge('baseline_commit' => '96b2f163f16085383f0c89a3dd18b244d6b7cf3c',
  'baseline_source_sha256' => BASELINE, 'final_source_sha256' => source, 'plan_sha256' => PLAN_HASH)
File.write(File.join(__dir__, 'source.json'), JSON.pretty_generate(provenance) + "\n")
summary[:validation][:source_provenance] = provenance
summary[:validation][:correctness] = correctness.fetch(:summary)

RUNTIMES.each do |runtime|
  runs = STATES.map do |state|
    binding_report!("control-#{runtime}-#{state}.json", state.end_with?('before') ? BASELINE : source, NATIVE_LANES, false, answers, schedules)
  end
  summary[:controls][runtime] = NATIVE_LANES.to_h do |lane|
    [lane, JOBS.keys.to_h do |name|
      medians = runs.map { |run| run.fetch('lanes').fetch(lane).fetch('result').fetch('workloads').find { |job| job['name'] == name }.fetch('median_ns') }
      before, final = medians.values_at(0, 3), medians.values_at(1, 2)
      [name, { before_us: before.sum / 2000.0, final_us: final.sum / 2000.0,
        speedup: before.sum.fdiv(final.sum), before_medians_ns: before, final_medians_ns: final,
        final_medians_below_both_before: final.max < before.min,
        final_medians_above_both_before: final.min > before.max }]
    end]
  end
end
summary[:isolated_scalar_control] = %w[4.0-none 4.0-yjit].to_h do |runtime|
  isolated = STATES.map do |state|
    binding_report!("scalar-control-#{runtime}-#{state}.json", state.end_with?('before') ? BASELINE : source,
      NATIVE_LANES, false, answers, schedules, names: ['scalar'])
  end
  [runtime, NATIVE_LANES.to_h do |lane|
    medians = isolated.map { |report| report.fetch('lanes').fetch(lane).fetch('result').fetch('workloads').first.fetch('median_ns') }
    before, final = medians.values_at(0, 3), medians.values_at(1, 2)
    [lane, { before_us: before.sum / 2000.0, final_us: final.sum / 2000.0,
      speedup: before.sum.fdiv(final.sum), before_medians_ns: before, final_medians_ns: final,
      final_medians_below_both_before: final.max < before.min,
      final_medians_above_both_before: final.min > before.max }]
  end]
end
scalar_programs = %w[before final].to_h do |state|
  expected = state == 'before' ? BASELINE : source
  report = read("scalar-source-#{state}.json")
  bound = read("scalar-source-#{state}-bounded.json")
  bounded!(bound, expected, timeout: 120.0)
  source_matches!(report, expected)
  raise 'scalar source proof script changed' unless report.fetch('script_sha256') == Digest::SHA256.file(File.join(__dir__, 'scalar_source.rb')).hexdigest
  raise 'scalar source proof stdout differs' unless JSON.parse(bound.fetch('stdout')) == report
  raise 'scalar source proof runtime differs' unless report.fetch('ruby').start_with?('ruby 4.0.7 ') && report.fetch('executable') == RUNTIME_INFO.fetch('4.0-none').fetch(:executable)
  raise 'scalar source hash differs' unless Digest::SHA256.hexdigest(report.fetch('generated_source')) == report.fetch('generated_source_sha256')
  [state, report.slice('sql', 'type', 'generated_source', 'generated_source_sha256', 'pool', 'outcomes')]
end
raise 'scalar generated source/pool or behavior changed' unless scalar_programs.fetch('before') == scalar_programs.fetch('final')
summary[:scalar_control_program] = scalar_programs.fetch('final').reject { |key, _| key == 'generated_source' }.merge('identical_across_sources' => true)
binding = binding_report!('binding-4.0-yjit.json', source, ['wrapper', *NATIVE_LANES], false, answers, schedules)
summary[:binding] = binding.fetch('lanes').to_h do |lane, entry|
  [lane, entry.fetch('result').fetch('workloads').to_h { |job| [job.fetch('name'), job.fetch('median_ns') / 1000.0] }]
end
summary[:validation][:matching_values_metadata_costs] = answers
summary[:validation][:schedules] = schedules

profiles = %w[before final].to_h do |state|
  profile = read("#{state}-profile.json")
  expected = state == 'before' ? BASELINE : source
  source_matches!(profile, expected)
  runtime_matches!('4.0-yjit', profile.fetch('ruby'), profile.fetch('ruby_executable'), 'yjit')
  raise 'profile did not run YJIT' unless profile.fetch('ruby').include?('+YJIT')
  bounded!(read("#{state}-profile-bounded.json"), expected)
  raise 'profile plan changed' unless profile.fetch('plan_sha256') == PLAN_HASH
  raise 'profile script changed' unless profile.fetch('script_sha256') == Digest::SHA256.file(File.join(ROOT, 'bench/ruby-native/query_profile.rb')).hexdigest
  raise 'profile jobs differ' unless profile.fetch('results').map { |job| job.fetch('workload') }.sort == JOBS.keys.sort
  [state, profile.fetch('results').to_h do |job|
    raise 'wrong profile API/sample count' unless job['api'] == 'execute' && job.fetch('samples').length == 5
    [job.fetch('workload'), { allocations_per_query: median(job.fetch('samples').map { |sample| sample.fetch('allocations_per_query') }),
      generic_like_calls: job.fetch('calls_per_query').fetch('#<Class:JedNative::Functions>#like', 0),
      collated_text_initializers: job.fetch('calls_per_query').fetch('JedNative::CollatedText#initialize', 0),
      traced_collated_text_allocations: job.fetch('allocation_sites').sum { |site| site.fetch('class') == 'JedNative::CollatedText' ? site.fetch('count') : 0 },
      answer_sha256: job.fetch('first_answer_sha256') }]
  end]
end
summary[:allocations] = profiles.fetch('before').to_h do |name, before|
  after = profiles.fetch('final').fetch(name)
  raise 'profile answer changed' unless before[:answer_sha256] == after[:answer_sha256]
  if !%w[scalar like_complex_fallback].include?(name)
    raise 'target still invokes generic LIKE' unless before[:generic_like_calls].positive? && after[:generic_like_calls].zero?
  end
  [name, { before: before[:allocations_per_query], final: after[:allocations_per_query],
    generic_like_calls: { before: before[:generic_like_calls], final: after[:generic_like_calls] },
    collated_text_initializers: { before: before[:collated_text_initializers], final: after[:collated_text_initializers] },
    traced_collated_text_allocations: { before: before[:traced_collated_text_allocations], final: after[:traced_collated_text_allocations] } }]
end

cross = read('cross-core.json')
raise 'cross-core source changed' unless cross['ruby_source_sha256'] == source && cross['ruby_source_sha256_after'] == source && cross['source_sha256'] == cross['source_sha256_after']
raise 'cross-core plan differs' unless cross['plan_sha256'] == PLAN_HASH && cross['plan'] == PLAN
raise 'cross-core failure or lanes differ' unless cross.fetch('statuses').keys.sort == %w[go ruby-yjit rust ts] && cross.fetch('statuses').values.all? { |entry| entry.fetch('status') == 'ready' }
raise 'cross-core unexpectedly disabled compilation' if cross['text_compilation_disabled']
raise 'cross-core Ruby runtime differs' unless cross.fetch('ruby_executable') == RUNTIME_INFO.fetch('4.0-yjit').fetch(:executable)
checksums = {}
summary[:cross_core] = cross.fetch('results').to_h do |lane, jobs|
  raise 'cross-core jobs differ' unless jobs.map { |job| job.fetch('workload') }.sort == JOBS.keys.sort
  [lane, jobs.to_h do |job|
    name = job.fetch('workload')
    if lane == 'ruby-yjit'
      raise 'cross-core Ruby JIT differs' unless job.fetch('runtime').start_with?('ruby 4.0.7 ') && job.fetch('runtime').include?('+YJIT')
    end
    expected = JOBS.fetch(name)
    raise 'cross-core schedule differs' unless job.values_at('warmup', 'iterations') == expected.values_at('warmup', 'iterations') && job.fetch('samples_ns').length == 5
    signature = job.values_at('checksum', 'validation_rows', 'rows_total', 'iterations', 'warmup')
    raise 'cross-core values/consumption differ' if checksums[name] && checksums[name] != signature
    checksums[name] = signature
    [name, median(job.fetch('samples_ns')).fdiv(job.fetch('iterations')) / 1000.0]
  end]
end
summary[:validation][:cross_core_values_and_rows] = checksums

diagnostics = LikeDiagnosticSummary.build(__dir__, ROOT, BASELINE, source, RUNTIME_INFO)
summary[:diagnostics] = { summary: 'diagnostic-summary.json', all_seven_methods_directly_jited_in_each_jit_run: true,
  compiler_limits: diagnostics.fetch(:compiler_limits),
  after_thread_exit_live_kernels: diagnostics.fetch(:runtimes).transform_values do |runtime|
    runtime.fetch(:churn).fetch('after_completed_thread_exit_and_gc').map { |snapshot| snapshot.fetch('live_kernels') }
  end,
  caveat: 'Engine cache bounds do not bound VM-retained classes or machine code; every churn observation is preserved.' }

counts = { 'controls' => 12, 'profiles' => 2, 'cross' => 1, 'binding' => 1, 'diagnostics' => 3, 'admission' => 1, 'scalar-controls' => 8 }
summary[:validation][:run_journals] = counts.to_h do |phase, count|
  journal = read("run-#{phase}.json")
  raise 'runner changed' unless journal.fetch('script_sha256') == Digest::SHA256.file(File.join(__dir__, 'run.rb')).hexdigest
  raise 'runner plan/source options differ' unless journal.fetch('plan_sha256') == PLAN_HASH && journal.fetch('options').fetch('final_hash') == source
  raise 'runner phase/count differs' unless journal.fetch('phase') == phase && journal.fetch('commands').length == count
  if phase == 'scalar-controls'
    raise 'isolated scalar driver changed' unless journal.fetch('options').fetch('isolation_driver_sha256') == Digest::SHA256.file(File.join(__dir__, 'isolated_scalar.rb')).hexdigest
    raise 'isolated scalar workload differs' unless journal.fetch('commands').all? do |entry|
      command = entry.fetch('command')
      command.fetch(command.index('--workloads') + 1) == 'scalar'
    end
  end
  journal.fetch('commands').each do |entry|
    expected_source = (entry.fetch('name').include?('before') || entry.fetch('name') == 'baseline-admission') ? BASELINE : source
    raise 'runner job failed or source changed' unless entry.fetch('success') && entry.values_at('source_sha256', 'final_source_sha256') == [expected_source, expected_source]
    raise 'runner did not invoke an explicit mise task' unless entry.fetch('command').take(2) == %w[mise run]
  end
  [phase, { commands: count, all_successful: true, runner_sha256: journal.fetch('script_sha256') }]
end

File.write(File.join(__dir__, 'summary.json'), JSON.pretty_generate(summary) + "\n")
ledger = LikeEvidenceJSON.run(__dir__)
ledger.fetch('files').each do |name, record|
  path = File.join(__dir__, name)
  raise 'compacted bytes differ' unless File.size(path) == record.fetch('compact_bytes') && Digest::SHA256.file(path).hexdigest == record.fetch('compact_file_sha256')
  canonical = JSON.generate(JSON.parse(File.read(path)))
  raise 'compacted JSON semantic hash differs' unless Digest::SHA256.hexdigest(canonical) == record.fetch('parsed_json_sha256')
end
manifest = Dir[File.join(__dir__, '*')].sort.filter_map do |path|
  next unless File.file?(path) && File.basename(path) != 'manifest.json'
  [File.basename(path), { bytes: File.size(path), sha256: Digest::SHA256.file(path).hexdigest }]
end.to_h
File.write(File.join(__dir__, 'manifest.json'), JSON.pretty_generate(manifest) + "\n")
puts 'Literal LIKE source ABBA, cross-core/binding values, exact metadata/costs, zero-failure corpus/host audits, allocations and direct JIT evidence verified; all parsed JSON values preserved in compact evidence.'
