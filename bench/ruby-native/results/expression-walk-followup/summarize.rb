# frozen_string_literal: true

# Read-only verification precedes derived output and lossless JSON compaction.
# Run only after every correctness, benchmark and profile process has exited.
require 'json'
require 'digest'
require 'time'
require 'pathname'
require_relative 'run'
require_relative 'check'
require_relative '../../expression_walk_diagnostics'
require_relative '../literal-like-followup/compact'

module ExpressionWalkEvidence
  ROOT = ExpressionWalkRuns::ROOT
  SOURCE = File.join(ROOT, 'impl/ruby-native')
  ARCHIVE = '/tmp/jed-expression-walk-before/impl/ruby-native'
  BASELINE = ExpressionWalkRuns::BASELINE
  PLAN = ExpressionWalkRuns::PLAN
  NATIVE = %w[native-prepared native-query native-eager].freeze
  STATES = %w[1-before 2-final 3-final 4-before].freeze
  module_function

  def read(name) = JSON.parse(File.read(File.join(__dir__, "#{name}.json")))
  def sha(path) = Digest::SHA256.file(path).hexdigest
  def median(values) = values.sort.fetch(values.length / 2)
  def write(name, value) = File.write(File.join(__dir__, "#{name}.json"), JSON.generate(value) + "\n")
  def assert(value, message)
    raise message unless value
  end

  def contents_hash(files)
    Digest::SHA256.hexdigest(files.sort.map { |path| File.binread(path) }.join)
  end

  def named_hash(files)
    value = Digest::SHA256.new
    files.sort.each { |path| value << path.delete_prefix(ROOT + '/') << "\0" << File.binread(path) }
    value.hexdigest
  end

  def runtime(name)
    binary, jit = ExpressionWalkRuns::RUNTIMES.fetch(name)
    { binary: binary, jit: jit, version: name == '4.1-zjit' ? 'ruby 4.1.0preview1 ' : 'ruby 4.0.7 ' }
  end

  def bounded!(report, source, command)
    assert(report['success'] && report['reason'].nil?, 'bounded worker failed')
    assert(report.values_at('source_sha256', 'final_source_sha256') == [source] * 2, 'bounded source changed')
    assert(report.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
      [536_870_912, 2_147_483_648, 300.0], 'bounded limits differ')
    assert(report['command'] == command, 'bounded command differs')
    assert(report.fetch('peak_rss_bytes').between?(1, 536_870_912), 'bounded RSS exceeded')
  end

  def pins!
    pins = read('source-pins')
    assert(pins.values_at('baseline_commit', 'baseline_source', 'baseline_source_sha256') ==
      [ExpressionWalkRuns::BASELINE_COMMIT, ARCHIVE, BASELINE], 'baseline pins differ')
    assert(NativeRubySource.digest(ARCHIVE) == BASELINE, 'baseline archive changed')
    assert(pins.fetch('source_sha256') == @source, 'final source differs from pins')
    expected = ExpressionWalkRuns::PINNED_FILES + Dir.chdir(ROOT) do
      Dir['bench/ruby-native/compare/go/*', 'bench/ruby-native/compare/rust/src/**/*.rs',
        'bench/ruby-native/compare/rust/Cargo.*', 'bench/ruby-native/compare/lane.rb',
        'bench/ruby-native/compare/lane.ts']
    end
    assert(pins.fetch('files').keys.sort == expected.uniq.sort, 'pin file set differs')
    pins.fetch('files').each { |path, hash| assert(sha(File.join(ROOT, path)) == hash, "pinned file changed #{path}") }
    binaries = pins.fetch('runtime_files')
    ruby = ExpressionWalkRuns::RUNTIMES.values.map(&:first).uniq
    assert(binaries.length == 3 && (ruby - binaries.keys).empty? &&
      (binaries.keys - ruby).all? { |path| File.basename(path) == 'node' }, 'runtime pin set differs')
    binaries.each { |path, hash| assert(sha(path) == hash, "runtime changed #{path}") }
    pins
  end

  def correctness
    current = ExpressionWalkChecks.provenance
    specifications = ExpressionWalkChecks.specifications(phase: 'focus') +
      ExpressionWalkChecks.specifications(phase: 'host') + ExpressionWalkChecks.specifications(phase: 'corpus')
    reports = specifications.to_h do |spec|
      name = spec.fetch(:name)
      journal, bound = read("#{name}-run"), read("#{name}-bounded")
      # The green focus runs predate the host-only AS adjustment. Preserve and
      # verify their exact checker instead of relabeling or repeating them.
      earlier_focus = spec[:phase] == 'focus' &&
        journal.dig('source_before', 'runner_sha256') != current['runner_sha256']
      pinned = earlier_focus ?
        current.merge('runner_sha256' => sha(File.join(__dir__, 'host-address-space/initial-check.rb'))) : current
      assert(journal.values_at('source_before', 'source_after') == [pinned] * 2, 'correctness provenance changed')
      assert(journal['command_success'] && journal['verified'], 'correctness driver failed')
      assert(journal['specification'] == JSON.parse(JSON.generate(spec)), 'correctness specification differs')
      assert(journal['command'] == spec.fetch(:command), 'correctness command differs')
      env = ExpressionWalkChecks.environment(spec, File.join(__dir__, "#{name}-bounded.json"), File.join(__dir__, "#{name}.json"))
      assert(journal['environment'] == env, 'correctness environment differs')
      assert(journal['ruby_executable_sha256'] == sha(spec.fetch(:binary)), 'correctness executable changed')
      report = spec[:phase] == 'corpus' ? read(name) : nil
      ExpressionWalkChecks.verify!(spec, bound, report, current)
      [name, { passed: report && report.fetch('passed'), peak_rss_bytes: bound.fetch('peak_rss_bytes'),
        started_at: journal.fetch('started_at'), finished_at: journal.fetch('finished_at') }]
    end
    reports.values.sort_by { |report| Time.iso8601(report[:started_at]) }.each_cons(2) do |left, right|
      assert(Time.iso8601(left[:finished_at]) <= Time.iso8601(right[:started_at]),
        'correctness workers overlapped')
    end
    supervisor_path = File.join(__dir__, 'correctness-supervisor.json')
    if File.file?(supervisor_path)
      supervisor = read('correctness-supervisor')
      assert(supervisor['success'] && supervisor['reason'].nil? &&
        supervisor.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
        supervisor.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
          [536_870_912, 2_147_483_648, 1800.0] &&
        supervisor['peak_rss_bytes'].between?(1, 536_870_912), 'final correctness supervisor failed/differed')
    end
    { records: reports.values.sum { |value| value[:passed].to_i }, failed: 0,
      host_files: current.fetch('host_files'), corpus_files: current.fetch('corpus_files'),
      max_rss_bytes: reports.values.map { |value| value[:peak_rss_bytes] }.max,
      reports: reports, provenance: current }
  end

  def host_address_space(current)
    directory = File.join(__dir__, 'host-address-space')
    return { present: false } unless Dir.exist?(directory)
    original = File.read(File.join(directory, 'initial-check.rb'))
    selected = File.read(File.join(__dir__, 'check.rb'))
    # Permit precisely the host virtual-address allowance change. Ignoring
    # comment lines accounts for the three explanatory comments added with it.
    normalize = ->(text) { text.lines.reject { |line| line.lstrip.start_with?('#') }.join }
    revised = original.sub("timeout: 180, address_space: 1_073_741_824 }]\n    when 'corpus'",
      "timeout: 180, address_space: 2_147_483_648 }]\n    when 'corpus'")
    assert(revised != original && normalize.call(revised) == normalize.call(selected), 'checker changed beyond host AS setting')
    journal = read('host-address-space/initial-host-tests-run')
    bound = read('host-address-space/initial-host-tests-bounded')
    old = current.merge('runner_sha256' => sha(File.join(directory, 'initial-check.rb')))
    spec = ExpressionWalkChecks.specifications(phase: 'host').first.merge(address_space: 1_073_741_824)
    assert(journal.values_at('source_before', 'source_after') == [old] * 2 &&
      journal['specification'] == JSON.parse(JSON.generate(spec)) &&
      journal['command'] == spec[:command] && !journal['command_success'] && !journal['verified'],
      'preserved failed host provenance differs')
    assert(journal['environment'] == ExpressionWalkChecks.environment(spec,
      File.join(__dir__, 'host-tests-bounded.json'), File.join(__dir__, 'host-tests.json')) &&
      journal['ruby_executable_sha256'] == sha(spec[:binary]), 'failed host environment differs')
    expected_command = ['ruby', '-Iimpl/ruby-native/lib', '-e',
      'Dir["impl/ruby-native/test/*_test.rb"].sort.each { |f| require_relative f }']
    assert(!bound['success'] && bound['reason'] == 'worker memory limit exceeded (536870912 bytes)' &&
      bound['stderr'].include?('[FATAL] failed to allocate memory') &&
      bound.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
      bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
        [536_870_912, 1_073_741_824, 180.0] && bound['command'] == expected_command &&
      bound['peak_rss_bytes'].between?(1, 536_870_912), 'preserved host failure differs')
    probe = read('host-address-space/probe-bounded')
    assert(probe['success'] && probe['reason'].nil? &&
      probe.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
      probe.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
        [536_870_912, 2_147_483_648, 180.0] &&
      probe['command'] == ['ruby', 'bench/ruby-native/results/expression-walk-followup/host-address-space/probe.rb'] &&
      probe['stdout'].include?('host address-space diagnostic: existing test prefix passed') &&
      probe['peak_rss_bytes'].between?(1, 536_870_912), 'host reservation probe failed/differed')
    records = probe['stdout'].lines.filter_map { |line| JSON.parse(line) if line.start_with?('{') }
    before = records.find { |record| record['label'] == 'call concurrent_admission' }
    after = records.find { |record| record['label'] == 'return concurrent_admission' }
    assert(before && after && before['pid'] == after['pid'] &&
      after['status']['VmSize'].to_i * 1024 > 1_073_741_824 &&
      after['status']['VmRSS'].to_i * 1024 < 536_870_912 &&
      after['anonymous_no_access_bytes'] > before['anonymous_no_access_bytes'], 'virtual reservation observation missing')
    supervisor = read('host-address-space/initial-supervisor')
    assert(!supervisor['success'] && supervisor['reason'] == 'worker memory limit exceeded (536870912 bytes)' &&
      supervisor.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
      supervisor.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
        [536_870_912, 2_147_483_648, 900.0] &&
      supervisor['peak_rss_bytes'].between?(1, 536_870_912), 'preserved supervisor failure differs')
    { failed_host_peak_rss_bytes: bound['peak_rss_bytes'], failed_host_address_space_bytes: 1_073_741_824,
      final_host_address_space_bytes: 2_147_483_648, memory_bytes: 536_870_912,
      before_thread_test: before, after_thread_test: after, probe_peak_rss_bytes: probe['peak_rss_bytes'],
      note: 'The original host run failed under its virtual-address ceiling while observed RSS remained below the unchanged resident-memory guard. The diagnostic retains Linux reservation snapshots; the final host suite uses 2 GiB AS. Original failures and probe bytes remain intact.' }
  end

  def host_sandbox(current)
    return { present: false } unless Dir.exist?(File.join(__dir__, 'host-sandbox'))
    journal, bound = read('host-sandbox/host-tests-run'), read('host-sandbox/host-tests-bounded')
    spec = ExpressionWalkChecks.specifications(phase: 'host').first
    assert(journal.values_at('source_before', 'source_after') == [current] * 2 &&
      journal['specification'] == JSON.parse(JSON.generate(spec)) &&
      journal['command'] == spec[:command] && !journal['command_success'] && !journal['verified'],
      'preserved sandbox host provenance differs')
    assert(journal['environment'] == ExpressionWalkChecks.environment(spec,
      File.join(__dir__, 'host-tests-bounded.json'), File.join(__dir__, 'host-tests.json')) &&
      journal['ruby_executable_sha256'] == sha(spec[:binary]), 'sandbox host environment differs')
    assert(!bound['success'] && bound['reason'] == 'worker exited 1' &&
      bound['stderr'].include?('Errno::EPERM') && bound['stderr'].include?("UNIXServer#initialize") &&
      bound['stdout'].include?('ruby-native method sharing:') &&
      bound.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
      bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
        [536_870_912, 2_147_483_648, 180.0] &&
      bound['peak_rss_bytes'].between?(1, 536_870_912), 'preserved sandbox failure differs')
    supervisor = read('host-sandbox/supervisor')
    assert(!supervisor['success'] && supervisor['reason'] == 'worker exited 1' &&
      supervisor.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
      supervisor.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
        [536_870_912, 2_147_483_648, 1800.0] &&
      supervisor['peak_rss_bytes'].between?(1, 536_870_912), 'preserved sandbox supervisor differs')
    { peak_rss_bytes: bound['peak_rss_bytes'], memory_bytes: 536_870_912, address_space_bytes: 2_147_483_648,
      note: 'The sandbox blocked a local page-service Unix socket. The complete final host suite was rerun with the same resource guards and socket access; no assertion was waived.' }
  end

  def binding(name, source, runtime_name, group, lanes, verify: false)
    raw = read(name)
    identity = runtime(runtime_name)
    path, jobs, fresh, filter = @groups.fetch(group).values_at(:path, :jobs, :fresh, :filter)
    options = raw.fetch('options')
    assert(raw.fetch('errors').empty?, "binding failed #{name}")
    assert(options.values_at('ruby', 'jit', 'native_source', 'lanes', 'plan', 'output') ==
      [identity[:binary], identity[:jit], source == BASELINE ? ARCHIVE : SOURCE, lanes, path,
       File.join(__dir__, "#{name}.json")], 'binding options differ')
    assert(!!options['verify_only'] == verify && !!options['fresh_sql'] == fresh &&
      options['workloads'] == filter && !options['file'], 'binding selection differs')
    driver = ['mise', 'run', 'ruby-native:binding-compare', '--', '--ruby', identity[:binary],
      '--jit', identity[:jit], '--native-source', options['native_source'], '--lanes', lanes.join(','),
      '--plan', path, '--output', File.join(__dir__, "#{name}.json")]
    driver += ['--workloads', filter.join(',')] if filter
    driver << '--fresh-sql' if fresh
    driver << '--verify-only' if verify
    @expected[name] = { command: driver, source: options['native_source'], hash: source, environment: {} }
    assert(options.none? { |key, value| key.start_with?('disable_') && value }, 'binding disabled older tier')
    provenance = raw.fetch('source_sha256')
    assert(provenance == raw.fetch('source_sha256_after') && provenance['native'] == source, 'binding source changed')
    assert(provenance['worker'] == sha(File.join(ROOT, 'bench/ruby-native/binding_compare_lane.rb')) &&
      provenance['selected_plan'] == sha(path), 'binding worker/plan changed')
    assert(provenance['wrapper_library'] == sha(File.join(ROOT, 'impl/ruby/ext/target/release/libjed_ruby.so')), 'wrapper binary changed')
    assert(provenance['wrapper'] == contents_hash(Dir[File.join(ROOT, 'impl/ruby/lib/**/*.rb'),
      File.join(ROOT, 'impl/ruby/ext/src/**/*.rs')]), 'wrapper sources changed')
    assert(provenance['rust'] == contents_hash(Dir[File.join(ROOT, 'impl/rust/src/**/*.rs')]), 'Rust sources changed')
    provenance.fetch('plans').each { |file, hash| assert(sha(File.join(ROOT, 'bench/ruby-native/compare', file)) == hash, 'auxiliary plan changed') }
    common = provenance.reject { |key, _| %w[native selected_plan].include?(key) }
    assert(!@binding_provenance || @binding_provenance == common, 'nonnative inputs changed')
    @binding_provenance = common
    assert(!@cpu || @cpu == raw.fetch('cpu'), 'CPU differs')
    @cpu = raw.fetch('cpu')
    assert(raw.fetch('lanes').keys.sort == lanes.sort, 'binding lane coverage differs')
    raw.fetch('lanes').to_h do |lane, entry|
      assert(entry['success'] && entry['reason'].nil? &&
        entry.values_at('memory_limit_bytes', 'address_space_bytes') == [536_870_912, 2_147_483_648], 'binding worker/bounds failed')
      assert(entry.fetch('peak_rss_bytes').between?(1, 536_870_912), 'binding RSS exceeded')
      @peaks << entry.fetch('peak_rss_bytes')
      command = [identity[:binary], *ExpressionWalkRuns.flags(identity[:jit]),
        File.join(ROOT, 'bench/ruby-native/binding_compare_lane.rb'), lane]
      command << '--verify-only' if verify
      command += ['--workloads', filter.join(',')] if filter
      command += ['--plan', path]
      command << '--fresh-sql' if fresh
      assert(entry['command'] == command, 'binding child command differs')
      result = entry.fetch('result')
      assert(result['runtime'].start_with?(identity[:version]) && result['ruby_executable'] == identity[:binary] &&
        result['ruby_executable_sha256'] == sha(identity[:binary]), 'binding runtime differs')
      assert(result['jit'] == { 'yjit' => identity[:jit] == 'yjit', 'zjit' => identity[:jit] == 'zjit' }, 'binding JIT differs')
      assert(result.values_at('engine', 'verify_only', 'fresh_sql', 'storage', 'dataset_rows', 'repetitions') ==
        [lane, verify, fresh, 'memory', 1000, 5], 'binding worker mode differs')
      assert(result['source_sha256'] == result['final_source_sha256'], 'worker source changed')
      assert(lane == 'wrapper' ? result['native_library_sha256'] == provenance['wrapper_library'] :
        result['source_sha256'] == source, 'worker source identity differs')
      assert(result.values_at('plan_path', 'plan_sha256', 'script_sha256') == [path, sha(path), provenance['worker']], 'worker provenance differs')
      assert(%w[pipeline_disabled ordering_disabled aggregate_disabled relation_disabled text_compilation_disabled].none? { |flag| result[flag] }, 'worker disabled tier')
      assert(result.fetch('workloads').map { |job| job['name'] } == jobs.map { |job| job['name'] }, 'worker jobs differ')
      times = result.fetch('workloads').to_h do |job|
        spec = jobs.find { |item| item['name'] == job['name'] }
        key = "#{group}/#{job.fetch('name')}"
        assert(job.values_at('sql', 'warmup', 'iterations') == spec.values_at('sql', 'warmup', 'iterations'), 'SQL/schedule differs')
        params = Array.new(32) { |index| [spec['offset'] + index * spec['step'] % spec['span']] }
        assert(job['validation_parameters'] == params, 'parameter cycle differs')
        signature = job.slice('validation_sha256', 'answer_sha256', 'validation_rows', 'validation_costs',
          'validation_columns', 'validation_column_types')
        assert(!@answers[key] || @answers[key] == signature, "values/metadata/costs differ #{key}")
        @answers[key] = signature
        assert(job['callback_counts'] == { 'row_calls' => 0, 'batch_calls' => 0, 'batch_sizes' => [] }, 'unexpected callbacks')
        samples = job.fetch('samples_ns')
        if verify
          assert(samples.empty? && job['median_ns'].nil?, 'verify-only report contains timing')
        else
          assert(samples.length == 5 && samples.all? { |value| value.is_a?(Numeric) && value.finite? && value.positive? }, 'invalid samples')
          assert(job['median_ns'] == median(samples).fdiv(job['iterations']), 'median differs')
          rows = job.fetch('rows_per_sample')
          assert(rows.length == 5 && rows.uniq.length == 1, 'timed drains differ')
          schedule = job.slice('iterations', 'warmup', 'rows_per_sample')
          assert(!@schedules[key] || @schedules[key] == schedule, "drain schedule changed #{key}")
          @schedules[key] = schedule
        end
        [job['name'], job['median_ns']]
      end
      [lane, times]
    end
  end

  def paired(values)
    before, final = values.values_at(0, 3), values.values_at(1, 2)
    { before_us: before.sum / 2000.0, final_us: final.sum / 2000.0,
      speedup: before.sum.fdiv(final.sum), elapsed_change_percent: (final.sum.fdiv(before.sum) - 1) * 100,
      before_medians_ns: before, final_medians_ns: final,
      before_range_us: before.minmax.map { |value| value / 1000.0 },
      final_range_us: final.minmax.map { |value| value / 1000.0 },
      separated_faster: final.max < before.min, separated_slower: final.min > before.max }
  end

  def comparisons
    %w[before final].each do |state|
      source = state == 'before' ? BASELINE : @source
      binding("verify-#{state}", source, '4.0-yjit', 'main', ['wrapper', *NATIVE], verify: true)
    end
    series = ExpressionWalkRuns::RUNTIMES.keys.to_h do |runtime_name|
      runs = STATES.map do |state|
        binding("control-#{runtime_name}-#{state}", state.end_with?('before') ? BASELINE : @source,
          runtime_name, 'main', NATIVE)
      end
      [runtime_name, NATIVE.to_h do |api|
        [api, @main.fetch('workloads').to_h do |job|
          [job['name'], paired(runs.map { |run| run.fetch(api).fetch(job['name']) })]
        end]
      end]
    end
    actual = binding('binding-4.0-yjit', @source, '4.0-yjit', 'main', ['wrapper', *NATIVE])
    wrapper = actual.fetch('wrapper').to_h do |name, value|
      [name, { wrapper_us: value / 1000.0, native: NATIVE.to_h do |api|
        time = actual.fetch(api).fetch(name)
        [api, { us: time / 1000.0, native_over_wrapper: time.fdiv(value) }]
      end }]
    end
    cells = series.flat_map do |runtime_name, apis|
      apis.flat_map do |api, jobs|
        jobs.map { |name, row| { runtime: runtime_name, api: api, workload: name }.merge(row) }
      end
    end
    assert(cells.length == 81, 'paired cell count differs')
    { paired: series, slower_cells: cells.select { |row| row[:elapsed_change_percent].positive? },
      separated_faster: cells.count { |row| row[:separated_faster] },
      separated_slower: cells.count { |row| row[:separated_slower] },
      overlapping: cells.count { |row| !row[:separated_faster] && !row[:separated_slower] },
      actual_binding: wrapper, exact_values_metadata_costs: @answers, schedules: @schedules,
      max_rss_bytes: @peaks.max,
      note: 'ABBA uses the mean of two process medians per source. Separation is descriptive, not a significance test. Actual binding ratios use separate single-process measurements.' }
  end

  def direct_report(name, source, runtime_name)
    raw, bound, identity = read(name), read("#{name}-bounded"), runtime(runtime_name)
    artifact = File.join(__dir__, "#{name}.json")
    command = [identity[:binary], *ExpressionWalkRuns.flags(identity[:jit]),
      'bench/ruby-native/expression_walk_diagnostics.rb', '--jit', identity[:jit], '--report', artifact]
    bounded!(bound, source, command)
    selected = source == BASELINE ? ARCHIVE : SOURCE
    @expected[name] = { command: ['mise', 'run', 'ruby-native:bounded', '--', *command],
      source: selected, hash: source,
      environment: { 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") } }
    assert(raw.values_at('source_sha256', 'final_source_sha256') == [source] * 2 &&
      raw['source'] == selected && raw['mode'] == 'untimed_expression_walk_diagnostic', 'direct source/mode differs')
    assert(raw['runtime'].start_with?(identity[:version]) && raw['ruby_executable'] == identity[:binary] &&
      raw['ruby_executable_sha256'] == sha(identity[:binary]), 'direct runtime differs')
    assert(raw['jit'] == { 'yjit' => identity[:jit] == 'yjit', 'zjit' => identity[:jit] == 'zjit' }, 'direct JIT differs')
    assert(raw['script_sha256'] == sha(File.join(ROOT, 'bench/ruby-native/expression_walk_diagnostics.rb')) &&
      raw.values_at('plan_path', 'plan_sha256') == [PLAN, sha(PLAN)], 'direct script/plan changed')
    assert(raw['optimized_module'] == (source != BASELINE), 'direct implementation availability differs')
    jobs = JSON.parse(JSON.generate(ExpressionWalkDiagnostics.jobs))
    assert(raw['jobs'] == jobs, 'direct jobs differ')
    semantic = raw.fetch('semantic')
    assert(raw['semantic_sha256'] == Digest::SHA256.hexdigest(JSON.generate(semantic)), 'direct semantic hash differs')
    assert(!@direct_semantic || @direct_semantic == semantic, 'direct complete outcomes differ by source/runtime')
    @direct_semantic = semantic
    expected = jobs.map { |job| job['name'] }.product(ExpressionWalkDiagnostics::APIS.map(&:to_s))
    totals = %w[enabled disabled].to_h { |side| [side, Hash.new(0)] }
    %w[memory disk].each do |mode|
      cases, observations = semantic.fetch(mode).fetch('workloads'), raw.fetch('observations').fetch(mode)
      assert(cases.map { |job| job.values_at('workload', 'api') } == expected &&
        observations.map { |job| job.values_at('workload', 'api') } == expected, 'direct API/storage coverage differs')
      cases.zip(observations).each do |job, observation|
        bindings = ExpressionWalkDiagnostics.bindings(job['workload']).map { |value| [value] }
        assert(job['values'].map { |value| value['params'] } == bindings &&
          observation['values'].map { |value| value['params'] } == bindings, 'direct bindings differ')
        job['values'].zip(observation['values']).each_with_index do |(value, observed), index|
          state = ExpressionWalkDiagnostics::STATES.fetch(index)
          assert(value['state'] == state && observed['state'] == state && value.dig('outcome', 'status') == 'rows', 'direct state differs')
          assert(observed.fetch('calls').keys.sort == %w[disabled enabled], 'direct control coverage differs')
          observed['calls'].each do |side, counts|
            counts.each do |method, count|
              assert(count.is_a?(Integer) && count >= 0, 'invalid trace count')
              totals.fetch(side)[method] += count
            end
          end
          next if source == BASELINE
          ExpressionWalkDiagnostics::ORIGINALS.each do |method, klass|
            assert(observed['calls']['enabled'].fetch("#{klass}##{method}", 0).zero?, 'optimized SQL entered original helper')
          end
        end
      end
      explains = semantic[mode].fetch('explains')
      assert(explains.map { |item| item['workload'] } == jobs.map { |job| job['name'] }, 'EXPLAIN coverage differs')
      explains.zip(jobs).each do |item, job|
        assert(item['params'] == [ExpressionWalkDiagnostics.bindings(job['name']).first] &&
          item.dig('outcome', 'status') == 'rows', 'EXPLAIN binding/outcome differs')
        if job['index']
          assert(JSON.generate(item['outcome']['rows']).include?(job['index']) == job['matched'], 'index-match control differs')
        end
      end
      budgets = semantic[mode].fetch('budgets')
      assert(budgets.map { |job| job.values_at('workload', 'api') } ==
        ExpressionWalkDiagnostics::BUDGET_JOBS.product(ExpressionWalkDiagnostics::APIS.map(&:to_s)), 'budget coverage differs')
      budgets.each do |job|
        ordinary = job.fetch('ordinary')
        cost = ordinary.fetch('cost')
        limits = [1, [cost / 2, 1].max, [cost - 1, 1].max, cost, cost + 1].uniq.sort
        assert(ordinary['status'] == 'rows' && job['params'] == [ExpressionWalkDiagnostics.bindings(job['workload']).first] &&
          job['values'].map { |value| value.values_at('limit', 'setting') } ==
            limits.product(%w[max_cost lifetime_max_cost]), 'budget schedule differs')
        job['values'].each do |value|
          outcome = value.fetch('outcome')
          if outcome['status'] == 'error'
            assert(outcome['sqlstate'] == (value['setting'] == 'max_cost' ? '54P01' : '54P02'), 'unexpected budget error')
          else
            assert(outcome == ordinary, 'completed budget outcome differs')
          end
          assert(outcome == ordinary, 'above-cost budget did not complete') if value['limit'] > cost
          # Exact-ceiling success/error is retained verbatim, rather than
          # assumed uniform across API collectors. Every complete outcome is
          # equal to the original helper and across source/runtime reports.
        end
      end
    end
    ExpressionWalkDiagnostics::ORIGINALS.each do |method, klass|
      original = "#{klass}##{method}"
      assert(totals['disabled'].fetch(original, 0).positive?, 'disabled original helper was not exercised')
      executed = source == BASELINE ? original : "JedNative::ExpressionWalks##{method}"
      assert(totals['enabled'].fetch(executed, 0).positive?, 'selected helper was not exercised')
    end
    file = raw.fetch('storage').fetch('disk')
    assert(raw['storage']['memory'] == { 'kind' => 'memory' } && file['kind'] == 'file' &&
      file['locking'] == 'exclusive' && file['removed_after_run'] && !File.exist?(file['path']) &&
      file['path'].start_with?(File.join(ROOT, '.dev/ruby-native-expression-walk-diagnostics/')), 'direct storage differs')
    signature = file.values_at('bytes', 'sha256', 'filesystem_device')
    assert(!@file_signature || @file_signature == signature, 'file fixture bytes vary by source/runtime')
    @file_signature = signature
    assert(raw.values_at('allocation_iterations', 'allocation_samples') == [128, 5], 'helper allocation schedule differs')
    helpers = raw.fetch('helpers')
    expected_helpers = %w[nodes_tree nodes_nested_array conjunctions key_equal key_unequal key_typemod key_native_body_type key_nil_args]
    assert(helpers.map { |item| item['name'] } == expected_helpers, 'helper coverage differs')
    answers = helpers.map { |item| item.slice('name', 'method', 'answer', 'exact_order_identity') }
    assert(!@helper_answers || @helper_answers == answers, 'helper result/order/identity differs by source/runtime')
    @helper_answers = answers
    allocation_results = helpers.to_h do |item|
      assert(item['exact_order_identity'] && ExpressionWalkDiagnostics::METHODS.map(&:to_s).include?(item['method']), 'helper identity/type differs')
      allocations = item.fetch('allocations')
      assert(allocations.keys.sort == %w[disabled enabled] && allocations.values.all? do |samples|
        samples.length == 5 && samples.all? { |value| value.is_a?(Integer) && value >= 0 }
      end, 'invalid helper allocation samples')
      [item['name'], { method: item['method'], enabled_objects: median(allocations['enabled']).fdiv(128),
        disabled_objects: median(allocations['disabled']).fdiv(128), samples: allocations }]
    end
    { semantic_sha256: raw['semantic_sha256'], peak_rss_bytes: bound['peak_rss_bytes'],
      workload_api_cases: expected.length * 2, traced_invocations: expected.length * 12,
      call_totals: totals, helpers: allocation_results, storage: raw['storage'] }
  end

  def diagnostics
    results = ExpressionWalkRuns::RUNTIMES.keys.to_h do |runtime_name|
      pair = %w[before final].to_h do |state|
        [state, direct_report("diagnostic-#{state}-#{runtime_name}", state == 'before' ? BASELINE : @source, runtime_name)]
      end
      [runtime_name, pair]
    end
    { runtimes: results, note: 'Untimed source-paired reports verify enabled/original helper equality, full result metadata/cost/error prefixes and EXPLAIN choices. Allocation samples count Ruby objects with GC enabled; they are not timing measurements. No new retained cache is introduced.' }
  end

  def smoke(pins, correctness)
    return { present: false } unless Dir.exist?(File.join(__dir__, 'harness-smoke'))
    journal, raw, bound = %w[run diagnostic diagnostic-bounded].map { |name| read("harness-smoke/#{name}") }
    command = [runtime('4.0-yjit')[:binary], '--yjit', 'bench/ruby-native/expression_walk_diagnostics.rb',
      '--jit', 'yjit', '--report', '/tmp/jed-expression-walk-smoke.json']
    bounded!(bound, @source, command)
    assert(journal['success'] && journal['command'] == ['mise', 'run', 'ruby-native:bounded', '--', *command] &&
      journal.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
      journal['script_sha256'] == sha(File.join(ROOT, 'bench/ruby-native/expression_walk_diagnostics.rb')) &&
      journal['plan_sha256'] == sha(PLAN), 'pre-pin smoke provenance differs')
    assert(raw.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
      raw['semantic'] == @direct_semantic &&
      raw['semantic_sha256'] == Digest::SHA256.hexdigest(JSON.generate(@direct_semantic)) &&
      raw['script_sha256'] == journal['script_sha256'] && raw['plan_sha256'] == sha(PLAN) &&
      raw['jit'] == { 'yjit' => true, 'zjit' => false }, 'pre-pin smoke outcome differs')
    assert(Time.iso8601(journal['started_at']) <= Time.iso8601(journal['finished_at']) &&
      Time.iso8601(journal['finished_at']) <= Time.iso8601(pins['captured_at']), 'smoke/pin order differs')
    correctness[:reports].each_value do |report|
      assert(Time.iso8601(report[:finished_at]) <= Time.iso8601(journal['started_at']), 'smoke overlapped correctness')
    end
    { present: true, started_at: journal['started_at'], finished_at: journal['finished_at'],
      semantic_sha256: raw['semantic_sha256'], peak_rss_bytes: bound['peak_rss_bytes'],
      note: 'One untimed pre-pin fixture smoke, kept outside the 24 formal commands. No source or fixture correction was needed.' }
  end

  def cross
    raw = read('cross-core')
    assert(raw.values_at('ruby_source_sha256', 'ruby_source_sha256_after') == [@source] * 2 &&
      raw['source_sha256'] == raw['source_sha256_after'], 'cross-core source changed')
    assert(raw.values_at('plan_path', 'plan_sha256', 'plan') == [PLAN, sha(PLAN), @main], 'cross-core plan differs')
    lanes = %w[go ruby-yjit rust ts]
    assert(raw.fetch('statuses').keys.sort == lanes && raw['statuses'].values.all? { |value| value['status'] == 'ready' } &&
      raw.fetch('results').keys.sort == lanes && raw.fetch('resources').keys.sort == lanes, 'cross-core failed or omitted lane')
    assert(raw.values_at('ruby_source', 'phase') == [SOURCE, 'expression-walk-cross-core'], 'cross-core selected source differs')
    assert(%w[ordering_disabled aggregate_disabled relation_disabled text_compilation_disabled verification_only].none? { |key| raw[key] }, 'cross-core disabled tier')
    binary = runtime('4.0-yjit')[:binary]
    assert(raw['ruby_executable'] == binary, 'cross-core Ruby runtime differs')
    adapters = File.join(ROOT, 'bench/ruby-native/compare')
    files = {
      'harness' => ['bench/ruby-native/compare.rb', File.join(ROOT, 'bench/ruby-native/source.rb'),
        File.join(ROOT, 'impl/ruby-native/tools/bounded_process.rb')],
      'ruby_adapter' => [File.join(adapters, 'lane.rb')],
      'go_adapter' => Dir[File.join(adapters, 'go/*')].select { |path| File.file?(path) },
      'rust_adapter' => Dir[File.join(adapters, 'rust/src/**/*.rs'), File.join(adapters, 'rust/Cargo.*')],
      'ts_adapter' => [File.join(adapters, 'lane.ts')],
      'go_core' => Dir[File.join(ROOT, 'impl/go/**/*.go')],
      'rust_core' => Dir[File.join(ROOT, 'impl/rust/src/**/*.rs')],
      'ts_core' => Dir[File.join(ROOT, 'impl/ts/src/**/*.ts')]
    }
    files.each { |key, paths| assert(raw['source_sha256'][key] == named_hash(paths), "cross-core source changed #{key}") }
    assert(raw['source_sha256']['ruby_executable'] == sha(binary), 'cross-core Ruby binary changed')
    %w[go rust].each do |lane|
      assert(raw['source_sha256']["#{lane}_binary"] == sha(raw['resources'][lane]['command'].first), 'cross-core binary changed')
    end
    tsv = '/tmp/jed-ruby-native-compare/plan.tsv'
    commands = {
      'ruby-yjit' => [binary, '--yjit', File.join(adapters, 'lane.rb'), tsv],
      'go' => ['/tmp/jed-ruby-native-compare/go-lane', tsv],
      'rust' => ['/tmp/jed-ruby-native-compare/rust-target/release/jed-ruby-experiment-compare', tsv],
      'ts' => ['node', File.join(adapters, 'lane.ts'), tsv]
    }
    raw['resources'].each do |lane, value|
      assert(value['command'] == commands.fetch(lane), 'cross-core worker command differs')
      assert(value.values_at('memory_limit_bytes', 'address_space_bytes') ==
        [536_870_912, lane == 'ruby-yjit' ? 2 * 1024**3 : 16 * 1024**3] &&
        value.fetch('peak_rss_bytes').between?(1, 536_870_912), 'cross-core bounds differ')
    end
    signatures = {}
    times = raw['results'].transform_values do |jobs|
      assert(jobs.map { |job| job['workload'] } == @main['workloads'].map { |job| job['name'] }, 'cross-core job coverage differs')
      jobs.to_h do |job|
        name = job.fetch('workload')
        spec = @main['workloads'].find { |item| item['name'] == name }
        samples = job.fetch('samples_ns')
        assert(job.values_at('iterations', 'warmup') == spec.values_at('iterations', 'warmup') &&
          samples.length == 5 && samples.all? { |value| value.is_a?(Numeric) && value.finite? && value.positive? }, 'cross-core schedule/samples differ')
        signature = job.slice('checksum', 'validation_rows', 'rows_total', 'iterations', 'warmup')
        assert(!signatures[name] || signatures[name] == signature, 'cross-core rendered values/rows differ')
        signatures[name] = signature
        [name, median(samples).fdiv(job['iterations']) / 1000.0]
      end
    end
    signatures.each do |name, signature|
      assert(signature['validation_rows'] == @answers.fetch("main/#{name}").fetch('validation_rows').sum &&
        signature['rows_total'] == @schedules.fetch("main/#{name}").fetch('rows_per_sample').sum, 'cross-core drains differ from exact binding')
    end
    labels = raw['results'].transform_values { |jobs| jobs.map { |job| job['runtime'] }.uniq }
    assert(labels.values.all? { |values| values.length == 1 }, 'cross-core runtime varies by job')
    assert(labels['ruby-yjit'].first.start_with?('ruby 4.0.7 ') && labels['ruby-yjit'].first.include?('+YJIT') &&
      labels['ts'].first == raw['host']['node'] && labels['go'].first == raw['host']['go'].split[2] &&
      labels['rust'] == ['rust-release'] && raw['host']['cpu'] == @cpu, 'cross-core runtime/CPU differs')
    @expected['cross-core'] = { command: ['mise', 'run', 'ruby-native:compare'], source: SOURCE, hash: @source,
      environment: { 'RUBY_NATIVE_LANES' => 'ruby-yjit,go,rust,ts', 'RUBY_NATIVE_PHASE' => 'expression-walk-cross-core',
        'RUBY_NATIVE_COMPARE_PLAN' => PLAN, 'RUBY_NATIVE_COMPARE_TIMEOUT' => '300',
        'RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES' => '2147483648' } }
    { us: times, ruby_over: times['ruby-yjit'].to_h do |name, ruby|
      [name, %w[go rust ts].to_h { |lane| [lane, ruby.fdiv(times[lane][name])] }]
    end, runtime_labels: labels.transform_values(&:first), resources: raw['resources'], validation: signatures,
      note: 'One fresh process per core; median of five samples. Cross-core checks rendered-value checksums/row counts, not exact costs/metadata. Existing collectors differ; exact binding verification is separate.' }
  end

  def profiles
    results = %w[before final].to_h do |state|
      name = "#{state}-profile"
      source = state == 'before' ? BASELINE : @source
      selected = state == 'before' ? ARCHIVE : SOURCE
      raw, bound = read(name), read("#{name}-bounded")
      artifact = File.join(__dir__, "#{name}.json")
      command = [runtime('4.0-yjit')[:binary], '--yjit', 'bench/ruby-native/query_profile.rb',
        '--api', 'execute', '--report', artifact]
      bounded!(bound, source, command)
      assert(raw.values_at('source_sha256', 'final_source_sha256') == [source] * 2 &&
        raw['source'] == selected && bound['source'] == selected, 'profile source differs')
      assert(raw['ruby'].start_with?('ruby 4.0.7 ') && raw['ruby'].include?('+YJIT') &&
        !raw['ruby'].include?('+ZJIT') && raw['ruby_executable'] == command.first, 'profile runtime differs')
      assert(raw.values_at('plan_path', 'plan_sha256', 'script_sha256') ==
        [PLAN, sha(PLAN), sha(File.join(ROOT, 'bench/ruby-native/query_profile.rb'))], 'profile provenance differs')
      assert(raw['options'] == { 'api' => 'execute', 'trace' => 3, 'report' => artifact }, 'profile options differ')
      assert(raw['results'].map { |job| job['workload'] } == @main['workloads'].map { |job| job['name'] }, 'profile jobs differ')
      jobs = raw['results'].to_h do |job|
        job_name = job.fetch('workload')
        spec = @main['workloads'].find { |item| item['name'] == job_name }
        samples = job.fetch('samples')
        assert(job.values_at('api', 'sql') == ['execute', spec['sql']] && samples.length == 5 &&
          samples.map { |sample| sample['rows'] } == @schedules.fetch("main/#{job_name}").fetch('rows_per_sample'), 'profile query/drains differ')
        assert(job['calls_per_query'].values.all? { |value| value.is_a?(Numeric) && value.finite? && value >= 0 } &&
          samples.all? { |sample| sample['ns_per_query'].positive? && sample['allocations_per_query'] >= 0 }, 'invalid profile counts/samples')
        [job_name, { allocations: median(samples.map { |sample| sample['allocations_per_query'] }),
          diagnostic_us: median(samples.map { |sample| sample['ns_per_query'] }) / 1000.0,
          calls: job['calls_per_query'], allocation_sites: job['allocation_sites'], answer_sha256: job['first_answer_sha256'] }]
      end
      @expected[name] = { command: ['mise', 'run', 'ruby-native:bounded', '--', *command], source: selected, hash: source,
        environment: { 'RUBY_NATIVE_COMPARE_PLAN' => PLAN, 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") } }
      [state, { jobs: jobs, peak_rss_bytes: bound['peak_rss_bytes'] }]
    end
    comparisons = results['before'][:jobs].to_h do |name, old|
      current = results['final'][:jobs].fetch(name)
      assert(old[:answer_sha256] == current[:answer_sha256], 'profile answer changed')
      [name, { before_allocations: old[:allocations], final_allocations: current[:allocations],
        allocation_change_percent: (current[:allocations].fdiv(old[:allocations]) - 1) * 100,
        before_calls: old[:calls], final_calls: current[:calls] }]
    end
    results.merge(comparisons: comparisons,
      note: 'execute_prepared with the existing eager collector. All timings precede tracing. Allocations and call counts are diagnostic observations, not sampled CPU-time attribution.')
  end

  def journals(pins, correctness)
    commands, reports = [], {}
    %w[verify diagnostics controls cross binding profiles].each do |phase|
      paths = Dir[File.join(__dir__, "run-#{phase}*.json")].sort
      assert(!paths.empty?, "missing #{phase} journal")
      paths.each do |path|
        raw = JSON.parse(File.read(path))
        options = raw.fetch('options')
        assert(raw['phase'] == phase && options['phase'] == phase &&
          raw['script_sha256'] == sha(File.join(__dir__, 'run.rb')) && raw['plan_sha256'] == sha(PLAN), 'runner provenance changed')
        assert(options.values_at('final_hash', 'baseline_source') == [@source, ARCHIVE], 'runner selected source differs')
        assert(!options['runtime'] || ExpressionWalkRuns::RUNTIMES.key?(options['runtime']), 'runner runtime differs')
        runtimes = options['runtime'] ? [options['runtime']] : ExpressionWalkRuns::RUNTIMES.keys
        names = case phase
                when 'verify' then %w[before final].map { |state| "verify-#{state}" }
                when 'diagnostics' then runtimes.flat_map { |runtime_name| %w[before final].map { |state| "diagnostic-#{state}-#{runtime_name}" } }
                when 'controls' then runtimes.flat_map { |runtime_name| STATES.map { |state| "control-#{runtime_name}-#{state}" } }
                when 'cross' then ['cross-core']
                when 'binding' then ['binding-4.0-yjit']
                when 'profiles' then %w[before-profile final-profile]
                end
        assert(raw['commands'].map { |entry| entry['name'] } == names, 'runner coverage/order differs')
        raw['commands'].each do |entry|
          expected = @expected.fetch(entry.fetch('name'))
          assert(entry['success'] && entry.values_at('source_sha256', 'final_source_sha256') == [expected[:hash]] * 2 &&
            entry['source'] == expected[:source] && entry['command'] == expected[:command], 'runner command failed/differed')
          environment = { 'MISE_STATE_DIR' => '/tmp/jed-ruby-native-mise-state', 'MISE_CACHE_DIR' => '/tmp/jed-ruby-native-mise-cache',
            'MISE_TRUSTED_CONFIG_PATHS' => ROOT, 'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
            'JED_NATIVE_MEMORY_BYTES' => '536870912', 'JED_NATIVE_ADDRESS_SPACE_BYTES' => '2147483648',
            'JED_NATIVE_TIMEOUT' => '300', 'JED_NATIVE_RUN_REPORT' => nil, 'RUBY_NATIVE_SOURCE' => expected[:source] }.merge(expected[:environment])
          assert(entry['environment'] == environment, 'runner environment differs')
          assert(Time.iso8601(entry['started_at']) <= Time.iso8601(entry['finished_at']), 'runner interval reversed')
          commands << entry
        end
        reports[File.basename(path)] = names
      end
    end
    assert(commands.length == 24 && commands.map { |entry| entry['name'] }.sort == @expected.keys.sort, 'duplicate/missing command')
    ordered = commands.sort_by { |entry| Time.iso8601(entry['started_at']) }
    ordered.each_cons(2) { |a, b| assert(Time.iso8601(a['finished_at']) <= Time.iso8601(b['started_at']), 'workload commands overlapped') }
    first_timed = ordered.find { |entry| !entry['name'].start_with?('verify-', 'diagnostic-') }.fetch('started_at')
    assert(Time.iso8601(pins['captured_at']) <= Time.iso8601(ordered.first['started_at']), 'pins captured after execution started')
    ordered.select { |entry| entry['name'].start_with?('verify-', 'diagnostic-') }.each do |entry|
      assert(Time.iso8601(entry['finished_at']) <= Time.iso8601(first_timed), 'verification occurred during/after timing')
    end
    correctness.fetch(:reports).each_value do |report|
      assert(Time.iso8601(report[:finished_at]) <= Time.iso8601(ordered.first['started_at']),
        'correctness overlapped the formal verification/diagnostic queue')
    end
    prefixes = ExpressionWalkRuns::RUNTIMES.keys.map { |name| "control-#{name}" }
    prefixes.each do |prefix|
      assert(ordered.map { |entry| entry['name'] }.grep(/\A#{Regexp.escape(prefix)}-/) ==
        STATES.map { |state| "#{prefix}-#{state}" }, 'ABBA order differs')
    end
    assert(ordered.last(2).map { |entry| entry['name'] } == %w[before-profile final-profile], 'profiles were not last')
    { serial_commands: ordered.length, journals: reports, first_timed_started_at: first_timed,
      last_finished_at: ordered.last['finished_at'] }
  end

  def tables(summary)
    lines = ["# Expression-walk measurements", "", "Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.", ""]
    summary[:comparisons][:paired].each do |runtime_name, apis|
      lines += ["## #{runtime_name}", "", "| API | Workload | Before | Final | Speedup | Before process range | Final process range |", "|---|---|---:|---:|---:|---|---|"]
      apis.each do |api, jobs|
        jobs.each do |name, row|
          ranges = %i[before_range_us final_range_us].map { |key| row[key].map { |value| format('%.3f', value) }.join('–') }
          lines << format('| %s | %s | %.3f | %.3f | %.3f× | %s | %s |', api, name, row[:before_us], row[:final_us], row[:speedup], *ranges)
        end
      end
      lines << ''
    end
    lines += ["## Slower observations", "", "| Runtime | API | Workload | Elapsed change | Both finals above both befores |", "|---|---|---|---:|---|"]
    summary[:comparisons][:slower_cells].each do |row|
      lines << format('| %s | %s | %s | +%.3f%% | %s |', row[:runtime], row[:api], row[:workload], row[:elapsed_change_percent], row[:separated_slower] ? 'yes' : 'no')
    end
    lines += ["", "## Fresh cross-core observations", "", "| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |", "|---|---:|---:|---:|---:|---:|---:|"]
    summary[:cross][:us]['ruby-yjit'].each do |name, value|
      times = summary[:cross][:us]
      ratios = summary[:cross][:ruby_over].fetch(name)
      lines << format('| %s | %.3f | %.3f | %.3f | %.3f | %.3f× | %.3f× |', name, value, times['go'][name], times['rust'][name], times['ts'][name], ratios['go'], ratios['rust'])
    end
    lines += ["", "## Actual Ruby Rust binding", "", "| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |", "|---|---:|---:|---:|---:|---:|"]
    summary[:comparisons][:actual_binding].each do |name, row|
      values = NATIVE.map { |api| row[:native][api][:us] }
      lines << format('| %s | %.3f | %.3f | %.3f | %.3f | %.3f× |', name, row[:wrapper_us], *values, row[:native]['native-prepared'][:native_over_wrapper])
    end
    lines += ["", "## Profile allocations", "", "| Workload | Before objects/query | Final objects/query | Change |", "|---|---:|---:|---:|---:|"]
    summary[:profiles][:comparisons].each do |name, row|
      lines << format('| %s | %.3f | %.3f | %+.3f%% |', name, row[:before_allocations], row[:final_allocations], row[:allocation_change_percent])
    end
    lines += ["", "## Profile calls (counts, not CPU-time percentages)", "", "| Workload | Method | Before | Final |", "|---|---|---:|---:|"]
    summary[:profiles][:comparisons].each do |name, row|
      ExpressionWalkDiagnostics::ORIGINALS.each do |method, klass|
        original, replacement = "#{klass}##{method}", "JedNative::ExpressionWalks##{method}"
        lines << format('| %s | %s → %s | %.3f | %.3f |', name, original, replacement,
          row[:before_calls].fetch(original, 0), row[:final_calls].fetch(replacement, 0))
      end
    end
    lines += ["", "## Direct helper allocations (objects/invocation; no elapsed-time samples)", "",
      "| Runtime | Helper | Before | Final | Final disabled control |", "|---|---|---:|---:|---:|"]
    summary[:diagnostics][:runtimes].each do |runtime_name, pair|
      pair['before'][:helpers].each do |name, original|
        current = pair['final'][:helpers].fetch(name)
        lines << format('| %s | %s | %.3f | %.3f | %.3f |', runtime_name, name,
          original[:enabled_objects], current[:enabled_objects], current[:disabled_objects])
      end
    end
    File.write(File.join(__dir__, 'tables.md'), lines.join("\n") + "\n")
  end

  def main
    @source = NativeRubySource.digest(SOURCE)
    @main = JSON.parse(File.read(PLAN))
    assert(@main.values_at('rows', 'repetitions', 'validation_iterations') == [1000, 5, 32] &&
      @main['workloads'].length == 9, 'plan fixture differs')
    @groups = { 'main' => { path: PLAN, jobs: @main['workloads'], fresh: false, filter: nil } }
    @answers, @schedules, @expected, @peaks = {}, {}, {}, []
    pins = pins!
    checks = correctness
    summary = { baseline_commit: ExpressionWalkRuns::BASELINE_COMMIT, baseline_source_sha256: BASELINE,
      source_sha256: @source, correctness: checks.reject { |key, _| key == :provenance },
      comparisons: comparisons, diagnostics: diagnostics, cross: cross, profiles: profiles }
    summary[:host_address_space] = host_address_space(checks[:provenance])
    summary[:host_sandbox] = host_sandbox(checks[:provenance])
    summary[:harness_smoke] = smoke(pins, checks)
    summary[:execution] = journals(pins, checks)
    write('summary', summary)
    write('source', { final: checks[:provenance], baseline_commit: ExpressionWalkRuns::BASELINE_COMMIT,
      baseline_source_sha256: BASELINE, benchmark_pins: pins,
      verifier_dependencies: %w[check.rb ../literal-like-followup/compact.rb].to_h do |path|
        [path, sha(File.expand_path(path, __dir__))]
      end })
    tables(summary)
    ledger = LiteralLikeEvidenceJSON.run(__dir__)
    ledger.fetch('files').each do |name, entry|
      path = File.join(__dir__, name)
      assert(File.size(path) == entry['compact_bytes'] && sha(path) == entry['compact_file_sha256'] &&
        Digest::SHA256.hexdigest(JSON.generate(JSON.parse(File.read(path)))) == entry['parsed_json_sha256'], 'lossless serialization changed values')
    end
    write('serialization', ledger)
    paths = Dir[File.join(__dir__, '**/*')].select { |path| File.file?(path) && File.basename(path) != 'manifest.json' }
    paths += [File.join(ROOT, 'bench/ruby-native/expression_walk_diagnostics.rb'), PLAN,
      File.expand_path('../literal-like-followup/compact.rb', __dir__)]
    manifest = paths.uniq.sort.to_h do |path|
      relative = Pathname.new(path).relative_path_from(Pathname.new(__dir__)).to_s
      [relative, { bytes: File.size(path), sha256: sha(path) }]
    end
    write('manifest', manifest)
    puts "Expression-walk evidence verified: #{summary[:execution][:serial_commands]} serial commands, #{manifest.length} manifest files."
  end
end

ExpressionWalkEvidence.main if $PROGRAM_NAME == __FILE__
