#!/usr/bin/env ruby
# frozen_string_literal: true

require 'json'
require 'digest'
require 'fileutils'

# This only combines explicitly supplied artifacts; it never executes workloads.
module NativeRubyFileCompareMerge
  module_function

  def build(paths)
    raise ArgumentError, 'supply original and retry reports in oldest-to-newest order' if paths.empty?
    inputs = paths.map do |path|
      bytes = File.binread(path)
      { 'path' => File.expand_path(path), 'sha256' => Digest::SHA256.hexdigest(bytes), 'report' => JSON.parse(bytes) }
    end
    first = inputs.first.fetch('report')
    settings = %w[corpus_sha256 dataset_fingerprint locking memory_bytes]
    host_keys = %w[platform cpu processors ruby go rust node]
    filesystem_keys = %w[source type mount_point device]
    workload_keys = %w[dataset iterations warmup sql source_sha256]
    workloads, datasets, histories, statuses = {}, {}, {}, {}
    work_mem = inputs.map { |input| input.fetch('report')['work_mem_bytes'] }.compact.uniq
    raise ArgumentError, 'work_mem settings differ' if work_mem.length > 1

    inputs.each_with_index do |input, input_index|
      report = input.fetch('report')
      raise ArgumentError, 'expected measured canonical file workloads' unless report['mode'] == 'canonical_file_workloads'
      settings.each do |key|
        raise ArgumentError, "incompatible #{key}: #{input.fetch('path')}" unless report.fetch(key) == first.fetch(key)
      end
      raise ArgumentError, 'host or runtime settings differ' unless report.fetch('host').slice(*host_keys) == first.fetch('host').slice(*host_keys)
      raise ArgumentError, 'workspace filesystems differ' unless report.fetch('workspace_filesystem').slice(*filesystem_keys) == first.fetch('workspace_filesystem').slice(*filesystem_keys)
      report.fetch('workloads').each do |name, definition|
        if workloads[name] && workloads[name].slice(*workload_keys) != definition.slice(*workload_keys)
          raise ArgumentError, "workload definition differs: #{name}"
        end
        workloads[name] ||= definition
      end
      report.fetch('datasets').each do |name, dataset|
        raise ArgumentError, "dataset bytes differ: #{name}" if datasets[name] && datasets[name].values_at('sha256', 'bytes') != dataset.values_at('sha256', 'bytes')
        datasets[name] ||= dataset
      end
      statuses.merge!(report.fetch('statuses'))
      report.fetch('results').each_with_index do |result, entry_index|
        key = result.values_at('lane', 'workload')
        (histories[key] ||= []) << { 'input' => input_index, 'entry' => entry_index, 'result' => result }
      end
    end

    selected = statuses.keys.product(workloads.keys).map do |lane, name|
      history = histories.fetch([lane, name], [])
      # An executed mismatching answer is still the latest answer: never hide
      # it by falling back to an earlier result whose checksum matched.
      attempt = history.reverse.find { |item| %w[completed checksum_mismatch].include?(item.fetch('result')['status']) }
      unless attempt
        next { 'lane' => lane, 'workload' => name, 'status' => 'no_success',
               'latest_attempt' => history.last&.slice('input', 'entry') }
      end
      row = attempt.fetch('result').dup
      result = row.fetch('result')
      definition = workloads.fetch(name)
      report = inputs.fetch(attempt.fetch('input')).fetch('report')
      lane_status = report.fetch('statuses').fetch(lane)
      valid = result['bench'] == name && result['dataset'] == definition['dataset'] &&
        result['iterations'] == definition['iterations'] && result['warmup'] == definition['warmup'] &&
        result['fingerprint'] == report['dataset_fingerprint'] && lane_status['status'] == 'ready' &&
        (!lane_status['final_source_sha256'] || lane_status['source_sha256'] == lane_status['final_source_sha256'])
      row.merge('status' => valid ? 'completed' : 'incompatible_result',
                'selected_attempt' => attempt.slice('input', 'entry'),
                'latest_attempt' => history.last.slice('input', 'entry'),
                'failure_after_selected_success' => history.last != attempt)
    end
    expected = {}
    selected.sort_by { |row| %w[go rust ts].index(row['lane']) || 3 }.each do |row|
      next unless row['status'] == 'completed'
      expected[row['workload']] ||= { 'lane' => row['lane'], 'answer' => row.fetch('result').values_at('checksum', 'rows_total') }
    end
    selected.each do |row|
      next unless row['status'] == 'completed'
      reference = expected.fetch(row['workload'])
      row['comparison_lane'] = reference.fetch('lane')
      row['status'] = 'checksum_mismatch' unless row.fetch('result').values_at('checksum', 'rows_total') == reference.fetch('answer')
    end
    warnings = []
    warnings << 'work_mem_not_recorded_in_every_input; numeric compatibility cannot be verified for those inputs' if inputs.any? { |input| !input.fetch('report').key?('work_mem_bytes') }
    {
      'schema' => 1, 'mode' => 'merged_canonical_file_workloads',
      'selection_policy' => 'Latest executed answer per lane/workload in explicit input order; original reports and failures remain intact.',
      'compatibility' => first.slice(*settings).merge('host' => first.fetch('host').slice(*host_keys),
        'workspace_filesystem' => first.fetch('workspace_filesystem'), 'recorded_work_mem_bytes' => work_mem, 'warnings' => warnings),
      'workloads' => workloads, 'datasets' => datasets, 'inputs' => inputs, 'results' => selected,
      'summary' => { 'lanes' => statuses.keys, 'workloads' => workloads.keys, 'cells' => selected.length,
        'status_counts' => selected.group_by { |row| row.fetch('status') }.transform_values(&:length),
        'historical_status_counts' => histories.values.flatten.group_by { |attempt| attempt.fetch('result').fetch('status') }.transform_values(&:length),
        'all_cells_match' => !selected.empty? && selected.all? { |row| row['status'] == 'completed' } }
    }
  end
end

if $PROGRAM_NAME == __FILE__
  abort 'Usage: file_compare_merge.rb OUTPUT.json ORIGINAL.json [RETRY.json ...]' if ARGV.length < 2
  output, *inputs = ARGV
  abort 'output must differ from every input artifact' if inputs.any? { |path| File.expand_path(path) == File.expand_path(output) }
  report = NativeRubyFileCompareMerge.build(inputs)
  FileUtils.mkdir_p(File.dirname(output))
  File.write("#{output}.tmp", JSON.pretty_generate(report) + "\n")
  File.rename("#{output}.tmp", output)
  puts JSON.pretty_generate(report.fetch('summary'))
  abort 'merged file comparison is incomplete or inconsistent' unless report.fetch('summary').fetch('all_cells_match')
end
