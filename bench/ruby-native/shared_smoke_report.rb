# frozen_string_literal: true

require 'digest'
require 'json'

# The ordered input list is part of the evidence. A successful retry adds
# coverage without erasing an earlier failure or claiming one uniform run.
module NativeRubySharedSmokeReport
  MODE = 'single_execution_smoke_no_timings'
  module_function

  def read_json(path)
    bytes = File.binread(path)
    [JSON.parse(bytes), Digest::SHA256.hexdigest(bytes)]
  end

  def display_path(path, root)
    absolute = File.expand_path(path)
    prefix = File.expand_path(root) + File::SEPARATOR
    absolute.start_with?(prefix) ? absolute.delete_prefix(prefix) : absolute
  end

  def compare(result, expected, metadata, workload_fingerprint)
    return 'not_executed' unless result['status'] == 'smoke_pass'
    return 'missing_go' unless expected
    return 'incompatible_input' unless result['iterations'] == 1 && metadata['mode'] == MODE
    fingerprint = metadata['dataset_fingerprint']
    return 'incompatible_input' if fingerprint && fingerprint != expected['fingerprint']
    return 'incompatible_input' if workload_fingerprint && metadata['workload_fingerprint'] && workload_fingerprint != metadata['workload_fingerprint']

    result.values_at('checksum', 'rows') == expected.values_at('checksum', 'rows_total') ? 'matches_go' : 'mismatch_go'
  end

  def build(reference:, attempts:, root:)
    raise ArgumentError, 'supply at least one Ruby status file' if attempts.empty?

    go, reference_hash = read_json(reference)
    raise ArgumentError, 'canonical Go smoke must be complete' unless go['completed'] && go['mode'] == MODE
    expected = {}
    go.fetch('outcomes').each do |entry|
      name = entry.fetch('bench')
      raise ArgumentError, "duplicate canonical Go workload: #{name}" if expected.key?(name)
      raise ArgumentError, "Go workload is not a single execution: #{name}" unless entry['iterations'] == 1 && entry['warmup'] == 0
      expected[name] = entry
    end

    histories = Hash.new { |hash, key| hash[key] = [] }
    inputs = attempts.each_with_index.map do |path, input_index|
      raise ArgumentError, "expected a .status.json file: #{path}" unless path.end_with?('.status.json')
      outcomes, status_hash = read_json(path)
      raise ArgumentError, "expected an outcome array: #{path}" unless outcomes.is_a?(Array)
      metadata_path = path.sub(/\.status\.json\z/, '.metadata.json')
      metadata, metadata_hash = File.file?(metadata_path) ? read_json(metadata_path) : [{}, nil]
      raise ArgumentError, "expected a metadata object: #{metadata_path}" unless metadata.is_a?(Hash)
      warnings = []
      warnings << 'metadata_file_missing' unless metadata_hash
      %w[source_sha256 source_sha256_after dataset_fingerprint workload_fingerprint work_mem_bytes].each do |key|
        warnings << "#{key}_not_recorded" unless metadata.key?(key)
      end
      if metadata['source_sha256'] && metadata['source_sha256_after'] && metadata['source_sha256'] != metadata['source_sha256_after']
        warnings << 'source_changed_during_run'
      end
      outcomes.each_with_index do |result, entry_index|
        name = result.fetch('bench')
        result.fetch('status')
        histories[name] << { 'input' => input_index, 'entry' => entry_index, 'result' => result,
                             'comparison' => compare(result, expected[name], metadata, go['workload_fingerprint']) }
      end
      { 'id' => input_index, 'status_path' => display_path(path, root), 'status_sha256' => status_hash,
        'metadata_path' => display_path(metadata_path, root), 'metadata_sha256' => metadata_hash,
        'metadata' => metadata, 'warnings' => warnings, 'attempt_count' => outcomes.length,
        'status_counts' => outcomes.group_by { |entry| entry.fetch('status') }.transform_values(&:length) }
    end

    workloads = (expected.keys + (histories.keys - expected.keys).sort).map do |name|
      history = histories[name]
      selected = history.reverse.find { |attempt| attempt.fetch('result')['status'] == 'smoke_pass' }
      status = if selected then selected.fetch('comparison')
               elsif history.empty? then 'missing_attempt'
               else 'no_success'
               end
      selected_index = selected && history.index(selected)
      later_failures = selected && history.drop(selected_index + 1).any? { |attempt| attempt.fetch('result')['status'] != 'smoke_pass' }
      { 'bench' => name, 'status' => status, 'go' => expected[name],
        'selected_attempt' => selected&.slice('input', 'entry'),
        'latest_attempt' => history.last&.slice('input', 'entry'),
        'failure_after_selected_success' => !!later_failures, 'attempts' => history }
    end
    selected_inputs = workloads.filter_map { |workload| workload['selected_attempt']&.fetch('input') }
    selected_metadata = selected_inputs.uniq.map { |id| inputs.fetch(id).fetch('metadata') }
    all_attempts = workloads.flat_map { |workload| workload.fetch('attempts') }
    workload_counts = workloads.group_by { |workload| workload.fetch('status') }.transform_values(&:length)
    {
      'schema_version' => 1, 'mode' => MODE,
      'coverage_scope' => 'Union of explicit attempts; not a single source snapshot, runtime, or memory configuration.',
      'selection_policy' => 'Last smoke_pass in input order, then entry order; checksum agreement does not influence selection.',
      'input_order' => 'oldest_to_newest_as_supplied',
      'reference' => { 'path' => display_path(reference, root), 'sha256' => reference_hash,
                       'metadata' => go.reject { |key, _value| key == 'outcomes' },
                       'warnings' => go.key?('workload_fingerprint') ? [] : ['workload_fingerprint_not_recorded'] },
      'inputs' => inputs,
      'summary' => {
        'canonical_workloads' => expected.length, 'attempts' => all_attempts.length,
        'workload_status_counts' => workload_counts,
        'attempt_status_counts' => all_attempts.group_by { |attempt| attempt.fetch('result').fetch('status') }.transform_values(&:length),
        'attempt_comparison_counts' => all_attempts.group_by { |attempt| attempt.fetch('comparison') }.transform_values(&:length),
        'all_canonical_workloads_match' => workloads.length == expected.length && workload_counts.fetch('matches_go', 0) == expected.length,
        'selected_input_counts' => selected_inputs.tally,
        'selected_source_sha256' => selected_metadata.map { |metadata| metadata['source_sha256'] }.uniq,
        'selected_source_sha256_after' => selected_metadata.map { |metadata| metadata['source_sha256_after'] }.uniq,
        'selected_work_mem_bytes' => selected_metadata.map { |metadata| metadata['work_mem_bytes'] }.uniq,
        'selected_jits' => selected_metadata.map { |metadata| metadata['jit'] }.uniq,
        'inputs_with_provenance_warnings' => inputs.select { |input| !input.fetch('warnings').empty? }.map { |input| input.fetch('id') },
        'successes_followed_by_failure' => workloads.select { |workload| workload.fetch('failure_after_selected_success') }.map { |workload| workload.fetch('bench') }
      },
      'workloads' => workloads
    }
  end
end
