# frozen_string_literal: true

require_relative 'run'

module QueryWalkWarmupEvidence
  TARGET = 'three_table_join'
  SEMANTIC_KEYS = %w[name sql kind validation_parameters validation_rows validation_costs
    validation_columns validation_column_types validation_sha256 answer_sha256 callback_counts].freeze
  module_function

  def read(name)
    JSON.parse(File.read(File.join(__dir__, name)))
  end

  def assert(condition, message)
    raise "Warmup diagnostic: #{message}" unless condition
  end

  def signature(job)
    job.values_at(*SEMANTIC_KEYS)
  end

  def source_for(state)
    state.end_with?('before') ? QueryWalkWarmupRuns::BASELINE_HASH : QueryWalkWarmupRuns::FINAL_HASH
  end

  def runtime(result, pins)
    assert(result.fetch('ruby_executable') == pins.fetch('runtime'), 'runtime path')
    assert(result.fetch('ruby_executable_sha256') == pins.fetch('runtime_sha256'), 'runtime digest')
    assert(result.fetch('zjit') == true && result.fetch('yjit') == false, 'actual JIT')
    assert(result.fetch('runtime').include?('ruby 4.1.0preview1'), 'runtime version')
  end

  def value_deltas(before, after)
    before.keys.intersection(after.keys).to_h { |key| [key, after.fetch(key) - before.fetch(key)] }
  end

  def pair(reports, name)
    jobs = reports.map { |report| report.fetch('workloads').find { |job| job.fetch('name') == name } }
    medians = jobs.map { |job| job.fetch('median_ns') }
    before = [medians[0], medians[3]]
    final = medians[1, 2]
    before_mean = before.sum.fdiv(2)
    final_mean = final.sum.fdiv(2)
    {
      name: name, process_medians_ns: medians, before_mean_ns: before_mean, final_mean_ns: final_mean,
      elapsed_change_percent: (final_mean / before_mean - 1) * 100,
      speedup: before_mean / final_mean,
      separated_slower: final.min > before.max, separated_faster: final.max < before.min,
      samples_ns_per_operation: jobs.map { |job| job.fetch('samples_ns').map { |sample| sample.fdiv(job.fetch('iterations')) } }
    }
  end

  def verify!(final_hash)
    assert(final_hash == QueryWalkWarmupRuns::FINAL_HASH, 'requested source differs')
    QueryWalkWarmupRuns.validate_pins
    pins = read('pins.json')
    plans = %w[short long].to_h { |condition| [condition, read("#{condition}-plan.json")] }
    primary = %w[1-before 2-final 3-final 4-before].map do |state|
      read("../control-4.1-zjit-#{state}.json").fetch('lanes').fetch('native-prepared').fetch('result')
    end
    expected_jobs = primary.first.fetch('workloads')
    names = expected_jobs.map { |job| job.fetch('name') }
    names.each_with_index do |name, index|
      assert(primary.all? { |result| signature(result.fetch('workloads')[index]) == signature(expected_jobs[index]) },
        "primary signature #{name}")
    end
    journal = read('run-all.json')
    assert(journal.fetch('script_sha256') == pins.fetch('files').fetch('run.rb'), 'journal runner')
    assert(journal.fetch('pins_sha256') == Digest::SHA256.file(File.join(__dir__, 'pins.json')).hexdigest, 'journal pins')
    expected_names = %w[short long].flat_map { |condition| %w[1-before 2-final 3-final 4-before].map { |state| "#{condition}-#{state}" } } +
      %w[short long].flat_map { |condition| %w[before final].map { |state| "stats-#{condition}-#{state}" } }
    commands = journal.fetch('commands')
    assert(commands.map { |command| command.fetch('name') } == expected_names, 'twelve ordered processes')
    profile_finish = Time.iso8601(read('../run-profiles.json').fetch('commands').last.fetch('finished_at'))
    previous = profile_finish
    commands.each do |command|
      started = Time.iso8601(command.fetch('started_at'))
      finished = Time.iso8601(command.fetch('finished_at'))
      assert(started >= previous && finished >= started, 'serial command intervals after primary profiles')
      previous = finished
      assert(command.fetch('success'), "command failed #{command.fetch('name')}")
      expected = source_for(command.fetch('name'))
      assert(command.values_at('source_sha256', 'final_source_sha256') == [expected, expected], 'journal source stable')
      env = command.fetch('environment')
      assert(env.values_at('JED_NATIVE_MEMORY_BYTES', 'JED_NATIVE_ADDRESS_SPACE_BYTES', 'JED_NATIVE_TIMEOUT') ==
        ['536870912', '2147483648', '300'], 'requested bounds')
      assert(env.values_at('RUBYOPT', 'RUBY_YJIT_ENABLE', 'RUBY_ZJIT_ENABLE') == [nil, nil, nil], 'JIT environment')
    end

    timing = {}
    peak_rss = []
    %w[short long].each do |condition|
      plan = plans.fetch(condition)
      reports = %w[1-before 2-final 3-final 4-before].map do |state|
        name = "#{condition}-#{state}"
        outer = read("#{name}.json")
        assert(outer.fetch('errors').empty?, "timing errors #{name}")
        assert(outer.fetch('source_sha256') == outer.fetch('source_sha256_after'), "timing source changed #{name}")
        assert(outer.fetch('source_sha256').fetch('native') == source_for(state), 'timing source')
        assert(outer.fetch('lanes').keys == ['native-prepared'], 'only prepared lane')
        lane = outer.fetch('lanes').fetch('native-prepared')
        assert(lane.fetch('success') && lane['reason'].nil?, 'successful bounded timing worker')
        assert(lane.values_at('memory_limit_bytes', 'address_space_bytes') == [536_870_912, 2_147_483_648], 'timing limits')
        peak_rss << lane.fetch('peak_rss_bytes')
        result = lane.fetch('result')
        runtime(result, pins)
        assert(result.values_at('source_sha256', 'final_source_sha256') == [source_for(state)] * 2, 'worker source')
        assert(result.fetch('script_sha256') == pins.fetch('primary_source_pins').fetch('files').fetch('bench/ruby-native/binding_compare_lane.rb'), 'worker hash')
        assert(result.fetch('plan_sha256') == pins.fetch('files').fetch("#{condition}-plan.json"), 'timing plan')
        assert(result.fetch('dataset_rows') == 1000 && result.fetch('repetitions') == 5, 'fixture/repetitions')
        assert(result.fetch('workloads').map { |job| job.fetch('name') } == names, 'timing job order')
        result.fetch('workloads').each_with_index do |job, index|
          planned = plan.fetch('workloads')[index]
          assert(signature(job) == signature(expected_jobs[index]), "exact values, metadata and costs #{name}/#{job['name']}")
          assert(job.values_at('warmup', 'iterations') == planned.values_at('warmup', 'iterations'), 'timing schedule')
          samples = job.fetch('samples_ns')
          assert(samples.length == 5 && samples.all? { |sample| sample.is_a?(Integer) && sample.positive? }, 'five positive samples')
          assert(job.fetch('median_ns') == samples.sort[2].fdiv(job.fetch('iterations')), 'per-process median')
          assert(job.fetch('rows_per_sample') == expected_jobs[index].fetch('rows_per_sample'), 'drained row counts')
        end
        result
      end
      timing[condition] = names.map { |name| pair(reports, name) }
    end

    statistics = {}
    %w[short long].each do |condition|
      %w[before final].each do |state|
        name = "stats-#{condition}-#{state}"
        result = read("#{name}.json")
        bounded = read("#{name}-bounded.json")
        assert(bounded.fetch('success') && bounded['reason'].nil?, "statistics bound #{name}")
        assert(bounded.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') ==
          [536_870_912, 2_147_483_648, 300.0], 'statistics limits')
        assert(bounded.values_at('source_sha256', 'final_source_sha256') == [source_for(state)] * 2, 'statistics bounded source')
        assert(bounded.fetch('command')[0, 3] == [pins.fetch('runtime'), '--zjit', '--zjit-stats-quiet'], 'stats quiet flag')
        peak_rss << bounded.fetch('peak_rss_bytes')
        runtime(result, pins)
        assert(result.values_at('source_sha256', 'final_source_sha256') == [source_for(state)] * 2, 'statistics source')
        assert(result.fetch('script_sha256') == pins.fetch('files').fetch('stats.rb'), 'stats script')
        assert(result.fetch('worker_sha256') == pins.fetch('primary_source_pins').fetch('files').fetch('bench/ruby-native/binding_compare_lane.rb'), 'stats shared helper')
        assert(result.fetch('plan_sha256') == pins.fetch('files').fetch("#{condition}-plan.json"), 'statistics plan')
        assert(result.fetch('validation').map { |job| signature(job) } == expected_jobs.map { |job| signature(job) }, 'statistics semantic signatures')
        assert(result.fetch('replay').map { |job| job.fetch('name') } == names, 'statistics full replay order')
        result.fetch('replay').each_with_index do |job, index|
          planned = plans.fetch(condition).fetch('workloads')[index]
          assert(job.values_at('warmup', 'iterations') == planned.values_at('warmup', 'iterations'), 'statistics schedule')
          assert(job.fetch('rows_per_sample') == expected_jobs[index].fetch('rows_per_sample'), 'statistics drained rows')
          %w[warmup iterations].each do |count_key|
            params = Array.new(planned.fetch(count_key)) { |i| [planned.fetch('offset') + (i * planned.fetch('step')) % planned.fetch('span')] }
            hash_key = count_key == 'warmup' ? 'warmup_parameters_sha256' : 'batch_parameters_sha256'
            assert(job.fetch(hash_key) == Digest::SHA256.hexdigest(JSON.generate(params)), 'statistics binding schedule')
          end
        end
        observations = result.fetch('observations')
        labels = %w[before_warmup after_warmup] + (1..5).map { |i| "after_batch_#{i}" }
        assert(observations.map { |observation| observation.fetch('label') } == labels, 'counter checkpoints')
        assert(observations.all? { |observation| observation.fetch('zjit').fetch('compiled_iseq_count').positive? }, 'actual compiled ISeq counters')
        deltas = observations.each_cons(2).map do |before, after|
          {
            from: before.fetch('label'), to: after.fetch('label'),
            zjit: value_deltas(before.fetch('zjit'), after.fetch('zjit')),
            gc: value_deltas(before.fetch('gc'), after.fetch('gc')),
            gc_total_time_ns: before['gc_total_time_ns'] && after.fetch('gc_total_time_ns') - before.fetch('gc_total_time_ns')
          }
        end
        statistics["#{condition}-#{state}"] = { gc_measure_total_time: result['gc_measure_total_time'], deltas: deltas }
      end
    end
    assert(peak_rss.all? { |rss| rss.positive? && rss <= 536_870_912 }, 'observed RSS bounds')
    {
      schema: 1, adaptive_followup: true, command_count: commands.length,
      started_at: commands.first.fetch('started_at'), finished_at: commands.last.fetch('finished_at'),
      primary_target: pair(primary, TARGET), paired_workloads: timing, statistics: statistics,
      max_worker_rss_bytes: peak_rss.max,
      pins_sha256: Digest::SHA256.file(File.join(__dir__, 'pins.json')).hexdigest,
      journal_sha256: Digest::SHA256.file(File.join(__dir__, 'run-all.json')).hexdigest,
      verifier_sha256: Digest::SHA256.file(__FILE__).hexdigest,
      notes: [
        'Adaptive follow-up after the primary 33 commands; does not replace any primary sample or conclusion.',
        'Only three_table_join warmup changes from 100 to 1000. Both conditions retain all nine prepared jobs, five samples and 20 target iterations.',
        'Paired results average two process medians per source, with no sample or process omitted.',
        'The four statistics-only replays use extra observation code and --zjit-stats-quiet; their counters do not establish the cause of uninstrumented timing changes.'
      ]
    }
  end
end

if $PROGRAM_NAME == __FILE__
  result = QueryWalkWarmupEvidence.verify!(QueryWalkWarmupRuns::FINAL_HASH)
  File.write(File.join(__dir__, 'summary.json'), JSON.generate(result) + "\n")
  puts "Verified #{result.fetch(:command_count)} adaptive processes"
end
