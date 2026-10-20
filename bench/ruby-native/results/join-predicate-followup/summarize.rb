# frozen_string_literal: true

# Read-only verification precedes derived output and lossless JSON compaction.
# Run only after every correctness, benchmark and profile process has exited.
require 'json'
require 'digest'
require 'time'
require 'pathname'
require_relative 'run'
require_relative 'check'
require_relative '../../join_predicate_diagnostics'
require_relative '../literal-like-followup/compact'

module JoinPredicateEvidence
  ROOT = JoinPredicateRuns::ROOT
  SOURCE = File.join(ROOT, 'impl/ruby-native')
  ARCHIVE = '/tmp/jed-join-predicate-before/impl/ruby-native'
  BASELINE = JoinPredicateRuns::BASELINE
  PLAN = JoinPredicateRuns::PLAN
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
    binary, jit = JoinPredicateRuns::RUNTIMES.fetch(name)
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
      [JoinPredicateRuns::BASELINE_COMMIT, ARCHIVE, BASELINE], 'baseline pins differ')
    assert(NativeRubySource.digest(ARCHIVE) == BASELINE, 'baseline archive changed')
    assert(pins.fetch('source_sha256') == @source, 'final source differs from pins')
    expected = JoinPredicateRuns::PINNED_FILES + Dir.chdir(ROOT) do
      Dir['bench/ruby-native/compare/go/*', 'bench/ruby-native/compare/rust/src/**/*.rs',
        'bench/ruby-native/compare/rust/Cargo.*', 'bench/ruby-native/compare/lane.rb',
        'bench/ruby-native/compare/lane.ts']
    end
    assert(pins.fetch('files').keys.sort == expected.uniq.sort, 'pin file set differs')
    pins.fetch('files').each { |path, hash| assert(sha(File.join(ROOT, path)) == hash, "pinned file changed #{path}") }
    binaries = pins.fetch('runtime_files')
    ruby = JoinPredicateRuns::RUNTIMES.values.map(&:first).uniq
    assert(binaries.length == 3 && (ruby - binaries.keys).empty? &&
      (binaries.keys - ruby).all? { |path| File.basename(path) == 'node' }, 'runtime pin set differs')
    binaries.each { |path, hash| assert(sha(path) == hash, "runtime changed #{path}") }
    pins
  end

  def correctness
    current = JoinPredicateChecks.provenance
    specifications = JoinPredicateChecks.specifications(phase: 'focus') +
      JoinPredicateChecks.specifications(phase: 'host') + JoinPredicateChecks.specifications(phase: 'corpus')
    reports = specifications.to_h do |spec|
      name = spec.fetch(:name)
      journal, bound = read("#{name}-run"), read("#{name}-bounded")
      assert(journal.values_at('source_before', 'source_after') == [current] * 2, 'correctness provenance changed')
      assert(journal['command_success'] && journal['verified'], 'correctness driver failed')
      assert(journal['specification'] == JSON.parse(JSON.generate(spec)), 'correctness specification differs')
      assert(journal['command'] == spec.fetch(:command), 'correctness command differs')
      env = JoinPredicateChecks.environment(spec, File.join(__dir__, "#{name}-bounded.json"), File.join(__dir__, "#{name}.json"))
      assert(journal['environment'] == env, 'correctness environment differs')
      assert(journal['ruby_executable_sha256'] == sha(spec.fetch(:binary)), 'correctness executable changed')
      report = spec[:phase] == 'corpus' ? read(name) : nil
      JoinPredicateChecks.verify!(spec, bound, report, current)
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

  def smoke_fixture
    directory = File.join(__dir__, 'smoke-fixture')
    return nil unless File.directory?(directory)

    metadata = JSON.parse(File.read(File.join(directory, 'failure.json')))
    initial = JSON.parse(File.read(File.join(directory, 'initial-bounded.json')))
    corrected = JSON.parse(File.read(File.join(directory, 'corrected-bounded.json')))
    old_script = File.join(directory, 'initial-diagnostic.rb')
    assert(metadata['source_sha256'] == @source && metadata['initial_script_sha256'] == sha(old_script),
      'smoke fixture provenance differs')
    expected = File.read(old_script).sub("  def resolve(engine, sql)\n",
      "  def resolve(engine, sql)\n    engine.instance_variable_set(:@params, [0])\n")
    assert(expected == File.read(File.join(ROOT, 'bench/ruby-native/join_predicate_diagnostics.rb')),
      'smoke correction was not solely the parameter-slot initialization')
    [initial, corrected].each do |report|
      assert(report.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
        report.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
          [536_870_912, 2_147_483_648, 300.0] &&
        report.fetch('peak_rss_bytes').between?(1, 536_870_912), 'smoke guard/source differs')
    end
    assert(initial['success'] == false && initial['reason'] == 'worker exited 1' &&
      initial['stderr'].include?('42601: bind parameter count mismatch') &&
      initial['stderr'].include?('JoinPredicateDiagnostics.resolve'), 'initial fixture failure differs')
    assert(corrected['success'] && corrected['reason'].nil? &&
      corrected['stdout'].include?('80 memory/file API groups passed'), 'corrected smoke failed')
    { status: 'fixture_corrected_before_pins', initial_source_sha256: @source,
      initial_script_sha256: metadata['initial_script_sha256'],
      corrected_peak_rss_bytes: corrected['peak_rss_bytes'], explanation: metadata['reason'] }
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
      command = [identity[:binary], *JoinPredicateRuns.flags(identity[:jit]),
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
    series = JoinPredicateRuns::RUNTIMES.keys.to_h do |runtime_name|
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
    assert(cells.length == 90, 'paired cell count differs')
    { paired: series, slower_cells: cells.select { |row| row[:elapsed_change_percent].positive? },
      separated_faster: cells.count { |row| row[:separated_faster] },
      separated_slower: cells.count { |row| row[:separated_slower] },
      overlapping: cells.count { |row| !row[:separated_faster] && !row[:separated_slower] },
      actual_binding: wrapper, exact_values_metadata_costs: @answers, schedules: @schedules,
      max_rss_bytes: @peaks.max,
      note: 'ABBA uses the mean of two process medians per source. Separation is descriptive, not a significance test. Actual binding ratios use separate single-process measurements.' }
  end

  def direct_trace!(trace, totals)
    assert(trace.keys.sort == %w[disabled enabled], 'direct control coverage differs')
    trace.each do |side, value|
      assert(value.keys.sort == %w[calls physical_plans], 'trace schema differs')
      value.fetch('calls').each do |method, count|
        assert(count.is_a?(Integer) && count >= 0, 'invalid trace count')
        totals.fetch(side)[method] += count
      end
    end
    assert(trace['enabled']['physical_plans'] == trace['disabled']['physical_plans'], 'JOIN compilation changed current physical plan')
    trace['enabled']['physical_plans']
  end

  def cache_state!(state, available)
    assert(state['limit'] == 64 && state['retained'].between?(0, 64) &&
      state['entries'].length == state['retained'] &&
      state['kernels'] == state['entries'].count { |entry| entry['materialized'] }, 'cache state invalid')
    state['entries'].each do |entry|
      assert(entry.values_at('source_sha256', 'pool_sha256').all? { |value| value.match?(/\A[0-9a-f]{64}\z/) }, 'cache key digest invalid')
      assert(entry['source_bytes'].between?(1, 32_768) && entry['pool_entries'].between?(0, 256) &&
        entry['pool_bytes'].between?(0, 32_768) &&
        (entry['materialized'] ? entry['node_count'].between?(1, 192) : entry['node_count'].nil?), 'cache program allowance exceeded')
    end
    assert(state['retained'].zero?, 'disabled/baseline JOIN cache populated') unless available
  end

  def direct_report(name, source, runtime_name)
    raw, bound, identity = read(name), read("#{name}-bounded"), runtime(runtime_name)
    artifact = File.join(__dir__, "#{name}.json")
    command = [identity[:binary], *JoinPredicateRuns.flags(identity[:jit]),
      'bench/ruby-native/join_predicate_diagnostics.rb', '--jit', identity[:jit], '--report', artifact]
    bounded!(bound, source, command)
    selected, available = source == BASELINE ? ARCHIVE : SOURCE, source != BASELINE
    @expected[name] = { command: ['mise', 'run', 'ruby-native:bounded', '--', *command],
      source: selected, hash: source,
      environment: { 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "#{name}-bounded.json") } }
    assert(raw.values_at('source_sha256', 'final_source_sha256') == [source] * 2 &&
      raw['source'] == selected && bound['source'] == selected &&
      raw['mode'] == 'untimed_join_predicate_diagnostic', 'direct source/mode differs')
    assert(raw['runtime'].start_with?(identity[:version]) && raw['ruby_executable'] == identity[:binary] &&
      raw['ruby_executable_sha256'] == sha(identity[:binary]), 'direct runtime differs')
    assert(raw['jit'] == { 'yjit' => identity[:jit] == 'yjit', 'zjit' => identity[:jit] == 'zjit' }, 'direct JIT differs')
    assert(raw['script_sha256'] == sha(File.join(ROOT, 'bench/ruby-native/join_predicate_diagnostics.rb')) &&
      raw.values_at('plan_path', 'plan_sha256') == [PLAN, sha(PLAN)], 'direct script/plan changed')
    assert(raw.values_at('join_compilation_available', 'fixture_rows', 'disk_work_mem') == [available, 32, 1024], 'direct implementation/guard differs')
    assert(raw['compiler_limits'] == { 'nodes' => 192, 'depth' => 32, 'source_bytes' => 32_768,
      'pool_entries' => 256, 'pool_bytes' => 32_768, 'join_cache_entries' => 64,
      'minimum_join_candidates' => available ? 8 : nil }, 'compiler/cache limits differ')
    jobs = JoinPredicateDiagnostics.jobs
    assert(raw['jobs'] == jobs, 'direct jobs differ')
    semantic = raw.fetch('semantic')
    assert(raw['semantic_sha256'] == Digest::SHA256.hexdigest(JSON.generate(semantic)), 'direct semantic hash differs')
    assert(!@direct_semantic || @direct_semantic == semantic, 'complete direct outcomes differ by source/runtime')
    @direct_semantic = semantic
    assert(semantic.keys.sort == %w[disk helpers memory] && raw['observations'].keys.sort == %w[disk memory], 'direct coverage differs')
    expected = jobs.map { |job| job['name'] }.product(JoinPredicateDiagnostics::APIS.map(&:to_s))
    totals = %w[enabled disabled].to_h { |side| [side, Hash.new(0)] }
    plans, reductions = {}, []
    %w[memory disk].each do |mode|
      cases = semantic.fetch(mode).fetch('workloads')
      observations = raw.fetch('observations').fetch(mode).fetch('workloads')
      assert(cases.map { |job| job.values_at('workload', 'api') } == expected &&
        observations.map { |job| job.values_at('workload', 'api') } == expected, 'direct API/storage coverage differs')
      cases.zip(observations).each do |job, observation|
        bindings = JoinPredicateDiagnostics.bindings.map { |value| [value] }
        assert(job['values'].map { |value| value['params'] } == bindings &&
          observation['values'].map { |value| value['params'] } == bindings, 'direct bindings differ')
        reduced = false
        observation.fetch('cache').each { |side, state| cache_state!(state, available && side == 'enabled') }
        job['values'].zip(observation['values']).each_with_index do |(value, observed), index|
          state = JoinPredicateDiagnostics::STATES.fetch(index)
          assert(value['state'] == state && observed['state'] == state && value.dig('outcome', 'status') == 'rows', 'direct state differs')
          plans[[mode, job['workload'], job['api'], index].join('/')] = direct_trace!(observed.fetch('trace'), totals)
          disabled_calls = observed['trace']['disabled']['calls'].fetch('join_kernel_calls', 0)
          enabled_calls = observed['trace']['enabled']['calls'].fetch('join_kernel_calls', 0)
          assert(disabled_calls.zero?, 'disabled JOIN tier executed a kernel')
          assert(enabled_calls.zero?, 'baseline executed new JOIN tier') unless available
          if enabled_calls.positive?
            reduced = true
            reductions << { storage: mode, workload: job['workload'], api: job['api'], state: state,
              actual_join_kernel_calls: enabled_calls }
          end
        end
        assert(reduced, "compiled JOIN path not exercised #{mode}/#{job['workload']}/#{job['api']}") if
          available && JoinPredicateDiagnostics::TARGETS.include?(job['workload'])
      end
      explains = semantic[mode].fetch('explains')
      assert(explains.map { |value| value['workload'] } == jobs.map { |job| job['name'] } &&
        explains.all? { |value| value['params'] == [17] && value['outcome']['status'] == 'rows' }, 'EXPLAIN coverage/outcome differs')
      budgets = semantic[mode].fetch('budgets')
      assert(budgets.map { |job| job.values_at('workload', 'api') } ==
        JoinPredicateDiagnostics::BUDGET_JOBS.product(JoinPredicateDiagnostics::APIS.map(&:to_s)), 'budget coverage differs')
      budgets.each do |job|
        ordinary = job.fetch('ordinary')
        cost = ordinary.fetch('cost')
        limits = [1, [cost / 2, 1].max, [cost - 1, 1].max, cost, cost + 1].uniq.sort
        assert(ordinary['status'] == 'rows' && job['params'] == [3] &&
          job['values'].map { |value| value.values_at('limit', 'setting') } == limits.product(%w[max_cost lifetime_max_cost]), 'budget schedule differs')
        job['values'].each do |value|
          outcome = value.fetch('outcome')
          if outcome['status'] == 'error'
            assert(outcome['sqlstate'] == (value['setting'] == 'max_cost' ? '54P01' : '54P02'), 'unexpected budget error')
          else
            assert(outcome == ordinary, 'completed budget outcome differs')
          end
          assert(outcome['status'] == 'error', 'below-cost budget unexpectedly completed') if value['limit'] < cost
          assert(outcome == ordinary, 'above-cost budget did not complete') if value['limit'] > cost
          # Exact-ceiling outcomes remain verbatim, without assuming all APIs
          # have the same success/error status at that boundary.
        end
      end
      mutations, traces = semantic[mode].fetch('mutations'), raw['observations'][mode].fetch('mutations')
      actions = [nil, 'ANALYZE ob', 'UPDATE ob SET score=200 WHERE id=1', 'CREATE INDEX ob_score ON ob(score)', 'DROP INDEX ob_score']
      expected_mutations = actions.product([[0, 1], [130, 1], [0, 2], [0, 3], [nil, 1], [0, 1]])
      assert(mutations.map { |value| value.values_at('after', 'params') } == expected_mutations &&
        traces.map { |value| value.values_at('after', 'params') } == expected_mutations, 'mutation coverage differs')
      mutations.zip(traces).each_with_index do |(value, observed), index|
        assert(value['outcome']['status'] == 'rows' && value['explain']['status'] == 'rows', 'mutation/EXPLAIN failed')
        plans["#{mode}/mutation/#{index}"] = direct_trace!(observed.fetch('trace'), totals)
      end
    end
    assert(!@direct_plans || @direct_plans == plans, 'current physical winners vary by source/runtime')
    @direct_plans = plans
    helpers = semantic.fetch('helpers')
    assert(helpers.map { |value| value['sql'] } == JoinPredicateDiagnostics::HELPER_SQL, 'helper predicate coverage differs')
    helpers.each do |job|
      assert(job.fetch('cases').length == 20, 'helper input count differs')
      job.fetch('cases').each do |value|
        outcome = value.fetch('outcome')
        assert(%w[value error].include?(outcome['status']) && outcome['cost'] == outcome['lifetime_cost'], 'helper outcome invalid')
        boundaries = (1..outcome.fetch('cost') + 1).to_a.product(%w[max_cost lifetime_max_cost])
        assert(value.fetch('boundaries').map { |item| item.values_at('limit', 'setting') } == boundaries, 'helper prefix schedule differs')
      end
    end
    cache = raw.fetch('cache_proof')
    assert(cache['available'] == available, 'cache proof availability differs')
    if available
      %w[second_use equal_program_reused fresh_pool_after_mutation disabled_cache_unchanged global_disabled immediate_materialized].each do |key|
        assert(cache[key] == true, "cache contract missing #{key}")
      end
      cache_state!(cache.fetch('admission_cache'), true)
      cache_state!(cache.fetch('during'), true)
      assert(cache['variants'] == 96 && cache['during'].values_at('retained', 'kernels') == [64, 64], 'JOIN churn coverage/bound differs')
      alive = cache.fetch('live_kernels_after_completed_thread_and_gc')
      assert(alive.length == 3 && alive.all? { |count| count.is_a?(Integer) && count.between?(0, 96) }, 'JOIN weak-reference observation invalid')
    else
      assert(cache == { 'available' => false }, 'baseline produced JOIN cache proof')
    end
    file = raw.fetch('storage').fetch('disk')
    assert(raw['storage']['memory'] == { 'kind' => 'memory' } && file['kind'] == 'file' &&
      file['locking'] == 'exclusive' && file['removed_after_run'] && !File.exist?(file['path']) &&
      file['scratch_directory'] == File.dirname(file['path']) && !File.exist?(file['scratch_directory']) &&
      file['path'].start_with?(File.join(ROOT, '.dev/ruby-native-join-predicate-diagnostics/')), 'direct storage differs')
    signature = file.values_at('bytes', 'sha256', 'filesystem_device')
    assert(file['bytes'].positive? && !file['sha256'].empty? && (!@file_signature || @file_signature == signature), 'file fixture bytes vary by source/runtime')
    @file_signature = signature
    { semantic_sha256: raw['semantic_sha256'], physical_plans_sha256: Digest::SHA256.hexdigest(JSON.generate(plans)),
      peak_rss_bytes: bound['peak_rss_bytes'], workload_api_cases: expected.length * 2,
      traced_invocations: expected.length * JoinPredicateDiagnostics::STATES.length * 2,
      mutation_cases: 60, helper_cases: helpers.length, executed_join_kernels: reductions,
      call_totals: totals, cache_proof: cache, storage: raw['storage'] }
  end

  def diagnostics
    results = JoinPredicateRuns::RUNTIMES.keys.to_h do |runtime_name|
      pair = %w[before final].to_h do |state|
        [state, direct_report("diagnostic-#{state}-#{runtime_name}", state == 'before' ? BASELINE : @source, runtime_name)]
      end
      [runtime_name, pair]
    end
    { runtimes: results,
      note: 'Untimed source-paired JOIN/control comparisons preserve complete values, metadata, costs, errors, consumed prefixes, selected plans and EXPLAIN. Internal cases exercise integer/NULL/text predicates, current bindings, errors and exact cost boundaries; cache proof verifies second use, changed pools, disable controls and eviction. TracePoint counts calls on kernels actually returned by the JOIN helper. These direct file cases cover durable file APIs, but their ordinary query paths do not spill; the focused tests separately exercise the spill seams. Some whole-query budget cases use tiny inputs below the compilation cutoff; actual compiled prefixes are checked by the helper cases and focused immediate-mode tests. Runtime JIT state is recorded; method-specific JIT proof is not claimed. The cache retains only bounded source/pool/kernel data.' }
  end

  def cross
    raw = read('cross-core')
    assert(raw.values_at('ruby_source_sha256', 'ruby_source_sha256_after') == [@source] * 2 &&
      raw['source_sha256'] == raw['source_sha256_after'], 'cross-core source changed')
    assert(raw.values_at('plan_path', 'plan_sha256', 'plan') == [PLAN, sha(PLAN), @main], 'cross-core plan differs')
    lanes = %w[go ruby-yjit rust ts]
    assert(raw.fetch('statuses').keys.sort == lanes && raw['statuses'].values.all? { |value| value['status'] == 'ready' } &&
      raw.fetch('results').keys.sort == lanes && raw.fetch('resources').keys.sort == lanes, 'cross-core failed or omitted lane')
    assert(raw.values_at('ruby_source', 'phase') == [SOURCE, 'join-predicate-cross-core'], 'cross-core selected source differs')
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
      environment: { 'RUBY_NATIVE_LANES' => 'ruby-yjit,go,rust,ts', 'RUBY_NATIVE_PHASE' => 'join-predicate-cross-core',
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
        assert(!options['runtime'] || JoinPredicateRuns::RUNTIMES.key?(options['runtime']), 'runner runtime differs')
        runtimes = options['runtime'] ? [options['runtime']] : JoinPredicateRuns::RUNTIMES.keys
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
    prefixes = JoinPredicateRuns::RUNTIMES.keys.map { |name| "control-#{name}" }
    prefixes.each do |prefix|
      assert(ordered.map { |entry| entry['name'] }.grep(/\A#{Regexp.escape(prefix)}-/) ==
        STATES.map { |state| "#{prefix}-#{state}" }, 'ABBA order differs')
    end
    assert(ordered.last(2).map { |entry| entry['name'] } == %w[before-profile final-profile], 'profiles were not last')
    { serial_commands: ordered.length, journals: reports, first_timed_started_at: first_timed,
      last_finished_at: ordered.last['finished_at'] }
  end

  def tables(summary)
    lines = ["# Join-predicate measurements", "", "Times are microseconds per complete query. Source comparisons average two process medians; each process retains five raw samples.", ""]
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
      %w[JedNative::CompiledPlans#native_compiled_join_predicate JedNative::Engine#evaluate JedNative::Engine#join JedNative::SpillSelect#spill_join JedNative::NWayJoins#nway_join_plan].each do |method|
        lines << format('| %s | %s | %.3f | %.3f |', name, method,
          row[:before_calls].fetch(method, 0), row[:final_calls].fetch(method, 0))
      end
    end
    File.write(File.join(__dir__, 'tables.md'), lines.join("\n") + "\n")
  end

  def main
    @source = NativeRubySource.digest(SOURCE)
    @main = JSON.parse(File.read(PLAN))
    assert(@main.values_at('rows', 'repetitions', 'validation_iterations') == [1000, 5, 32] &&
      @main['workloads'].length == 10, 'plan fixture differs')
    original = JSON.parse(File.read(JoinPredicateRuns::ORIGINAL_PLAN))
    assert(@main['workloads'].first(9) == original['workloads'] &&
      @main.values_at('rows', 'repetitions', 'validation_iterations') == original.values_at('rows', 'repetitions', 'validation_iterations'),
      'original nine workloads/schedules changed')
    @groups = { 'main' => { path: PLAN, jobs: @main['workloads'], fresh: false, filter: nil } }
    @answers, @schedules, @expected, @peaks = {}, {}, {}, []
    pins = pins!
    checks = correctness
    summary = { baseline_commit: JoinPredicateRuns::BASELINE_COMMIT, baseline_source_sha256: BASELINE,
      source_sha256: @source, correctness: checks.reject { |key, _| key == :provenance },
      provisional_smoke: smoke_fixture, comparisons: comparisons, diagnostics: diagnostics, cross: cross, profiles: profiles }
    summary[:execution] = journals(pins, checks)
    write('summary', summary)
    write('source', { final: checks[:provenance], baseline_commit: JoinPredicateRuns::BASELINE_COMMIT,
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
    paths += [File.join(ROOT, 'bench/ruby-native/join_predicate_diagnostics.rb'), PLAN,
      File.expand_path('../literal-like-followup/compact.rb', __dir__)]
    manifest = paths.uniq.sort.to_h do |path|
      relative = Pathname.new(path).relative_path_from(Pathname.new(__dir__)).to_s
      [relative, { bytes: File.size(path), sha256: sha(path) }]
    end
    write('manifest', manifest)
    puts "Join-predicate evidence verified: #{summary[:execution][:serial_commands]} serial commands, #{manifest.length} manifest files."
  end
end

JoinPredicateEvidence.main if $PROGRAM_NAME == __FILE__
