# frozen_string_literal: true

require 'json'
require 'digest'
require_relative '../../nway_frontier_diagnostics'

module NwayFrontierDirectEvidence
  module_function

  def assert(value, message)
    raise message unless value
  end

  def digest(value) = Digest::SHA256.hexdigest(JSON.generate(value))

  def trace!(trace, available, totals, allocation: false)
    assert(trace.keys.sort == %w[disabled enabled], 'direct control sides differ')
    enabled, disabled = trace.values_at('enabled', 'disabled')
    trace.each do |side, value|
      calls = value.fetch('calls')
      assert(calls.values.all? { |count| count.is_a?(Integer) && count >= 0 }, 'invalid call counts')
      calls.each { |name, count| totals.fetch(side)[name] += count }
      decisions, frontiers = value.values_at('decisions', 'dominance_frontiers')
      assert(frontiers.keys.sort == decisions.map { |entry| entry['prior_dimensions_sha256'] }.uniq.sort,
        'dominance witness pool coverage differs')
      frontiers.each do |hash, states|
        assert(hash == digest(states) && states.is_a?(Array) && states.all? do |state|
          state.length == 3 && state.all? { |number| number.is_a?(Integer) && number >= 0 }
        end, 'dominance witness hash or dimensions differ')
      end
      decisions.each do |decision|
        values = decision.fetch('dimensions')
        prior = frontiers.fetch(decision.fetch('prior_dimensions_sha256'))
        assert(values.length == 3 && values.all? { |value| value.is_a?(Integer) && value >= 0 },
          'candidate dimensions differ')
        strict = prior.any? { |state| NwayFrontierDiagnostics.strict?(state, values) }
        assert(decision['skipped'] == strict && decision['exact_dimension_tie'] == prior.include?(values),
          'actual helper did not preserve strict dominance and exact ties')
      end
      assert(calls.fetch('strict_construction_skips', 0) == decisions.count { |decision| decision['skipped'] } &&
        calls.fetch('JedNative::NWayJoins#nway_strictly_dominated?', 0) == decisions.length,
        'dominance call/return counts differ')
      assert(value['constructed_candidates'].is_a?(Integer) && value['constructed_candidates'] >= 0,
        'invalid constructed-candidate count')
      value.fetch('frontiers').each do |frontier|
        assert(frontier['buckets'].is_a?(Integer) && frontier['buckets'] >= 0 &&
          frontier['states'].is_a?(Integer) && frontier['states'] >= frontier['buckets'] &&
          frontier['full_image_sha256'].match?(/\A[0-9a-f]{64}\z/), 'invalid complete-frontier image')
      end
      (value.fetch('physical_plans') + value.fetch('frontiers').map { |frontier| frontier['winner'] }).each do |plan|
        assert(plan.keys.sort == %w[normalized_sha256 summary] &&
          plan['normalized_sha256'].match?(/\A[0-9a-f]{64}\z/) &&
          (plan['summary'].nil? || plan['summary'].is_a?(Hash)),
          'invalid normalized selected-plan signature')
      end
      if side == 'disabled' || !available
        assert(decisions.empty? && calls.fetch('strict_construction_skips', 0).zero?,
          'baseline/disabled path performed early filtering')
      end
      if allocation
        counts = value.fetch('candidate_site_allocations')
        assert(counts.fetch('Array', 0) == value['constructed_candidates'] * 6 &&
          counts.fetch('Hash', 0) == value['constructed_candidates'] &&
          (counts.keys - %w[Array Hash]).empty?, 'actual candidate-site allocations differ')
      else
        assert(value['candidate_site_allocations'].nil?, 'ordinary trace unexpectedly disables GC for allocations')
      end
    end
    assert(enabled.values_at('physical_plans', 'frontiers') == disabled.values_at('physical_plans', 'frontiers'),
      'retained frontier or physical winner differs')
    skipped = enabled['calls'].fetch('strict_construction_skips', 0)
    assert(disabled['constructed_candidates'] - enabled['constructed_candidates'] == skipped,
      'actual candidate construction savings do not equal skips')
    %w[JedNative::NWayJoins#nway_step_cost JedNative::JoinPlanner#estimate_join_rows
       JedNative::QueryPlans#expression_work JedNative::Engine#compile].each do |name|
      assert(enabled['calls'].fetch(name, 0) == disabled['calls'].fetch(name, 0),
        "required estimator/work calls changed: #{name}")
    end
    enabled.slice('physical_plans', 'frontiers')
  end

  def verify(raw, available:)
    selected = JSON.parse(JSON.generate(NwayFrontierDiagnostics.jobs))
    assert(raw['frontier_filter_available'] == available && raw['fixture_rows'] == 32 && raw['jobs'] == selected,
      'direct feature/fixture/job coverage differs')
    semantic, observations = raw.values_at('semantic', 'observations')
    assert(semantic.keys.sort == %w[disk memory] && observations.keys.sort == %w[disk memory] &&
      raw['semantic_sha256'] == digest(semantic), 'direct semantic hash/storage differs')
    root = NwayFrontierDiagnostics::ROOT
    source_file = File.join(raw.fetch('source'), 'lib/jed_native/nway_joins.rb')
    site = File.readlines(source_file).find_index { |line| line.include?('{ order: state[:order] + [ordinal]') }&.+(1)
    assert(raw['candidate_site'] == { 'path' => 'lib/jed_native/nway_joins.rb', 'line' => site },
      'measured candidate construction source site differs')
    expected = selected.map { |job| job['name'] }.product(NwayFrontierDiagnostics::APIS.map(&:to_s),
      NwayFrontierDiagnostics::POLICIES)
    totals = { 'enabled' => Hash.new(0), 'disabled' => Hash.new(0) }
    signatures, positive, allocation_savings = {}, [], []
    %w[memory disk].each do |mode|
      outcomes, measured = semantic.fetch(mode), observations.fetch(mode)
      assert(outcomes.keys.sort == %w[allocations budgets mutations workloads] &&
        measured.keys.sort == %w[allocations mutations workloads], 'direct section coverage differs')
      assert(outcomes['workloads'].map { |group| group.values_at('workload', 'api', 'policy') } == expected &&
        measured['workloads'].map { |group| group.values_at('workload', 'api', 'policy') } == expected,
        'workload/API/reuse-policy coverage differs')
      outcomes['workloads'].zip(measured['workloads']).each do |group, observed|
        job = selected.find { |value| value['name'] == group['workload'] }
        values = NwayFrontierDiagnostics.bindings(name: job['name'])
        states = NwayFrontierDiagnostics::STATES.zip(values.map { |value| [value] })
        assert(group['values'].map { |value| value.values_at('state', 'params') } == states &&
          observed['values'].map { |value| value.values_at('state', 'params') } == states,
          'binding/state coverage differs')
        skips = 0
        group['values'].zip(observed['values']).each_with_index do |(value, observation), index|
          assert(value.fetch('outcome')['status'] == 'rows', 'ordinary diagnostic failed')
          trace = observation.fetch('trace')
          key = [mode, group['workload'], group['api'], group['policy'], index].join('/')
          signatures[key] = trace!(trace, available, totals)
          skips += trace['enabled']['calls'].fetch('strict_construction_skips', 0)
        end
        if available && group['policy'] == 'physical_disabled' &&
            NwayFrontierDiagnostics::TARGETS.include?(group['workload'])
          assert(skips.positive?, 'fresh-planner target has no actual early candidate skips')
          positive << [mode, group['workload'], group['api']]
        end
      end

      allocations = measured.fetch('allocations')
      assert(allocations.map { |item| item['workload'] } == NwayFrontierDiagnostics::ALLOCATION_JOBS &&
        outcomes['allocations'] == allocations.map { |item| item.reject { |key, _| key == 'trace' } },
        'separate allocation coverage/outcome differs')
      allocations.each do |item|
        assert(item.values_at('api', 'policy', 'params') == ['execute_prepared', 'physical_disabled', [1]] &&
          item['outcome']['status'] == 'rows', 'allocation fixture failed/differed')
        signatures["#{mode}/allocations/#{item['workload']}"] = trace!(item['trace'], available, totals, allocation: true)
        enabled, disabled = item['trace'].values_at('enabled', 'disabled')
        skips = enabled['calls'].fetch('strict_construction_skips', 0)
        assert(skips.positive?, 'allocation trace lacks actual early skips') if available
        allocation_savings << { storage: mode, workload: item['workload'], skips: skips,
          before: disabled['candidate_site_allocations'], after: enabled['candidate_site_allocations'] }
      end

      assert(outcomes['budgets'].map { |group| group.values_at('workload', 'api') } ==
        NwayFrontierDiagnostics::ALLOCATION_JOBS.product(NwayFrontierDiagnostics::APIS.map(&:to_s)),
        'cost-ceiling coverage differs')
      outcomes['budgets'].each do |group|
        ordinary, limits = group.values_at('ordinary', 'limits')
        assert(group['params'] == [3] && ordinary['status'] == 'rows' && ordinary['cost'].positive?,
          'ordinary budget result differs')
        cost = ordinary.fetch('cost')
        expected_limits = [1, cost / 2, cost - 1, cost, cost + 1].select(&:positive?).uniq
        assert(limits.map { |value| value.values_at('setting', 'limit') } ==
          %w[max_cost lifetime_max_cost].product(expected_limits), 'budget prefix missing')
        limits.each do |value|
          if value['limit'] < cost
            state = value['setting'] == 'max_cost' ? '54P01' : '54P02'
            assert(value['outcome']['status'] == 'error' && value['outcome']['sqlstate'] == state,
              'below-cost budget was not enforced')
          elsif value['limit'] > cost
            assert(value['outcome'] == ordinary, 'above-cost outcome differs')
          end
        end
      end

      actions = [nil, 'ANALYZE items', 'UPDATE items SET score=9 WHERE id=0',
        'CREATE INDEX frontier_bucket ON items(bucket)', 'DROP INDEX frontier_bucket']
      sequence = actions.product([[1], [8], [nil]])
      assert(outcomes['mutations'].map { |value| value.values_at('after', 'params') } == sequence &&
        measured['mutations'].map { |value| value.values_at('after', 'params') } == sequence,
        'mutation/EXPLAIN coverage differs')
      outcomes['mutations'].zip(measured['mutations']).each_with_index do |(value, observation), index|
        assert(value['outcome']['status'] == 'rows' && value['explain']['status'] == 'rows',
          'mutation/EXPLAIN failed')
        signatures["#{mode}/mutation/#{index}"] = trace!(observation.fetch('trace'), available, totals)
      end
    end
    disk = raw.fetch('storage').fetch('disk')
    assert(raw['storage']['memory'] == { 'kind' => 'memory' } && disk['kind'] == 'file' &&
      disk['locking'] == 'exclusive' && disk['removed_after_run'] && !File.exist?(disk['path']) &&
      disk['path'].start_with?(File.join(root, '.dev/ruby-native-nway-frontier-diagnostics/')) &&
      disk['bytes'].positive? && disk['sha256'].match?(/\A[0-9a-f]{64}\z/), 'private file fixture/cleanup differs')
    { semantic_sha256: raw['semantic_sha256'], physical_plans_sha256: digest(signatures),
      file_signature: disk.values_at('bytes', 'sha256', 'filesystem_device'), storage: raw['storage'],
      workload_api_policy_cases: expected.length * 2,
      workload_traced_invocations: expected.length * NwayFrontierDiagnostics::STATES.length * 2,
      mutation_cases: 30, allocation_cases: 6, positive_skip_cases: positive,
      call_totals: totals, candidate_site_allocations: allocation_savings }
  end
end
