# frozen_string_literal: true

# Verify every retained measurement before deriving paired statistics and a
# lossless serialization ledger. This never executes a workload or a benchmark.
require 'json'
require 'digest'
require 'time'
require_relative '../../source'
require_relative '../performance-checkpoint/binding_evidence'
require_relative '../literal-like-followup/compact'
require_relative 'correctness_summary'
require_relative 'shared_regression_evidence'

module MultiwayPlanEvidence
  ROOT = File.expand_path('../../../..', __dir__)
  SOURCE = File.join(ROOT, 'impl/ruby-native')
  BASELINE = '730a4a03496f62feec03acb2355e9c5f2fc9dda69fa055e7d9595f7457bb5edf'
  BASELINE_COMMIT = 'a612935772c97075826bb98e34fa89d2ec9bce34'
  ARCHIVE = '/tmp/jed-multiway-plan-before/impl/ruby-native'
  PLAN = File.join(ROOT, 'bench/ruby-native/compare/multiway-plan.json')
  NATIVE = CheckpointBindingEvidence::NATIVE
  STATES = CheckpointBindingEvidence::STATES
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

  def contents_hash(paths)
    Digest::SHA256.hexdigest(paths.sort.map { |path| File.binread(path) }.join)
  end

  def retained_artifact(name, original_hash, semantic_hash = nil)
    path = File.join(__dir__, name)
    current = sha(path)
    unless current == original_hash
      entry = read('serialization').fetch('files').fetch(name)
      raise "retained artifact changed #{name}" unless entry.values_at('original_file_sha256', 'compact_file_sha256') == [original_hash, current]
    end
    return unless semantic_hash
    parsed = JSON.parse(File.read(path))
    raise "retained JSON values changed #{name}" unless Digest::SHA256.hexdigest(JSON.generate(parsed)) == semantic_hash
    parsed
  end

  def corrected_preflights
    provenance = read('preflight-provenance')
    raise 'preflight baseline differs' unless provenance.values_at('baseline_commit', 'baseline_source_sha256') == [BASELINE_COMMIT, BASELINE]
    raise 'preflight coverage differs' unless provenance.fetch('reports').keys == %w[initial cast]
    reports = provenance.fetch('reports').to_h do |state, entry|
      report = retained_artifact(entry.fetch('retained_report'), entry.fetch('report_original_sha256'), entry.fetch('report_semantic_sha256'))
      raise 'preflight plan snapshot changed' unless sha(File.join(__dir__, entry.fetch('plan_snapshot'))) == entry.fetch('plan_sha256')
      raise 'preflight measured sources changed' unless report.fetch('source_sha256') == report.fetch('source_sha256_after') &&
        report.fetch('source_sha256').values_at('native', 'selected_plan') == [BASELINE, entry.fetch('plan_sha256')]
      raise 'preflight errors changed' unless report.fetch('errors') == entry.fetch('errors') && report.fetch('errors').length == 3
      raise 'preflight lanes differ' unless report.fetch('lanes').keys == ['wrapper', *NATIVE]
      raise 'preflight was not verification only' unless report.fetch('options').fetch('verify_only')
      report.fetch('lanes').each_value do |lane|
        raise 'preflight limits differ' unless lane.values_at('memory_limit_bytes', 'address_space_bytes') == [536870912, 2147483648]
        raise 'preflight exceeded RSS bound' unless lane.fetch('peak_rss_bytes').between?(1, 536870912)
        next unless lane['result']
        raise 'preflight unexpectedly measured timings' unless lane.fetch('result').fetch('workloads').all? { |job| job.fetch('samples_ns').empty? }
      end
      [state, report]
    end
    initial, cast = reports.values_at('initial', 'cast')
    raise 'initial Rust preflight did not pass' unless initial.fetch('lanes').fetch('wrapper').fetch('success')
    NATIVE.each do |lane|
      entry = initial.fetch('lanes').fetch(lane)
      raise 'initial parameter defect differs' unless !entry.fetch('success') && entry.fetch('stderr').include?('42804: conflicting parameter types')
    end
    cast_jobs = cast.fetch('lanes').transform_values do |entry|
      raise 'cast preflight worker failed' unless entry.fetch('success')
      entry.fetch('result').fetch('workloads').find { |job| job.fetch('name') == 'three_table_computed_key' } || raise('missing cast preflight job')
    end
    expected = cast_jobs.fetch('wrapper')
    raise 'old Rust cast cost differs' unless expected.fetch('validation_costs') == [2_010_015] * 32
    NATIVE.each do |lane|
      job = cast_jobs.fetch(lane)
      fields = %w[answer_sha256 validation_parameters validation_rows validation_columns validation_column_types]
      raise 'cast preflight values/metadata differed' unless job.slice(*fields) == expected.slice(*fields)
      raise 'old native cast cost differs' unless job.fetch('validation_costs') == [6024] * 32
    end
    { reports: provenance.fetch('reports').transform_values { |entry| entry.slice('retained_report', 'plan_snapshot', 'errors') },
      note: 'These two failed untimed baseline preflights are retained separately from the successful formal benchmark and correctness runs. The final benchmark uses the independently correct modulo-key shape.' }
  end

  def cast_regression
    provenance = read('cast-gate-provenance')
    provenance.fetch('artifacts').each do |name, entry|
      retained_artifact(name, entry.fetch('original_sha256'), entry['semantic_sha256'])
    end
    corpus = provenance.fetch('corpus')
    raise 'cast regression corpus differs' unless corpus.fetch('path') == 'spec/conformance/suites/joins/safe_conjunct_pushdown.test' &&
      sha(File.join(__dir__, corpus.fetch('snapshot'))) == corpus.fetch('sha256') && sha(File.join(ROOT, corpus.fetch('path'))) == corpus.fetch('sha256')
    %w[probe go native].each do |kind|
      report = read("cast-gate-#{kind}-bounded")
      expected = provenance.fetch('commands').fetch(kind)
      raise 'cast regression worker failed/changed' unless report.fetch('success') && report.slice(*expected.keys) == expected
      raise 'cast regression RSS bound differs' unless report.fetch('memory_bytes') == 536870912 && report.fetch('peak_rss_bytes').between?(1, 536870912)
      raise 'cast regression AS/deadline differs' unless report.values_at('address_space_bytes', 'timeout_seconds') ==
        [kind == 'go' ? 16 * 1024**3 : 2 * 1024**3, kind == 'probe' ? 30.0 : 60.0]
      raise 'cast regression source changed during execution' unless report.fetch('source_sha256') == report.fetch('final_source_sha256')
    end
    go = provenance.fetch('go_binary')
    raise 'cast regression Go binary changed' unless sha(File.join(ROOT, go.fetch('path'))) == go.fetch('sha256')
    raise 'cast regression Go file failed' unless read('cast-gate-go-bounded').fetch('stdout').include?("PASS joins/safe_conjunct_pushdown.test\n") &&
      read('cast-gate-go-bounded').fetch('stdout').include?('[memory] 1 passed, 0 failed, 0 skipped')
    raw = read('cast-gate-probe')
    shape = { 'rows' => [[2, 200]], 'columns' => %w[id w], 'types' => %w[i32 i32] }
    raise 'cast comparison voices differ' unless raw.keys == %w[ruby-before rust]
    raw.each do |core, jobs|
      raise 'cast comparison cases differ' unless jobs.keys == ['identity parameter', 'identity column', 'literal cast', 'widening column']
      jobs.each do |name, job|
        cost = name == 'widening column' || (core == 'ruby-before' && name.start_with?('identity')) ? 37 : 46
        raise 'cast comparison result differs' unless job == shape.merge('cost' => cost)
      end
    end
    native = read('cast-gate-native')
    raise 'corrected Ruby cast file did not pass' unless native.values_at('files', 'records', 'passed', 'failed', 'query_api_records', 'execute_api_records') == [1, 30, 30, 0, 30, 0] && native.fetch('failures').empty?
    raise 'corrected Ruby cast resources/API differ' unless native.values_at('filter', 'query_api', 'query_memory_api', 'query_collection', 'worker_memory_bytes', 'worker_address_space_bytes', 'worker_timeout_seconds') ==
      ['joins/safe_conjunct_pushdown.test', 'query', 'query', 'external', 536870912, 2147483648, 10.0]
    raise 'corrected Ruby cast source differs' unless native.values_at('source_sha256', 'final_source_sha256') ==
      provenance.fetch('commands').fetch('native').values_at('source_sha256', 'final_source_sha256')
    { before_costs: raw.fetch('ruby-before').transform_values { |job| job.fetch('cost') },
      shared_costs: raw.fetch('rust').transform_values { |job| job.fetch('cost') }, records_passed: 30,
      native_source_sha256: native.fetch('source_sha256'), note: 'This targeted correction proof used an intermediate combined source. The final full query/execute audits supersede it.' }
  end

  def flags(runtime)
    jit = RUNTIME_INFO.fetch(runtime).fetch(:jit)
    jit == 'none' ? %w[--disable-yjit --disable-zjit] : ["--#{jit}"]
  end

  def configure_binding(plan)
    checker = CheckpointBindingEvidence
    { directory: __dir__, root: ROOT, runtime_info: RUNTIME_INFO, answers: {}, callbacks: {}, schedules: {}, peaks: [],
      plan_paths: { 'memory' => PLAN }, plans: { 'memory' => plan } }.each do |key, value|
      checker.instance_variable_set("@#{key}", value)
    end
    checker
  end

  def binding_report(checker, name, source, runtime, lanes, verify: false)
    report = checker.inspect_report(name, 'memory', source, runtime, lanes, verify: verify)
    options = report.fetch('options')
    selected = source == BASELINE ? ARCHIVE : SOURCE
    raise 'binding source path/output differs' unless options.values_at('native_source', 'output', 'lanes') ==
      [selected, File.join(__dir__, "#{name}.json"), lanes]
    raise 'binding selected unexpected workload/filter' if options['workloads'] || options['file'] || options['fresh_sql']
    raise 'binding disabled compiler tier' if options.any? { |key, value| key.start_with?('disable_') && value }
    provenance = report.fetch('source_sha256')
    wrapper = Dir[File.join(ROOT, 'impl/ruby/lib/**/*.rb'), File.join(ROOT, 'impl/ruby/ext/src/**/*.rs')]
    raise 'binding wrapper source changed' unless provenance.fetch('wrapper') == contents_hash(wrapper)
    raise 'binding Rust source changed' unless provenance.fetch('rust') == contents_hash(Dir[File.join(ROOT, 'impl/rust/src/**/*.rs')])
    provenance.fetch('plans').each do |file, hash|
      raise 'binding auxiliary plan changed' unless hash == sha(File.join(ROOT, 'bench/ruby-native/compare', file))
    end
    @benchmark_provenance ||= provenance.reject { |key, _| key == 'native' }
    raise 'binding nonnative inputs differ between runs' unless @benchmark_provenance == provenance.reject { |key, _| key == 'native' }
    @cpu ||= report.fetch('cpu')
    raise 'binding CPU differs between runs' unless @cpu == report.fetch('cpu')
    report.fetch('lanes').each do |lane, entry|
      command = [RUNTIME_INFO.fetch(runtime).fetch(:executable), *flags(runtime),
        File.join(ROOT, 'bench/ruby-native/binding_compare_lane.rb'), lane]
      command << '--verify-only' if verify
      command += ['--plan', PLAN]
      raise 'binding worker command differs' unless entry.fetch('command') == command
      raise 'binding worker exceeded RSS bound' unless entry.fetch('peak_rss_bytes').between?(1, 536870912)
      result = entry.fetch('result')
      result.fetch('workloads').each do |job|
        raise 'binding sample duration is invalid' unless job.fetch('samples_ns').all? { |value| value.is_a?(Numeric) && value.finite? && value.positive? }
        raise 'binding validation shape differs' unless job.fetch('validation_costs').length == job.fetch('validation_parameters').length &&
          job.fetch('validation_rows').length == job.fetch('validation_parameters').length
        raise 'binding timed rows differ among samples' unless verify || job.fetch('rows_per_sample').uniq.length == 1
      end
    end
    report
  end

  def bindings(source, plan)
    checker = configure_binding(plan)
    verification = %w[before final].to_h do |state|
      name = "verify-#{state}"
      binding_report(checker, name, state == 'before' ? BASELINE : source, '4.0-yjit', ['wrapper', *NATIVE], verify: true)
      [state, "#{name}.json"]
    end
    controls = RUNTIME_INFO.keys.to_h do |runtime|
      reports = STATES.map do |state|
        binding_report(checker, "control-#{runtime}-#{state}", state.end_with?('before') ? BASELINE : source, runtime, NATIVE)
      end
      medians = reports.map { |report| checker.medians(report) }
      [runtime, NATIVE.to_h do |api|
        [api, plan.fetch('workloads').to_h do |job|
          name = job.fetch('name')
          [name, checker.paired(medians.map { |run| run.fetch(api).fetch(name) })]
        end]
      end]
    end
    actual = binding_report(checker, 'binding-4.0-yjit', source, '4.0-yjit', ['wrapper', *NATIVE])
    medians = checker.medians(actual)
    binding = medians.fetch('wrapper').to_h do |name, wrapper|
      [name, { wrapper_us: wrapper / 1000.0, native: NATIVE.to_h do |api|
        value = medians.fetch(api).fetch(name)
        [api, { us: value / 1000.0, native_over_wrapper: value.fdiv(wrapper) }]
      end }]
    end
    { statistic: 'Each source ABBA quartet uses the mean of two process medians; each process median is five samples divided by iterations. Microseconds per complete query. The actual binding comparison is a separate fresh process per API.',
      controls: controls, actual_binding: binding, verification: verification,
      exact_values_metadata_costs: checker.instance_variable_get(:@answers),
      schedules: checker.instance_variable_get(:@schedules),
      callback_schedules: checker.instance_variable_get(:@callbacks),
      max_sampled_rss_bytes: checker.instance_variable_get(:@peaks).max }
  end

  def cross_memory(source, plan_path, plan)
    raw = read('cross-core')
    raise 'cross-core source changed' unless raw.values_at('ruby_source_sha256', 'ruby_source_sha256_after') == [source, source] && raw['source_sha256'] == raw['source_sha256_after']
    raise 'cross-core plan differs' unless raw.values_at('plan_path', 'plan_sha256', 'plan') == [plan_path, sha(plan_path), plan]
    raise 'cross-core failed/lanes differ' unless raw.fetch('statuses').keys.sort == %w[go ruby-yjit rust ts] && raw.fetch('statuses').values.all? { |entry| entry['status'] == 'ready' }
    raise 'cross-core result/resource lanes differ' unless raw.fetch('results').keys.sort == %w[go ruby-yjit rust ts] && raw.fetch('resources').keys.sort == %w[go ruby-yjit rust ts]
    raise 'cross-core source path/phase differs' unless raw.values_at('ruby_source', 'phase') == [SOURCE, 'multiway-plan-cross-core']
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
    tsv = '/tmp/jed-ruby-native-compare/plan.tsv'
    expected_commands = {
      'ruby-yjit' => [RUNTIME_INFO.fetch('4.0-yjit').fetch(:executable), '--yjit', File.join(sources, 'lane.rb'), tsv],
      'go' => ['/tmp/jed-ruby-native-compare/go-lane', tsv],
      'rust' => ['/tmp/jed-ruby-native-compare/rust-target/release/jed-ruby-experiment-compare', tsv],
      'ts' => ['node', File.join(sources, 'lane.ts'), tsv]
    }
    raw.fetch('resources').each do |lane, resource|
      raise 'cross-core command differs' unless resource.fetch('command') == expected_commands.fetch(lane)
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
        raise 'cross-core sample durations are invalid' unless job.fetch('samples_ns').all? { |value| value.is_a?(Numeric) && value.finite? && value.positive? }
        signature = job.slice('checksum', 'validation_rows', 'rows_total', 'iterations', 'warmup')
        raise "cross-core rendered values/rows differ #{name}" if signatures[name] && signatures[name] != signature
        signatures[name] = signature
        [name, median(job.fetch('samples_ns')).fdiv(job.fetch('iterations')) / 1000.0]
      end
    end
    raw.fetch('results').fetch('ruby-yjit').each do |job|
      raise 'cross-core YJIT unavailable' unless job.fetch('runtime').start_with?('ruby 4.0.7 ') && job.fetch('runtime').include?('+YJIT')
    end
    runtime_labels = raw.fetch('results').transform_values { |jobs| jobs.map { |job| job.fetch('runtime') }.uniq }
    raise 'cross-core runtime identities differ between jobs' unless runtime_labels.values.all? { |labels| labels.length == 1 }
    raise 'cross-core TS runtime differs from host' unless runtime_labels.fetch('ts').first == raw.fetch('host').fetch('node')
    raise 'cross-core Go runtime differs from host' unless raw.fetch('host').fetch('go').split[2] == runtime_labels.fetch('go').first
    raise 'cross-core Rust profile differs' unless runtime_labels.fetch('rust') == ['rust-release']
    raise 'cross-core CPU differs from binding runs' unless raw.fetch('host').fetch('cpu') == @cpu
    { statistic: 'Single fresh process per core, median of five samples, microseconds per complete query.',
      host: raw.fetch('host'), runtime_labels: runtime_labels.transform_values(&:first),
      validation_scope: 'Rendered-value checksums and row counts only. These adapters use their existing collectors, including Ruby Rows#rows; exact costs and column metadata are independently checked against the Rust Ruby package in the binding reports.',
      us: times, ruby_over: times.fetch('ruby-yjit').to_h do |name, ruby|
        [name, %w[go rust ts].to_h { |lane| [lane, ruby.fdiv(times.fetch(lane).fetch(name))] }]
      end, validation: signatures, resources: raw.fetch('resources') }
  end

  def profile(name, source, plan)
    raw, bound = read(name), read("#{name}-bounded")
    MultiwayPlanCorrectnessEvidence.bounded!(bound, source, timeout: 300.0)
    raise 'profile source changed' unless raw.values_at('source_sha256', 'final_source_sha256') == [source, source]
    selected = source == BASELINE ? ARCHIVE : SOURCE
    raise 'profile source path differs' unless raw.fetch('source') == selected && bound.fetch('source') == selected
    runtime = RUNTIME_INFO.fetch('4.0-yjit')
    raise 'profile runtime differs' unless raw.fetch('ruby').start_with?(runtime.fetch(:version)) && raw.fetch('ruby').include?('+YJIT') &&
      !raw.fetch('ruby').include?('+ZJIT') && raw.fetch('ruby_executable') == runtime.fetch(:executable)
    raise 'profile plan/script changed' unless raw.values_at('plan_path', 'plan_sha256', 'script_sha256') ==
      [PLAN, sha(PLAN), sha(File.join(ROOT, 'bench/ruby-native/query_profile.rb'))]
    artifact = File.join(__dir__, "#{name}.json")
    expected = [runtime.fetch(:executable), '--yjit', 'bench/ruby-native/query_profile.rb', '--api', 'execute', '--report', artifact]
    raise 'profile command differs' unless bound.fetch('command') == expected
    raise 'profile options differ' unless raw.fetch('options') == { 'api' => 'execute', 'trace' => 3, 'report' => artifact }
    raise 'profile jobs differ' unless raw.fetch('results').map { |job| job.fetch('workload') } == plan.fetch('workloads').map { |job| job.fetch('name') }
    raw.fetch('results').to_h do |job|
      name = job.fetch('workload')
      spec = plan.fetch('workloads').find { |item| item.fetch('name') == name }
      raise 'profile API/SQL/sample count differs' unless job.values_at('api', 'sql') == ['execute', spec.fetch('sql')] && job.fetch('samples').length == 5
      expected_rows = CheckpointBindingEvidence.instance_variable_get(:@schedules).fetch("memory/#{name}").fetch('rows_per_sample')
      raise 'profile consumed different row counts' unless job.fetch('samples').map { |sample| sample.fetch('rows') } == expected_rows
      raise 'profile counts are invalid' unless job.fetch('calls_per_query').values.all? { |value| value.is_a?(Numeric) && value.finite? && value >= 0 }
      samples = job.fetch('samples')
      raise 'profile samples are invalid' unless samples.all? { |sample| sample.fetch('ns_per_query').positive? && sample.fetch('allocations_per_query') >= 0 }
      [name, { allocations_per_query: median(samples.map { |sample| sample.fetch('allocations_per_query') }),
        diagnostic_us: median(samples.map { |sample| sample.fetch('ns_per_query') }) / 1000.0,
        calls_per_query: job.fetch('calls_per_query'), first_answer_sha256: job.fetch('first_answer_sha256'), allocation_sites: job.fetch('allocation_sites') }]
    end
  end

  def profiles(source, plan)
    before, final = profile('before-profile', BASELINE, plan), profile('final-profile', source, plan)
    comparisons = before.to_h do |name, old|
      current = final.fetch(name)
      raise 'profile first answer changed' unless old.fetch(:first_answer_sha256) == current.fetch(:first_answer_sha256)
      [name, { before_allocations: old.fetch(:allocations_per_query), final_allocations: current.fetch(:allocations_per_query),
        allocation_change_percent: (current.fetch(:allocations_per_query).fdiv(old.fetch(:allocations_per_query)) - 1) * 100,
        before_calls: old.fetch(:calls_per_query), final_calls: current.fetch(:calls_per_query) }]
    end
    { note: 'Diagnostic timing and allocations use the existing execute_prepared/eager Result#rows collector. All timing precedes TracePoint and ObjectSpace tracing; these timing observations are separate from the externally drained source ABBA measurements.',
      before: before, final: final, comparisons: comparisons }
  end

  def cache_state!(state, interning:)
    count = state.fetch('entries')
    raise 'diagnostic cache exceeded statement bound' unless count.between?(0, 64)
    { 'roots' => 128, 'key_units' => 4096, 'key_bytes' => 65_536 }.each do |key, maximum|
      values = state.fetch(key)
      raise "diagnostic cache #{key} differs" unless values.length == count && values.all? { |value| value.between?(0, maximum) }
    end
    raise 'diagnostic scope count differs' unless state.fetch('column_scopes').length == count && state.fetch('column_scopes').all? { |value| value >= 0 }
    raise 'diagnostic key interning availability differs' unless state.fetch('interning_available') == interning
    raise 'diagnostic retained mutable snapshots' unless state.fetch('snapshots_frozen') == [true] * count
    %w[interned_nodes interned_scopes].each do |key|
      values = state.fetch(key)
      valid = values.length == count && values.zip(state.fetch('roots')).all? { |value, roots|
        interning ? value.between?(roots.positive? ? 1 : 0, roots) : value.zero? }
      raise 'diagnostic interned key bounds differ' unless valid
    end
  end

  def diagnostics(source, plan)
    apis = %w[query_prepared query execute_prepared execute]
    targets = plan.fetch('workloads').map { |job| job.fetch('name') } - %w[scalar pk_hit three_table_left_fence]
    @semantic = nil
    reports = RUNTIME_INFO.keys.to_h do |runtime|
      identity = RUNTIME_INFO.fetch(runtime)
      [runtime, %w[before final].to_h do |state|
        name = "diagnostic-#{state}-#{runtime}"
        raw, bound = read(name), read("#{name}-bounded")
        expected_source = state == 'before' ? BASELINE : source
        MultiwayPlanCorrectnessEvidence.bounded!(bound, expected_source, timeout: 300.0)
        raise 'direct diagnostic source changed' unless raw.values_at('source_sha256', 'final_source_sha256') == [expected_source, expected_source]
        selected = state == 'before' ? ARCHIVE : SOURCE
        raise 'direct diagnostic source path differs' unless raw.fetch('source') == selected && bound.fetch('source') == selected
        command = [identity.fetch(:executable), *flags(runtime), 'bench/ruby-native/multiway_plan_diagnostics.rb',
          '--jit', identity.fetch(:jit), '--report', File.join(__dir__, "#{name}.json")]
        raise 'direct diagnostic command differs' unless bound.fetch('command') == command
        raise 'direct diagnostic did not finish' unless bound.fetch('stdout').include?('memory/file APIs, parameters, plans, budgets and cache observations passed')
        raise 'direct diagnostic identity differs' unless raw.fetch('mode') == 'untimed_multiway_resolution_diagnostic' &&
          raw.fetch('runtime').start_with?(identity.fetch(:version)) && raw.fetch('ruby_executable') == identity.fetch(:executable) &&
          raw.fetch('ruby_executable_sha256') == sha(identity.fetch(:executable)) &&
          raw.fetch('jit') == { 'yjit' => identity[:jit] == 'yjit', 'zjit' => identity[:jit] == 'zjit' }
        raise 'direct diagnostic script/plan changed' unless raw.fetch('script_sha256') == sha(File.join(ROOT, 'bench/ruby-native/multiway_plan_diagnostics.rb')) && raw.fetch('plan_sha256') == sha(PLAN)
        raise 'direct diagnostic availability/bounds differ' unless raw.fetch('compiler_available') &&
          raw.fetch('multiway_available') == (state == 'final') && raw.fetch('limits') == {
            'entries' => 64, 'roots' => 128, 'key_units' => 4096, 'key_bytes' => 65_536,
            'input_sources' => state == 'final' ? 8 : 2,
            'local_extensions' => state == 'final' ? 128 : 0 }
        storage = raw.fetch('storage')
        workspace = File.join(ROOT, '.dev/ruby-native-multiway-diagnostics')
        disk = storage.fetch('disk')
        raise 'direct fixture was not an actual private file' unless storage.fetch('memory') == { 'kind' => 'memory' } &&
          disk.values_at('kind', 'locking', 'removed_after_run') == ['file', 'exclusive', true] &&
          disk.fetch('path').start_with?(workspace + '/') && File.basename(disk.fetch('path')) == 'fixture.jed' &&
          disk.fetch('bytes').positive? && disk.fetch('sha256').match?(/\A[0-9a-f]{64}\z/) &&
          disk.fetch('filesystem_device') == File.stat(workspace).dev && !File.exist?(disk.fetch('path')) && !File.exist?(File.dirname(disk.fetch('path')))
        @diagnostic_file_signature ||= disk.values_at('bytes', 'sha256', 'filesystem_device')
        raise 'direct fixture bytes changed across sources/runtimes' unless @diagnostic_file_signature == disk.values_at('bytes', 'sha256', 'filesystem_device')
        semantic = raw.fetch('semantic')
        raise 'direct semantic digest differs' unless raw.fetch('semantic_sha256') == Digest::SHA256.hexdigest(JSON.generate(semantic))
        raise 'direct semantic results changed across sources/runtimes' if @semantic && @semantic != semantic
        @semantic ||= semantic
        raise 'direct storage modes differ' unless semantic.keys == %w[memory disk] && raw.fetch('observations').keys == %w[memory disk]
        semantic.each do |mode, cases|
          expected_cells = plan.fetch('workloads').flat_map { |job| apis.map { |api| [job.fetch('name'), api] } }
          raise 'direct workload/API coverage differs' unless cases.fetch('workloads').map { |job| job.values_at('workload', 'api') } == expected_cells
          cases.fetch('workloads').each do |job|
            values = job.fetch('workload') == 'three_table_join_range' ? [0, 1, 10, 90, 100, nil, 1] : [0, 3, 17, 999, 1000, nil, 0]
            raise 'direct parameter coverage differs' unless job.fetch('values').map { |item| item.fetch('params') } == values.map { |value| [value] }
          end
          budget_cells = %w[three_table_join three_table_join_range three_table_left_fence].flat_map { |name| apis.map { |api| [name, api] } }
          raise 'direct budget coverage differs' unless cases.fetch('budgets').map { |job| job.values_at('workload', 'api') } == budget_cells
          cases.fetch('budgets').each do |job|
            cost = job.fetch('ordinary').fetch('cost')
            ceilings = cost <= 64 ? (1..cost + 1).to_a : [1, 2, 5, 10, cost / 2, cost - 1, cost, cost + 1].uniq.sort
            expected = ceilings.flat_map { |limit| %w[max_cost lifetime_max_cost].map { |setting| [setting, limit] } }
            raise 'direct cost prefixes differ' unless job.fetch('ceilings').map { |item| item.values_at('setting', 'limit') } == expected && job.fetch('exhaustive_cost_prefixes') == (cost <= 64)
          end
          actions = [nil, 'ANALYZE js', 'UPDATE js SET bucket = 0 WHERE id = 36', 'CREATE INDEX js_id ON js(id)', 'DROP INDEX js_id']
          expected_skew = actions.flat_map { |action| [0, 36, 90, nil, 0].map { |value| [action, [value]] } }
          raise 'direct statistics/index coverage differs' unless cases.fetch('skew').map { |item| item.values_at('after', 'params') } == expected_skew
          observations = raw.fetch('observations').fetch(mode)
          rows = observations.fetch('workloads')
          expected_traces = cases.fetch('workloads').flat_map do |job|
            job.fetch('values').map { |value| [job.fetch('workload'), job.fetch('api'), value.fetch('params')] }
          end
          raise 'direct workload trace coverage differs' unless rows.map { |item| item.values_at('workload', 'api', 'params') } == expected_traces
          raise 'direct skew trace coverage differs' unless observations.fetch('skew').map { |item| item.values_at('after', 'params') } == expected_skew
          (rows + observations.fetch('skew')).each do |item|
            cache_state!(item.fetch('cache'), interning: state == 'final')
            trace = item.fetch('trace')
            raise 'diagnostic physical winner changed' unless trace.fetch('enabled').fetch('physical_plans') == trace.fetch('disabled').fetch('physical_plans')
            raise 'disabled diagnostic retained local extensions' unless trace.fetch('disabled').fetch('local_extension_entries_peak').zero?
            peak = trace.fetch('enabled').fetch('local_extension_entries_peak')
            raise 'diagnostic local extensions exceeded bound' unless peak.between?(0, state == 'final' ? 128 : 0)
            if item.key?('workload')
              admitted = state == 'final' ? targets.include?(item.fetch('workload')) : item.fetch('workload') == 'indexed_join'
              raise 'diagnostic target/fallback admission differs' unless item.fetch('cache').fetch('entries').positive? == admitted
            elsif state == 'before'
              raise 'baseline unexpectedly retained multiway skew cache' unless item.fetch('cache').fetch('entries').zero?
            end
          end
          rows.group_by { |item| item.values_at('workload', 'api') }.each do |(workload, _), entries|
            next unless state == 'final' ? targets.include?(workload) : workload == 'indexed_join'
            raise 'diagnostic target never reused its original entry' unless entries.any? { |item| item.fetch('original_entry_still_cached') }
          end
        end
        retention = raw.fetch('retention')
        if state == 'final'
          raise 'direct churn coverage differs' unless retention.values_at('available', 'variants') == [true, 96]
          cache_state!(retention.fetch('retained_before_close'), interning: true)
          raise 'direct churn did not reach statement bound' unless retention.fetch('retained_before_close').fetch('entries') == 64
          values = retention.fetch('live_entries_after_completed_thread_gc')
          raise 'direct GC observations differ' unless values.length == 3 && values.all? { |value| value.between?(0, 96) }
        else
          raise 'baseline unexpectedly ran cache churn' unless retention == { 'available' => false }
        end
        [state, { report: "#{name}.json", semantic_sha256: raw.fetch('semantic_sha256'), storage: storage, retention: retention,
          observations: raw.fetch('observations'), peak_rss_bytes: bound.fetch('peak_rss_bytes'), elapsed_seconds: bound.fetch('elapsed_seconds') }]
      end]
    end
    { note: 'Untimed source/runtime comparison of all four APIs, memory and file storage, exact outputs/metadata/cost/error prefixes, binding-sensitive winners and invalidation. Remaining weak references after GC are recorded as observed, without assuming the VM releases them.',
      semantic_sha256: Digest::SHA256.hexdigest(JSON.generate(@semantic)), runtimes: reports }
  end
  def expected_runs(source)
    runs = {}
    binding = lambda do |name, runtime, selected, lanes, verify|
      command = ['mise', 'run', 'ruby-native:binding-compare', '--', '--ruby', RUNTIME_INFO.fetch(runtime).fetch(:executable),
        '--jit', RUNTIME_INFO.fetch(runtime).fetch(:jit), '--native-source', selected, '--lanes', lanes.join(','),
        '--plan', PLAN, '--output', File.join(__dir__, "#{name}.json")]
      command << '--verify-only' if verify
      runs[name] = { command: command, source: selected, hash: selected == ARCHIVE ? BASELINE : source, environment: {} }
    end
    %w[before final].each do |state|
      binding.call("verify-#{state}", '4.0-yjit', state == 'before' ? ARCHIVE : SOURCE, ['wrapper', *NATIVE], true)
    end
    RUNTIME_INFO.each do |runtime, identity|
      STATES.each do |state|
        binding.call("control-#{runtime}-#{state}", runtime, state.end_with?('before') ? ARCHIVE : SOURCE, NATIVE, false)
      end
      %w[before final].each do |state|
        name = "diagnostic-#{state}-#{runtime}"
        selected = state == 'before' ? ARCHIVE : SOURCE
        runs[name] = { command: ['mise', 'run', 'ruby-native:bounded', '--', identity.fetch(:executable), *flags(runtime),
          'bench/ruby-native/multiway_plan_diagnostics.rb', '--jit', identity.fetch(:jit), '--report', File.join(__dir__, "#{name}.json")],
          source: selected, hash: state == 'before' ? BASELINE : source,
          environment: { 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") } }
      end
    end
    binding.call('binding-4.0-yjit', '4.0-yjit', SOURCE, ['wrapper', *NATIVE], false)
    runs['cross-core'] = { command: ['mise', 'run', 'ruby-native:compare'], source: SOURCE, hash: source,
      environment: { 'RUBY_NATIVE_LANES' => 'ruby-yjit,go,rust,ts', 'RUBY_NATIVE_PHASE' => 'multiway-plan-cross-core',
        'RUBY_NATIVE_COMPARE_PLAN' => PLAN, 'RUBY_NATIVE_COMPARE_TIMEOUT' => '300',
        'RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES' => '2147483648' } }
    %w[before final].each do |state|
      name = "#{state}-profile"
      runs[name] = { command: ['mise', 'run', 'ruby-native:bounded', '--', RUNTIME_INFO.fetch('4.0-yjit').fetch(:executable),
        '--yjit', 'bench/ruby-native/query_profile.rb', '--api', 'execute', '--report', File.join(__dir__, "#{name}.json")],
        source: state == 'before' ? ARCHIVE : SOURCE, hash: state == 'before' ? BASELINE : source,
        environment: { 'RUBY_NATIVE_COMPARE_PLAN' => PLAN, 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") } }
    end
    runs
  end

  def journals(source, pins)
    expected = expected_runs(source)
    commands, summaries = [], {}
    %w[verify diagnostics controls cross binding profiles].each do |phase|
      files = Dir[File.join(__dir__, "run-#{phase}*.json")].sort
      raise "missing phase journal #{phase}" if files.empty?
      files.each do |path|
        journal = JSON.parse(File.read(path))
        options = journal.fetch('options')
        raise 'driver changed/phase differs' unless journal.fetch('script_sha256') == sha(File.join(__dir__, 'run.rb')) &&
          journal.fetch('phase') == phase && options.fetch('phase') == phase && journal.fetch('plan_sha256') == sha(PLAN)
        raise 'driver selected wrong sources' unless options.values_at('final_hash', 'baseline_source') == [source, ARCHIVE]
        runtime = options['runtime']
        raise 'driver selected unknown runtime' if runtime && !RUNTIME_INFO.key?(runtime)
        runtimes = runtime ? [runtime] : RUNTIME_INFO.keys
        names = case phase
                when 'verify' then %w[verify-before verify-final]
                when 'controls' then runtimes.flat_map { |name| STATES.map { |state| "control-#{name}-#{state}" } }
                when 'diagnostics' then runtimes.flat_map { |name| %w[before final].map { |state| "diagnostic-#{state}-#{name}" } }
                when 'cross' then ['cross-core']
                when 'binding' then ['binding-4.0-yjit']
                when 'profiles' then %w[before-profile final-profile]
                end
        raise 'driver command coverage/order differs' unless journal.fetch('commands').map { |entry| entry.fetch('name') } == names
        journal.fetch('commands').each do |entry|
          requirement = expected.fetch(entry.fetch('name'))
          raise 'driver worker failed/source changed' unless entry.fetch('success') && entry.values_at('source_sha256', 'final_source_sha256') == [requirement.fetch(:hash)] * 2
          raise 'driver command/source path differs' unless entry.fetch('command') == requirement.fetch(:command) && entry.fetch('source') == requirement.fetch(:source)
          environment = {
            'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state', 'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache',
            'MISE_TRUSTED_CONFIG_PATHS' => ROOT, 'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
            'JED_NATIVE_MEMORY_BYTES' => '536870912', 'JED_NATIVE_ADDRESS_SPACE_BYTES' => '2147483648',
            'JED_NATIVE_TIMEOUT' => '300', 'JED_NATIVE_RUN_REPORT' => nil, 'RUBY_NATIVE_SOURCE' => requirement.fetch(:source)
          }.merge(requirement.fetch(:environment))
          raise 'driver environment differs' unless entry.fetch('environment') == environment
          raise 'driver time interval is reversed' if Time.iso8601(entry.fetch('started_at')) > Time.iso8601(entry.fetch('finished_at'))
          commands << entry
        end
        summaries[File.basename(path)] = { phase: phase, commands: names, all_successful: true }
      end
    end
    names = commands.map { |entry| entry.fetch('name') }
    raise 'duplicate or missing orchestrated command' unless names.length == expected.length && names.sort == expected.keys.sort
    ordered = commands.sort_by { |entry| Time.iso8601(entry.fetch('started_at')) }
    ordered.each_cons(2) do |left, right|
      raise 'orchestrated workers overlapped' if Time.iso8601(left.fetch('finished_at')) > Time.iso8601(right.fetch('started_at'))
    end
    timed = ordered.reject { |entry| entry.fetch('name').start_with?('verify-', 'diagnostic-') }
    first_timed = Time.iso8601(timed.first.fetch('started_at'))
    raise 'source pins captured after timing began' unless Time.iso8601(pins.fetch('captured_at')) <= first_timed
    ordered.select { |entry| entry.fetch('name').start_with?('verify-', 'diagnostic-') }.each do |entry|
      raise 'verification/diagnostics did not finish before timing' if Time.iso8601(entry.fetch('finished_at')) > first_timed
    end
    RUNTIME_INFO.keys.each do |runtime|
      observed = ordered.map { |entry| entry.fetch('name') }.grep(/\Acontrol-#{Regexp.escape(runtime)}-/)
      raise 'ABBA control order differs' unless observed == STATES.map { |state| "control-#{runtime}-#{state}" }
    end
    raise 'profiles were not last' unless ordered.last(2).map { |entry| entry.fetch('name') } == %w[before-profile final-profile]
    audit_journals = Dir[File.join(__dir__, '*-query-*-run.json'), File.join(__dir__, '*-execute-*-run.json')].sort
    raise 'corpus completion journals differ' unless audit_journals.length == 4
    audit_journals.each do |path|
      journal = JSON.parse(File.read(path))
      raise 'corpus audit did not finish before timing' unless journal.fetch('verified') && Time.iso8601(journal.fetch('finished_at')) <= first_timed
    end
    { journals: summaries, serial_commands_verified: ordered.length, timed_commands: timed.length,
      first_timed_started_at: timed.first.fetch('started_at'), last_command_finished_at: ordered.last.fetch('finished_at'),
      corpus_audits_completed_before_timing: audit_journals.map { |path| File.basename(path) },
      host_order_note: 'The host suite passed its bounded report checks. That report has no wall-clock start/end timestamps, so journal verification does not establish its ordering.' }
  end

  def main
    require_relative 'pin_sources'
    source = NativeRubySource.digest(SOURCE)
    baseline = read('baseline')
    raise 'baseline provenance differs' unless baseline.values_at('commit', 'source_sha256', 'archive') == [BASELINE_COMMIT, BASELINE, ARCHIVE]
    raise 'baseline archive changed' unless NativeRubySource.digest(ARCHIVE) == BASELINE
    pins = read('source-pins')
    raise 'benchmark source pins changed' unless pins.reject { |key, _| key == 'captured_at' } == MultiwayPlanSourcePins.current
    raise 'source pins differ from final source' unless pins.fetch('source_sha256') == source
    plan = JSON.parse(File.read(PLAN))
    raise 'multiway plan fixture/schedule size differs' unless plan.values_at('rows', 'repetitions', 'validation_iterations') == [1000, 5, 32] && plan.fetch('workloads').length == 9
    correctness = MultiwayPlanCorrectnessEvidence.build(source, sha(File.join(ROOT, 'impl/ruby-native/bin/conformance.rb')))
    binding = bindings(source, plan)
    cross = cross_memory(source, PLAN, plan)
    cross.fetch(:validation).each do |name, signature|
      exact = binding.fetch(:exact_values_metadata_costs).fetch("memory/#{name}")
      schedule = binding.fetch(:schedules).fetch("memory/#{name}")
      raise 'cross-core/exact binding row consumption differs' unless signature.fetch('validation_rows') == exact.fetch('validation_rows').sum &&
        signature.fetch('rows_total') == schedule.fetch('rows_per_sample').sum
    end
    summary = { baseline_source_sha256: BASELINE, source_sha256: source, runtime_identities: RUNTIME_INFO,
      corrected_preflights: corrected_preflights, cast_regression: cast_regression,
      shared_regressions: MultiwaySharedRegressionEvidence.build(source),
      correctness: correctness.fetch(:summary), binding: binding, cross_memory: cross,
      profiles: profiles(source, plan), diagnostics: diagnostics(source, plan), execution: journals(source, pins) }
    provenance = correctness.fetch(:provenance).merge('baseline_commit' => BASELINE_COMMIT,
      'baseline_source_sha256' => BASELINE, 'final_source_sha256' => source, 'benchmark_source_pins' => pins,
      'plan_path' => PLAN, 'plan_sha256' => sha(PLAN),
      'verifier_dependencies' => %w[../performance-checkpoint/binding_evidence.rb ../literal-like-followup/compact.rb correctness_summary.rb shared_regression_evidence.rb run_regressions.rb pin_sources.rb].to_h do |path|
        [path, sha(File.expand_path(path, __dir__))]
      end)
    correctness.each do |name, value|
      File.write(File.join(__dir__, "correctness-#{name}.json"), JSON.pretty_generate(value) + "\n")
    end
    File.write(File.join(__dir__, 'source.json'), JSON.pretty_generate(provenance) + "\n")
    File.write(File.join(__dir__, 'summary.json'), JSON.pretty_generate(summary) + "\n")
    ledger = LiteralLikeEvidenceJSON.run(__dir__)
    ledger.fetch('files').each do |name, entry|
      path = File.join(__dir__, name)
      raise 'compact evidence changed' unless File.size(path) == entry.fetch('compact_bytes') && sha(path) == entry.fetch('compact_file_sha256') &&
        Digest::SHA256.hexdigest(JSON.generate(JSON.parse(File.read(path)))) == entry.fetch('parsed_json_sha256')
    end
    manifest = Dir[File.join(__dir__, '*')].sort.filter_map do |path|
      [File.basename(path), { bytes: File.size(path), sha256: sha(path) }] if File.file?(path) && File.basename(path) != 'manifest.json'
    end.to_h
    File.write(File.join(__dir__, 'manifest.json'), JSON.pretty_generate(manifest) + "\n")
    puts "Multiway-plan source/API values, costs, binding-sensitive planning, bounds and #{summary.fetch(:execution).fetch(:serial_commands_verified)} serial commands verified."
  end
end

MultiwayPlanEvidence.main if $PROGRAM_NAME == __FILE__
