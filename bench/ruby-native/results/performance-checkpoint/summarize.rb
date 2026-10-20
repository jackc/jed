# frozen_string_literal: true

# Read-only evidence verification, then lossless formatting and a final manifest.
require 'json'
require 'digest'
require 'time'
require_relative '../../source'
require_relative 'binding_evidence'
require_relative 'file_evidence'
require_relative 'blocking_evidence'
require_relative 'compact'
require_relative 'pin_sources'
require_relative '../literal-like-followup/correctness_summary'

module PerformanceCheckpointEvidence
  ROOT = File.expand_path('../../../..', __dir__)
  BASELINE = '3e749562c6b764681d6917604cf02aa73b3194cfdbb8c5e756616588de91dc88'
  RUNTIME_INFO = {
    '4.0-none' => { executable: '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', version: 'ruby 4.0.7 ', jit: 'none' },
    '4.0-yjit' => { executable: '/home/jack/.local/share/mise/installs/ruby/4.0.7/bin/ruby', version: 'ruby 4.0.7 ', jit: 'yjit' },
    '4.1-zjit' => { executable: File.join(ROOT, '.dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby'), version: 'ruby 4.1.0preview1 ', jit: 'zjit' }
  }.freeze
  module_function

  def read(name) = JSON.parse(File.read(File.join(__dir__, "#{name}.json")))
  def sha(path) = Digest::SHA256.file(path).hexdigest
  def median(values) = values.sort.fetch(values.length / 2)

  def named_hash(paths)
    digest = Digest::SHA256.new
    paths.sort.each { |path| digest << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path) }
    digest.hexdigest
  end

  def cross_memory(source, plan_path, plan)
    raw = read('cross-memory')
    raise 'cross-core source changed' unless raw.values_at('ruby_source_sha256', 'ruby_source_sha256_after') == [source, source] && raw['source_sha256'] == raw['source_sha256_after']
    raise 'cross-core plan differs' unless raw.values_at('plan_path', 'plan_sha256', 'plan') == [plan_path, sha(plan_path), plan]
    raise 'cross-core failed/lanes differ' unless raw.fetch('statuses').keys.sort == %w[go ruby-yjit rust ts] && raw.fetch('statuses').values.all? { |entry| entry['status'] == 'ready' }
    raise 'cross-core disabled a tier' if %w[ordering_disabled aggregate_disabled relation_disabled text_compilation_disabled verification_only].any? { |key| raw[key] }
    raise 'cross-core runtime differs' unless raw['ruby_executable'] == RUNTIME_INFO.fetch('4.0-yjit').fetch(:executable)
    sources = File.join(ROOT, 'bench/ruby-native/compare')
    paths = {
      # compare.rb is invoked by its task using a relative __FILE__; preserve
      # that mixed-path sort order when reconstructing its original digest.
      'harness' => ['bench/ruby-native/compare.rb', File.join(ROOT, 'bench/ruby-native/source.rb'), File.join(ROOT, 'impl/ruby-native/tools/bounded_process.rb')],
      'ruby_adapter' => [File.join(sources, 'lane.rb')],
      'go_adapter' => Dir[File.join(sources, 'go/*')].select { |path| File.file?(path) },
      'rust_adapter' => Dir[File.join(sources, 'rust/src/**/*.rs'), File.join(sources, 'rust/Cargo.*')],
      'ts_adapter' => [File.join(sources, 'lane.ts')],
      'go_core' => Dir[File.join(ROOT, 'impl/go/**/*.go')],
      'rust_core' => Dir[File.join(ROOT, 'impl/rust/src/**/*.rs')],
      'ts_core' => Dir[File.join(ROOT, 'impl/ts/src/**/*.ts')]
    }
    paths.each { |key, files| raise "cross-core source changed #{key}" unless raw.fetch('source_sha256').fetch(key) == named_hash(files) }
    raise 'cross-core Ruby binary changed' unless raw.fetch('source_sha256').fetch('ruby_executable') == sha(raw.fetch('ruby_executable'))
    %w[go rust].each do |lane|
      binary = raw.fetch('resources').fetch(lane).fetch('command').first
      raise 'cross-core measured binary changed' unless raw.fetch('source_sha256').fetch("#{lane}_binary") == sha(binary)
    end
    raw.fetch('resources').each do |lane, resource|
      raise 'cross-core bounds differ' unless resource.values_at('memory_limit_bytes', 'address_space_bytes') == [536870912, lane == 'ruby-yjit' ? 2 * 1024**3 : 16 * 1024**3]
      raise 'cross-core exceeded RSS bound' unless resource.fetch('peak_rss_bytes').between?(1, 536870912)
    end
    signatures = {}
    times = raw.fetch('results').transform_values.with_index do |jobs, _|
      raise 'cross-core job set differs' unless jobs.map { |job| job['workload'] } == plan.fetch('workloads').map { |job| job['name'] }
      jobs.to_h do |job|
        name = job.fetch('workload')
        spec = plan.fetch('workloads').find { |item| item['name'] == name }
        raise 'cross-core schedule differs' unless job.values_at('iterations', 'warmup') == spec.values_at('iterations', 'warmup') && job.fetch('samples_ns').length == 5
        signature = job.slice('checksum', 'validation_rows', 'rows_total', 'iterations', 'warmup')
        raise "cross-core rendered values/rows differ #{name}" if signatures[name] && signatures[name] != signature
        signatures[name] = signature
        [name, median(job.fetch('samples_ns')).fdiv(job.fetch('iterations')) / 1000.0]
      end
    end
    raw.fetch('results').fetch('ruby-yjit').each do |job|
      raise 'cross-core YJIT unavailable' unless job.fetch('runtime').start_with?('ruby 4.0.7 ') && job.fetch('runtime').include?('+YJIT')
    end
    { statistic: 'Single fresh process per core, median of five samples, microseconds per complete query.',
      validation_scope: 'Rendered-value checksums and row counts only. These adapters use their existing collectors, including Ruby Rows#rows; exact costs and column metadata are independently checked against the Rust Ruby package in the binding reports.',
      us: times, ruby_over: times.fetch('ruby-yjit').to_h do |name, ruby|
        [name, %w[go rust ts].to_h { |lane| [lane, ruby.fdiv(times.fetch(lane).fetch(name))] }]
      end, validation: signatures, resources: raw.fetch('resources') }
  end

  def profile(source, plan_path, plan)
    raw = read('final-profile')
    bound = read('final-profile-bounded')
    [raw, bound].each { |report| raise 'profile source changed' unless report.values_at('source_sha256', 'final_source_sha256') == [source, source] }
    raise 'profile worker failed/bounds differ' unless bound['success'] && bound['reason'].nil? && bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [536870912, 2147483648, 300.0]
    raise 'profile runtime differs' unless raw['ruby'].start_with?('ruby 4.0.7 ') && raw['ruby'].include?('+YJIT') && raw['ruby_executable'] == RUNTIME_INFO.fetch('4.0-yjit').fetch(:executable)
    raise 'profile plan/script changed' unless raw['plan_sha256'] == sha(plan_path) && raw['script_sha256'] == sha(File.join(ROOT, 'bench/ruby-native/query_profile.rb'))
    raise 'profile jobs differ' unless raw.fetch('results').map { |job| job['workload'] } == plan.fetch('workloads').map { |job| job['name'] }
    raw.fetch('results').to_h do |job|
      raise 'profile API/sample count differs' unless job['api'] == 'execute' && job.fetch('samples').length == 5
      [job.fetch('workload'), { allocations_per_query: median(job.fetch('samples').map { |sample| sample.fetch('allocations_per_query') }),
        diagnostic_us: median(job.fetch('samples').map { |sample| sample.fetch('ns_per_query') }) / 1000.0,
        calls_per_query: job.fetch('calls_per_query'), allocation_sites: job.fetch('allocation_sites') }]
    end
  end

  def main
    source = NativeRubySource.digest(File.join(ROOT, 'impl/ruby-native'))
    pins = read('source-pins')
    raise 'checkpoint benchmark source pins changed' unless pins.reject { |key, _| key == 'captured_at' } == CheckpointSourcePins.current
    prior = JSON.parse(File.read(File.join(__dir__, '../literal-like-followup/source.json')))
    raise 'checkpoint source differs from audited phase2' unless source == prior.fetch('final_source_sha256')
    correctness = LiteralLikeCorrectnessEvidence.build(source, sha(File.join(ROOT, 'impl/ruby-native/bin/conformance.rb')))
    plan_paths = %w[checkpoint-plan.json checkpoint-host-plan.json blocking-plan.json].to_h { |name| [name, File.join(ROOT, 'bench/ruby-native/compare', name)] }
    plan = JSON.parse(File.read(plan_paths.fetch('checkpoint-plan.json')))
    raise 'wrong memory checkpoint size' unless plan.fetch('workloads').length == 21
    provenance = correctness.fetch(:provenance).merge('baseline_commit' => 'fc12f42798420fabc91dd6ee1274b961647071f8',
      'baseline_source_sha256' => BASELINE, 'final_source_sha256' => source,
      'plans' => plan_paths.transform_values { |path| sha(path) },
      'benchmark_source_pins' => pins,
      'correctness_evidence' => '../literal-like-followup', 'correctness_manifest_sha256' => sha(File.join(__dir__, '../literal-like-followup/manifest.json')))
    File.write(File.join(__dir__, 'source.json'), JSON.pretty_generate(provenance) + "\n")
    arguments = { directory: __dir__, root: ROOT, baseline: BASELINE, final_source: source, runtime_info: RUNTIME_INFO }
    summary = { baseline_source_sha256: BASELINE, source_sha256: source, runtime_identities: RUNTIME_INFO,
      correctness: correctness.fetch(:summary), binding: CheckpointBindingEvidence.build(**arguments),
      canonical_file: CheckpointFileEvidence.build(**arguments), blocking: CheckpointBlockingEvidence.build(**arguments),
      cross_memory: cross_memory(source, plan_paths.fetch('checkpoint-plan.json'), plan),
      final_profile: profile(source, plan_paths.fetch('checkpoint-plan.json'), plan) }
    counts = { 'verify' => 5, 'memory-controls' => 4, 'host-controls' => 4, 'file-controls' => 4,
      'blocking-controls' => 4, 'characterize' => 8, 'binding' => 3, 'cross' => 2, 'profile' => 1 }
    binding_file_failed = read('binding-file-4.0-yjit').fetch('lanes').values.any? { |entry| !entry.fetch('success') }
    rust_file_failed = read('cross-file').fetch('statuses').fetch('rust').fetch('status') != 'ready'
    commands = []
    summary[:journals] = counts.to_h do |phase, count|
      journal = read("run-#{phase}")
      raise 'driver changed/phase differs' unless journal['script_sha256'] == sha(File.join(__dir__, 'run.rb')) && journal['phase'] == phase
      raise 'driver count/source differs' unless journal.fetch('commands').length == count && journal.fetch('options').fetch('final_hash') == source
      expected_plans = %w[memory host blocking].zip(plan_paths.values.map { |path| sha(path) }).to_h
      raise 'driver plans changed' unless journal.fetch('plans') == expected_plans
      journal.fetch('commands').each do |entry|
        expected = entry.fetch('name').end_with?('before') ? BASELINE : source
        permitted_failure = (binding_file_failed && phase == 'binding' && entry.fetch('name') == 'binding-file-4.0-yjit') ||
          (rust_file_failed && phase == 'cross' && entry.fetch('name') == 'cross-file')
        raise 'driver worker failed/source changed' unless entry['success'] == !permitted_failure && entry.values_at('source_sha256', 'final_source_sha256') == [expected, expected]
        raise 'driver bypassed explicit task' unless entry.fetch('command').take(2) == %w[mise run]
        commands << entry
      end
      [phase, { commands: count, successful: journal.fetch('commands').count { |entry| entry['success'] },
        failed: journal.fetch('commands').count { |entry| !entry['success'] } }]
    end
    if binding_file_failed
      failed = read('binding-file-4.0-yjit')
      retried = read('binding-file-4.0-yjit-retry')
      raise 'permission failure provenance differs' unless failed['source_sha256'] == failed['source_sha256_after'] && failed['source_sha256'] == retried['source_sha256']
      raise 'permission failure lane set differs' unless failed.fetch('lanes').keys.sort == %w[wrapper native-prepared native-query native-eager].sort
      failed.fetch('lanes').each do |lane, entry|
        raise 'unexpected failed file-binding attempt' unless !entry['success'] && entry['reason'] == 'worker exited 1' && entry.fetch('stderr').include?('Operation not permitted') && !entry.key?('result')
        raise 'failed attempt bounds differ' unless entry.values_at('memory_limit_bytes', 'address_space_bytes') == [536870912, 2147483648]
        raise 'failed attempt changed runtime' unless entry.fetch('command').first == RUNTIME_INFO.fetch('4.0-yjit').fetch(:executable) && entry.fetch('command').include?('--yjit')
      end
      retry_journal = read('run-binding-file-retry')
      raise 'retry script changed' unless retry_journal.fetch('script_sha256') == sha(File.join(__dir__, 'retry_binding_file.rb'))
      raise 'retry journal identity differs' unless retry_journal.values_at('phase', 'original_report', 'original_journal') == ['binding-file-retry', 'binding-file-4.0-yjit.json', 'run-binding.json'] && retry_journal.fetch('commands').length == 1
      retry_entry = retry_journal.fetch('commands').first
      raise 'permission retry failed/source changed' unless retry_entry['success'] && retry_entry.values_at('source_sha256', 'final_source_sha256') == [source, source] && retry_entry.fetch('command').take(2) == %w[mise run]
      commands << retry_entry
      summary[:permission_retry] = { reason: retry_journal.fetch('reason'), original_report: 'binding-file-4.0-yjit.json',
        successful_report: 'binding-file-4.0-yjit-retry.json', failed_lanes: failed.fetch('lanes').keys,
        note: 'All four initial workers failed opening their database before query timing. The successful memory and host binding measurements were retained; only the unchanged file worker was retried.' }
    end
    if rust_file_failed
      rust_retry = read('run-cross-file-retry')
      raise 'Rust retry script changed' unless rust_retry.fetch('script_sha256') == sha(File.join(__dir__, 'retry_cross_file.rb'))
      raise 'Rust retry journal differs' unless rust_retry.values_at('phase', 'original_report', 'original_journal') == ['cross-file-retry', 'cross-file.json', 'run-cross.json'] &&
        rust_retry.fetch('commands').map { |entry| entry.fetch('name') } == %w[cross-file-rust-prebuild cross-file-rust-retry]
      rust_retry.fetch('commands').each do |entry|
        raise 'Rust retry failed/source changed' unless entry['success'] && entry.values_at('source_sha256', 'final_source_sha256') == [source, source] && entry.fetch('command').take(2) == %w[mise run]
        commands << entry
      end
      prebuild = read('cross-file-rust-prebuild-bounded')
      raise 'Rust prebuild failed/source changed' unless prebuild['success'] && prebuild['reason'].nil? && prebuild.values_at('source_sha256', 'final_source_sha256') == [source, source]
      raise 'Rust prebuild limits differ' unless prebuild.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [1024**3, 16 * 1024**3, 600.0] && prebuild.fetch('peak_rss_bytes').between?(1, 1024**3)
      raise 'Rust prebuild was not offline' unless prebuild.fetch('command').take(4) == %w[cargo build --release --offline] && rust_retry.fetch('commands').first.fetch('environment').fetch('CARGO_BUILD_JOBS') == '1'
      summary[:rust_file_preparation_retry] = { reason: rust_retry.fetch('reason'),
        original_report: 'cross-file.json', successful_report: 'cross-file-rust-retry.json',
        build_peak_rss_bytes: prebuild.fetch('peak_rss_bytes'), build_memory_limit_bytes: prebuild.fetch('memory_bytes'),
        measurement_memory_limit_bytes: 536870912 }
    end
    ordered = commands.sort_by { |entry| Time.iso8601(entry.fetch('started_at')) }
    first_measured = ordered.reject { |entry| entry.fetch('name').start_with?('verify-') }.first
    raise 'source pins captured after timing began' unless Time.iso8601(pins.fetch('captured_at')) <= Time.iso8601(first_measured.fetch('started_at'))
    ordered.each_cons(2) { |left, right| raise 'worker phases overlapped' if Time.iso8601(left.fetch('finished_at')) > Time.iso8601(right.fetch('started_at')) }
    raise 'profile was not last' unless ordered.last.fetch('name') == 'final-profile'
    summary[:serial_commands_verified] = ordered.length
    File.write(File.join(__dir__, 'summary.json'), JSON.pretty_generate(summary) + "\n")
    ledger = CheckpointEvidenceJSON.run(__dir__)
    ledger.fetch('files').each do |name, entry|
      path = File.join(__dir__, name)
      raise 'compact evidence changed' unless File.size(path) == entry.fetch('compact_bytes') && sha(path) == entry.fetch('compact_file_sha256') && Digest::SHA256.hexdigest(JSON.generate(JSON.parse(File.read(path)))) == entry.fetch('parsed_json_sha256')
    end
    manifest = Dir[File.join(__dir__, '*')].sort.filter_map do |path|
      [File.basename(path), { bytes: File.size(path), sha256: sha(path) }] if File.file?(path) && File.basename(path) != 'manifest.json'
    end.to_h
    File.write(File.join(__dir__, 'manifest.json'), JSON.pretty_generate(manifest) + "\n")
    puts 'Checkpoint source/API values, costs, callbacks, canonical files, real spill, runtime bounds and serial phase evidence verified.'
  end
end

PerformanceCheckpointEvidence.main if $PROGRAM_NAME == __FILE__
