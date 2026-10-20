# frozen_string_literal: true

# Read-only verification precedes derived output and lossless JSON compaction.
# Run only after every correctness, benchmark and profile process has exited.
require 'json'
require 'digest'
require 'time'
require 'pathname'
require_relative 'run'
require_relative 'check'
require_relative '../../query_walk_diagnostics'
require_relative '../literal-like-followup/compact'

module QueryWalkEvidence
  ROOT = QueryWalkRuns::ROOT
  SOURCE = File.join(ROOT, 'impl/ruby-native')
  ARCHIVE = '/tmp/jed-query-shape-before/impl/ruby-native'
  BASELINE = QueryWalkRuns::BASELINE
  PLAN = QueryWalkRuns::PLAN
  CONTROL_PLAN = QueryWalkRuns::CONTROL_PLAN
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
    binary, jit = QueryWalkRuns::RUNTIMES.fetch(name)
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
      [QueryWalkRuns::BASELINE_COMMIT, ARCHIVE, BASELINE], 'baseline pins differ')
    assert(NativeRubySource.digest(ARCHIVE) == BASELINE, 'baseline archive changed')
    assert(pins.fetch('source_sha256') == @source, 'final source differs from pins')
    expected = QueryWalkRuns::PINNED_FILES + Dir.chdir(ROOT) do
      Dir['bench/ruby-native/compare/go/*', 'bench/ruby-native/compare/rust/src/**/*.rs',
        'bench/ruby-native/compare/rust/Cargo.*', 'bench/ruby-native/compare/lane.rb',
        'bench/ruby-native/compare/lane.ts']
    end
    assert(pins.fetch('files').keys.sort == expected.uniq.sort, 'pin file set differs')
    pins.fetch('files').each { |path, hash| assert(sha(File.join(ROOT, path)) == hash, "pinned file changed #{path}") }
    binaries = pins.fetch('runtime_files')
    ruby = QueryWalkRuns::RUNTIMES.values.map(&:first).uniq
    assert(binaries.length == 3 && (ruby - binaries.keys).empty? &&
      (binaries.keys - ruby).all? { |path| File.basename(path) == 'node' }, 'runtime pin set differs')
    binaries.each { |path, hash| assert(sha(path) == hash, "runtime changed #{path}") }
    pins
  end

  def correctness
    current = QueryWalkChecks.provenance
    specifications = QueryWalkChecks.specifications(phase: 'focus') +
      QueryWalkChecks.specifications(phase: 'host') + QueryWalkChecks.specifications(phase: 'corpus')
    reports = specifications.to_h do |spec|
      name = spec.fetch(:name)
      journal, bound = read("#{name}-run"), read("#{name}-bounded")
      assert(journal.values_at('source_before', 'source_after') == [current] * 2, 'correctness provenance changed')
      assert(journal['command_success'] && journal['verified'], 'correctness driver failed')
      assert(journal['specification'] == JSON.parse(JSON.generate(spec)), 'correctness specification differs')
      assert(journal['command'] == spec.fetch(:command), 'correctness command differs')
      env = QueryWalkChecks.environment(spec, File.join(__dir__, "#{name}-bounded.json"), File.join(__dir__, "#{name}.json"))
      assert(journal['environment'] == env, 'correctness environment differs')
      assert(journal['ruby_executable_sha256'] == sha(spec.fetch(:binary)), 'correctness executable changed')
      report = spec[:phase] == 'corpus' ? read(name) : nil
      QueryWalkChecks.verify!(spec, bound, report, current)
      [name, { passed: report && report.fetch('passed'), peak_rss_bytes: bound.fetch('peak_rss_bytes'),
        started_at: journal.fetch('started_at'), finished_at: journal.fetch('finished_at') }]
    end
    reports.values.sort_by { |report| Time.iso8601(report[:started_at]) }.each_cons(2) do |left, right|
      assert(Time.iso8601(left[:finished_at]) <= Time.iso8601(right[:started_at]),
        'correctness workers overlapped')
    end
    { records: reports.values.sum { |value| value[:passed].to_i }, failed: 0,
      host_files: current.fetch('host_files'), corpus_files: current.fetch('corpus_files'),
      max_rss_bytes: reports.values.map { |value| value[:peak_rss_bytes] }.max,
      reports: reports, provenance: current }
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
      command = [identity[:binary], *QueryWalkRuns.flags(identity[:jit]),
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
      binding("verify-supplemental-#{state}", source, '4.0-yjit', 'supplemental', ['wrapper', *NATIVE], verify: true)
      binding("verify-fresh-#{state}", source, '4.0-yjit', 'fresh', %w[wrapper native-query native-eager], verify: true)
    end
    series = {}
    [['main', QueryWalkRuns::RUNTIMES.keys, NATIVE, 'control'],
      ['supplemental', ['4.0-yjit'], NATIVE, 'supplemental'],
      ['fresh', ['4.0-yjit'], %w[native-query native-eager], 'fresh']].each do |group, runtimes, lanes, prefix|
      series[group] = runtimes.to_h do |runtime_name|
        runs = STATES.map do |state|
          binding("#{prefix}-#{runtime_name}-#{state}", state.end_with?('before') ? BASELINE : @source, runtime_name, group, lanes)
        end
        [runtime_name, lanes.to_h do |api|
          [api, @groups.fetch(group).fetch(:jobs).to_h do |job|
            [job['name'], paired(runs.map { |run| run.fetch(api).fetch(job['name']) })]
          end]
        end]
      end
    end
    actual = binding('binding-4.0-yjit', @source, '4.0-yjit', 'main', ['wrapper', *NATIVE])
    wrapper = actual.fetch('wrapper').to_h do |name, value|
      [name, { wrapper_us: value / 1000.0, native: NATIVE.to_h do |api|
        time = actual.fetch(api).fetch(name)
        [api, { us: time / 1000.0, native_over_wrapper: time.fdiv(value) }]
      end }]
    end
    slower = series.flat_map do |group, runtimes|
      runtimes.flat_map do |runtime_name, apis|
        apis.flat_map do |api, jobs|
          jobs.filter_map { |name, row| { group: group, runtime: runtime_name, api: api, workload: name }.merge(row) if row[:elapsed_change_percent].positive? }
        end
      end
    end
    { paired: series, slower_cells: slower, actual_binding: wrapper,
      exact_values_metadata_costs: @answers, schedules: @schedules, max_rss_bytes: @peaks.max,
      note: 'ABBA statistics use the mean of two process medians per source. Separation is descriptive, not a statistical significance test. Fresh SQL has only unprepared query/eager lanes. Binding ratios are separate single-process measurements.' }
  end

  def diagnostics
    reference = nil
    QueryWalkRuns::RUNTIMES.keys.to_h do |name|
      raw, bound, identity = read("diagnostic-#{name}"), read("diagnostic-#{name}-bounded"), runtime(name)
      artifact = File.join(__dir__, "diagnostic-#{name}.json")
      bounded!(bound, @source, [identity[:binary], *QueryWalkRuns.flags(identity[:jit]),
        'bench/ruby-native/query_walk_diagnostics.rb', '--jit', identity[:jit], '--report', artifact])
      @expected["diagnostic-#{name}"] = { command: ['mise', 'run', 'ruby-native:bounded', '--', *bound.fetch('command')],
        source: SOURCE, hash: @source, environment: { 'JED_NATIVE_RUN_REPORT' => File.join(__dir__, "diagnostic-#{name}-bounded.json") } }
      assert(raw.values_at('source_sha256', 'final_source_sha256') == [@source] * 2 &&
        raw['source'] == SOURCE && raw['mode'] == 'untimed_query_walk_diagnostic', 'direct source/mode differs')
      assert(raw['runtime'].start_with?(identity[:version]) && raw['ruby_executable'] == identity[:binary] &&
        raw['ruby_executable_sha256'] == sha(identity[:binary]), 'direct runtime differs')
      assert(raw['jit'] == { 'yjit' => identity[:jit] == 'yjit', 'zjit' => identity[:jit] == 'zjit' }, 'direct JIT differs')
      assert(raw['script_sha256'] == sha(File.join(ROOT, 'bench/ruby-native/query_walk_diagnostics.rb')), 'direct script changed')
      assert(raw['plans'] == [PLAN, CONTROL_PLAN].to_h { |path| [path, sha(path)] }, 'direct plans changed')
      jobs = @main.fetch('workloads') + @control.fetch('workloads')
      assert(raw['jobs'] == jobs && raw['source_targets'] == QueryWalkDiagnostics::SOURCE_TARGETS, 'direct jobs differ')
      assert(raw['limits'] == { 'join_entries' => 64, 'sources_per_entry' => 8, 'source_units' => 4096, 'source_bytes' => 65536 }, 'direct bounds differ')
      semantic = raw.fetch('semantic')
      assert(raw['semantic_sha256'] == Digest::SHA256.hexdigest(JSON.generate(semantic)), 'direct semantic hash differs')
      assert(!reference || reference == semantic, 'direct semantics vary by runtime')
      reference = semantic
      expected = jobs.flat_map do |job|
        (QueryWalkDiagnostics::FRESH_JOBS.include?(job['name']) ? %w[query execute] : QueryWalkDiagnostics::APIS.map(&:to_s)).map { |api| [job['name'], api] }
      end
      %w[memory disk].each do |mode|
        cases = semantic.fetch(mode).fetch('workloads')
        observations = raw.fetch('observations').fetch(mode).fetch('calls')
        assert(cases.map { |job| job.values_at('workload', 'api') } == expected &&
          observations.map { |job| job.values_at('workload', 'api') } == expected, 'direct coverage differs')
        totals = { 'enabled' => Hash.new(0), 'disabled' => Hash.new(0) }
        cases.zip(observations).each do |job, observation|
          bindings = QueryWalkDiagnostics.bindings(job['workload']).map { |value| [value] }
          assert(job['values'].map { |value| value['params'] } == bindings &&
            observation['values'].map { |value| value['params'] } == bindings, 'direct bindings differ')
          assert(job['syntax_units'] > 1024, 'oversized control is too small') if job['workload'] == 'oversized_syntax'
          job['values'].zip(observation['values']).each_with_index do |(value, observed), index|
            state = QueryWalkDiagnostics.states(job['workload']).fetch(index)
            assert(value['state'] == state && observed['state'] == state && value.dig('outcome', 'status') == 'rows', 'direct state/outcome differs')
            calls = observed.fetch('calls')
            calls.each do |side, counts|
              counts.each { |method, count| assert(count.is_a?(Integer) && count >= 0, 'invalid trace count'); totals.fetch(side)[method] += count }
            end
            QueryWalkDiagnostics::ORIGINALS.each do |method, klass|
              original, replacement = "#{klass}##{method}", "JedNative::QueryWalks##{method}"
              assert(calls['enabled'].fetch(original, 0).zero? &&
                calls['enabled'].fetch(replacement, 0) <= calls['disabled'].fetch(original, 0), 'walker call-count proof differs')
            end
            %w[JedNative::JoinPlanner#two_base_join_plan JedNative::NWayJoins#nway_join_plan].each do |method|
              assert(calls['enabled'].fetch(method, 0) == calls['disabled'].fetch(method, 0), 'physical admission changed')
            end
            observed.fetch('cache').each do |side, cache|
              assert(cache['entries'].between?(0, 64) && cache['per_entry'].length == cache['entries'], 'metadata entry bounds differ')
              assert(cache['source_entries'] == cache['per_entry'].sum { |item| item['sources'] }, 'metadata entry total differs')
              assert(cache['source_entries'].zero?, 'disabled metadata cache retained entries') if side == 'disabled'
              cache['per_entry'].each do |item|
                assert(item['sources'].between?(0, 8) && item['units'].between?(0, 4096) &&
                  item['bytes'].between?(0, 65536) && item['owned_frozen'], 'metadata ownership/bounds differ')
              end
            end
          end
          if QueryWalkDiagnostics::SOURCE_TARGETS.include?(job['workload'])
            reused = observation['values'].any? do |value|
              method = 'JedNative::QueryColumns#query_source_columns'
              %w[warm warm_again].include?(value['state']) && value.dig('cache', 'enabled', 'source_entries').positive? &&
                value['calls']['enabled'].fetch(method, 0) < value['calls']['disabled'].fetch(method, 0)
            end
            assert(reused, 'retained source metadata was never reused')
          end
        end
        assert(totals == raw['observations'][mode]['totals'], 'direct call totals differ')
        budgets = semantic[mode].fetch('budgets')
        expected_budgets = %w[three_table_join three_table_join_range positive_window ordinary_aggregate].product(QueryWalkDiagnostics::APIS.map(&:to_s))
        assert(budgets.map { |job| job.values_at('workload', 'api') } == expected_budgets, 'budget coverage differs')
        budgets.each do |job|
          cost = job.fetch('ordinary').fetch('cost')
          limits = [1, [cost / 2, 1].max, [cost - 1, 1].max, cost, cost + 1].uniq.sort
          assert(job['params'] == [3] && job['values'].map { |value| value.values_at('limit', 'setting') } ==
            limits.product(%w[max_cost lifetime_max_cost]), 'budget schedule differs')
          # At the exact ceiling, a completed blocking collector can return
          # before another limit check. Pin those source-local API cases; the
          # diagnostic also compares every full outcome with the disabled path.
          eager = %w[execute_prepared execute].include?(job['api'])
          boundary_success = job['workload'] == 'positive_window' && eager ||
            job['workload'] == 'ordinary_aggregate' && (mode == 'disk' || eager)
          job['values'].each do |value|
            outcome = value.fetch('outcome')
            if value['limit'] > cost || value['limit'] == cost && boundary_success
              assert(outcome == job['ordinary'], 'sufficient budget changed outcome')
            else
              assert(outcome['status'] == 'error' && outcome['sqlstate'] ==
                (value['setting'] == 'max_cost' ? '54P01' : '54P02'), 'budget error differs')
            end
          end
        end
      end
      file = raw.fetch('storage').fetch('disk')
      assert(raw['storage']['memory'] == { 'kind' => 'memory' } && file['kind'] == 'file' &&
        file['locking'] == 'exclusive' && file['removed_after_run'] && !File.exist?(file['path']) &&
        file['path'].start_with?(File.join(ROOT, '.dev/ruby-native-query-walk-diagnostics/')), 'direct storage/cleanup differs')
      signature = file.values_at('bytes', 'sha256', 'filesystem_device')
      assert(!@file_signature || @file_signature == signature, 'direct file fixture varies by runtime')
      @file_signature = signature
      [name, { semantic_sha256: raw['semantic_sha256'], peak_rss_bytes: bound['peak_rss_bytes'],
        workload_api_cases: expected.length * 2, traced_invocations: expected.length * 10,
        totals: raw['observations'].transform_values { |mode| mode['totals'] }, storage: raw['storage'] }]
    end
  end

  def cross
    raw = read('cross-core')
    assert(raw.values_at('ruby_source_sha256', 'ruby_source_sha256_after') == [@source] * 2 &&
      raw['source_sha256'] == raw['source_sha256_after'], 'cross-core source changed')
    assert(raw.values_at('plan_path', 'plan_sha256', 'plan') == [PLAN, sha(PLAN), @main], 'cross-core plan differs')
    lanes = %w[go ruby-yjit rust ts]
    assert(raw.fetch('statuses').keys.sort == lanes && raw['statuses'].values.all? { |value| value['status'] == 'ready' } &&
      raw.fetch('results').keys.sort == lanes && raw.fetch('resources').keys.sort == lanes, 'cross-core failed or omitted lane')
    assert(raw.values_at('ruby_source', 'phase') == [SOURCE, 'query-walk-cross-core'], 'cross-core selected source differs')
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
      environment: { 'RUBY_NATIVE_LANES' => 'ruby-yjit,go,rust,ts', 'RUBY_NATIVE_PHASE' => 'query-walk-cross-core',
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
    %w[verify diagnostics controls supplemental cross binding profiles].each do |phase|
      paths = Dir[File.join(__dir__, "run-#{phase}*.json")].sort
      assert(!paths.empty?, "missing #{phase} journal")
      paths.each do |path|
        raw = JSON.parse(File.read(path))
        options = raw.fetch('options')
        assert(raw['phase'] == phase && options['phase'] == phase &&
          raw['script_sha256'] == sha(File.join(__dir__, 'run.rb')) && raw['plan_sha256'] == sha(PLAN), 'runner provenance changed')
        assert(options.values_at('final_hash', 'baseline_source') == [@source, ARCHIVE], 'runner selected source differs')
        assert(!options['runtime'] || QueryWalkRuns::RUNTIMES.key?(options['runtime']), 'runner runtime differs')
        runtimes = options['runtime'] ? [options['runtime']] : QueryWalkRuns::RUNTIMES.keys
        names = case phase
                when 'verify' then %w[before final].flat_map { |state| ["verify-#{state}", "verify-supplemental-#{state}", "verify-fresh-#{state}"] }
                when 'diagnostics' then runtimes.map { |runtime_name| "diagnostic-#{runtime_name}" }
                when 'controls' then runtimes.flat_map { |runtime_name| STATES.map { |state| "control-#{runtime_name}-#{state}" } }
                when 'supplemental' then %w[supplemental fresh].flat_map { |group| STATES.map { |state| "#{group}-4.0-yjit-#{state}" } }
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
    assert(commands.length == 33 && commands.map { |entry| entry['name'] }.sort == @expected.keys.sort, 'duplicate/missing command')
    ordered = commands.sort_by { |entry| Time.iso8601(entry['started_at']) }
    ordered.each_cons(2) { |a, b| assert(Time.iso8601(a['finished_at']) <= Time.iso8601(b['started_at']), 'workload commands overlapped') }
    first_timed = ordered.find { |entry| !entry['name'].start_with?('verify-', 'diagnostic-') }.fetch('started_at')
    assert(Time.iso8601(pins['captured_at']) <= Time.iso8601(ordered.first['started_at']), 'pins captured after execution started')
    ordered.select { |entry| entry['name'].start_with?('verify-', 'diagnostic-') }.each do |entry|
      assert(Time.iso8601(entry['finished_at']) <= Time.iso8601(first_timed), 'verification occurred during/after timing')
    end
    correctness.fetch(:reports).each_value do |report|
      assert(Time.iso8601(report[:finished_at]) <= Time.iso8601(first_timed), 'correctness overlapped timing')
    end
    regression_phases = %w[native supported oracle]
    regression_phases.each do |phase|
      report = read("flat-in-#{phase}")
      finished = report.fetch('commands').all? do |entry|
        entry['success'] && Time.iso8601(entry.fetch('finished_at')) <= Time.iso8601(first_timed)
      end
      assert(report['verified'] && finished, 'flat-IN regression overlapped timing')
    end
    prefixes = QueryWalkRuns::RUNTIMES.keys.map { |name| "control-#{name}" } + %w[supplemental-4.0-yjit fresh-4.0-yjit]
    prefixes.each do |prefix|
      assert(ordered.map { |entry| entry['name'] }.grep(/\A#{Regexp.escape(prefix)}-/) ==
        STATES.map { |state| "#{prefix}-#{state}" }, 'ABBA order differs')
    end
    assert(ordered.last(2).map { |entry| entry['name'] } == %w[before-profile final-profile], 'profiles were not last')
    { serial_commands: ordered.length, journals: reports, first_timed_started_at: first_timed,
      last_finished_at: ordered.last['finished_at'],
      regression_phases_finished_before_timing: regression_phases }
  end

  def tables(summary)
    lines = ["# Query-walk measurements", "", "All times are microseconds per complete query. Source comparisons use means of two process medians; each process has five samples.", ""]
    summary[:comparisons][:paired].each do |group, runtimes|
      runtimes.each do |runtime_name, apis|
        lines += ["## #{group}: #{runtime_name}", "", "| API | Workload | Before | Final | Speedup | Before process range | Final process range |", "|---|---|---:|---:|---:|---|---|"]
        apis.each do |api, jobs|
          jobs.each do |name, row|
            ranges = %i[before_range_us final_range_us].map { |key| row[key].map { |value| format('%.3f', value) }.join('–') }
            lines << format('| %s | %s | %.3f | %.3f | %.3f× | %s | %s |', api, name, row[:before_us], row[:final_us], row[:speedup], *ranges)
          end
        end
        lines << ''
      end
    end
    lines += ["## Slower observations", "", "| Group | Runtime | API | Workload | Elapsed change | Both finals above both befores |", "|---|---|---|---|---:|---|"]
    summary[:comparisons][:slower_cells].each do |row|
      lines << format('| %s | %s | %s | %s | +%.3f%% | %s |', row[:group], row[:runtime], row[:api], row[:workload], row[:elapsed_change_percent], row[:separated_slower] ? 'yes' : 'no')
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
    lines += ["", "## Profile allocations", "", "| Workload | Before objects/query | Final objects/query | Change |", "|---|---:|---:|---:|"]
    summary[:profiles][:comparisons].each do |name, row|
      lines << format('| %s | %.3f | %.3f | %+.3f%% |', name, row[:before_allocations], row[:final_allocations], row[:allocation_change_percent])
    end
    lines += ["", "## Profile calls (counts, not CPU-time percentages)", "",
      "| Workload | Method | Before | Final |", "|---|---|---:|---:|"]
    methods = QueryWalkDiagnostics::ORIGINALS.to_h do |method, klass|
      ["#{klass}##{method}", "JedNative::QueryWalks##{method}"]
    end.merge('JedNative::QueryColumns#query_source_columns' => 'JedNative::QueryColumns#query_source_columns',
      'JedNative::PreparedJoins#query_source_columns' => 'JedNative::PreparedJoins#query_source_columns')
    summary[:profiles][:comparisons].each do |name, row|
      methods.each do |original, replacement|
        lines << format('| %s | %s → %s | %.3f | %.3f |', name, original, replacement,
          row[:before_calls].fetch(original, 0), row[:final_calls].fetch(replacement, 0))
      end
    end
    warmup = summary.fetch(:warmup_diagnostic)
    lines += ["", "## Adaptive ZJIT prepared three-table follow-up", "",
      "These separate runs retain the original 94 paired cells above. Only the target's warmup changes in the long condition.",
      "", "| Condition | Before | Final | Speedup | Elapsed change |", "|---|---:|---:|---:|---:|"]
    warmup_rows = { 'primary warmup 100' => warmup.fetch(:primary_target) }
    warmup.fetch(:paired_workloads).each do |condition, jobs|
      warmup_rows["adaptive #{condition}"] = jobs.find { |job| job.fetch(:name) == 'three_table_join' }
    end
    warmup_rows.each do |condition, row|
      lines << format('| %s | %.3f | %.3f | %.3f× | %+.3f%% |', condition,
        row[:before_mean_ns] / 1000.0, row[:final_mean_ns] / 1000.0,
        row[:speedup], row[:elapsed_change_percent])
    end
    File.write(File.join(__dir__, 'tables.md'), lines.join("\n") + "\n")
  end

  def compact_reports
    preserved = %w[flat-in-original-before-bounded.json flat-in-original-controls.json]
    ledger = File.file?(File.join(__dir__, 'serialization.json')) ? read('serialization') : {
      'format' => 'UTF-8 JSON.generate(JSON.parse(original_bytes)) followed by one newline',
      'preservation' => 'All parsed JSON values remain unchanged. Original flat-IN evidence and nested adaptive evidence remain byte-for-byte unchanged.',
      'files' => {}
    }
    previous = ledger.fetch('preserved_files', {})
    ledger['preserved_files'] = preserved.to_h do |name|
      path = File.join(__dir__, name)
      identity = { 'bytes' => File.size(path), 'sha256' => sha(path) }
      assert(!previous.key?(name) || previous.fetch(name) == identity, 'original evidence bytes changed')
      [name, identity]
    end
    Dir[File.join(__dir__, '*.json')].sort.each do |path|
      name = File.basename(path)
      next if preserved.include?(name) || %w[serialization.json manifest.json].include?(name)
      recorded = ledger.fetch('files')[name]
      next if recorded && recorded.fetch('compact_file_sha256') == sha(path)
      ledger.fetch('files')[name] = LiteralLikeEvidenceJSON.compact(path)
    end
    ledger
  end

  def main
    @source = NativeRubySource.digest(SOURCE)
    @main, @control = [PLAN, CONTROL_PLAN].map { |path| JSON.parse(File.read(path)) }
    assert(@main.values_at('rows', 'repetitions', 'validation_iterations') == [1000, 5, 32] &&
      @main['workloads'].length == 9 && @control.values_at('rows', 'repetitions', 'validation_iterations') == [1000, 5, 32], 'plan fixture differs')
    assert(@control['workloads'].map { |job| job['name'] } == QueryWalkRuns::CONTROL_JOBS + QueryWalkRuns::FRESH_JOBS,
      'supplemental workload set differs')
    @groups = {
      'main' => { path: PLAN, jobs: @main['workloads'], fresh: false, filter: nil },
      'supplemental' => { path: CONTROL_PLAN, jobs: @control['workloads'].select { |job| QueryWalkRuns::CONTROL_JOBS.include?(job['name']) },
        fresh: false, filter: QueryWalkRuns::CONTROL_JOBS },
      'fresh' => { path: CONTROL_PLAN, jobs: @control['workloads'].select { |job| QueryWalkRuns::FRESH_JOBS.include?(job['name']) },
        fresh: true, filter: QueryWalkRuns::FRESH_JOBS }
    }
    @answers, @schedules, @expected, @peaks = {}, {}, {}, []
    pins = pins!
    checks = correctness
    summary = { baseline_commit: QueryWalkRuns::BASELINE_COMMIT, baseline_source_sha256: BASELINE,
      source_sha256: @source, correctness: checks.reject { |key, _| key == :provenance },
      comparisons: comparisons, diagnostics: diagnostics, cross: cross, profiles: profiles }
    summary[:execution] = journals(pins, checks)
    require_relative 'flat_in_evidence'
    summary[:flat_in_regression] = QueryWalkFlatInChecks.verify!(@source)
    require_relative 'warmup-diagnostic/evidence'
    summary[:warmup_diagnostic] = QueryWalkWarmupEvidence.verify!(@source).merge(
      verified: true, evidence_directory: 'warmup-diagnostic')
    summary[:execution][:adaptive_serial_commands] = summary[:warmup_diagnostic].fetch(:command_count)
    summary[:execution][:total_serial_commands] = summary[:execution][:serial_commands] +
      summary[:execution][:adaptive_serial_commands]
    write('summary', summary)
    write('source', { final: checks[:provenance], baseline_commit: QueryWalkRuns::BASELINE_COMMIT,
      baseline_source_sha256: BASELINE, benchmark_pins: pins,
      verifier_dependencies: %w[check.rb flat_in_checks.rb flat_in_evidence.rb
        warmup-diagnostic/evidence.rb
        ../literal-like-followup/compact.rb].to_h { |path| [path, sha(File.expand_path(path, __dir__))] } })
    tables(summary)
    ledger = compact_reports
    ledger.fetch('files').each do |name, entry|
      path = File.join(__dir__, name)
      assert(File.size(path) == entry['compact_bytes'] && sha(path) == entry['compact_file_sha256'] &&
        Digest::SHA256.hexdigest(JSON.generate(JSON.parse(File.read(path)))) == entry['parsed_json_sha256'], 'lossless serialization changed values')
    end
    write('serialization', ledger)
    paths = Dir[File.join(__dir__, '**/*')].select { |path| File.file?(path) && File.basename(path) != 'manifest.json' }
    paths += [File.join(ROOT, 'bench/ruby-native/query_walk_diagnostics.rb'), CONTROL_PLAN, PLAN,
      File.expand_path('../literal-like-followup/compact.rb', __dir__)]
    manifest = paths.uniq.sort.to_h do |path|
      relative = Pathname.new(path).relative_path_from(Pathname.new(__dir__)).to_s
      [relative, { bytes: File.size(path), sha256: sha(path) }]
    end
    write('manifest', manifest)
    puts "Query-walk evidence verified: #{summary[:execution][:serial_commands]} primary + " \
      "#{summary[:execution][:adaptive_serial_commands]} adaptive serial commands, #{manifest.length} manifest files."
  end
end

QueryWalkEvidence.main if $PROGRAM_NAME == __FILE__
