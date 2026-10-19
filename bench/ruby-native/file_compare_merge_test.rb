# frozen_string_literal: true

require 'tmpdir'
require_relative 'file_compare_merge'

module NativeRubyFileCompareMergeTest
  extend self
  def assert(value, message)
    raise message unless value
  end
  def write(path, value) = File.write(path, JSON.generate(value))

  Dir.mktmpdir('jed-file-merge-test-') do |directory|
    original = File.join(directory, 'original.json')
    retry_path = File.join(directory, 'retry.json')
    result = { 'bench' => 'lookup', 'dataset' => 'small', 'iterations' => 3, 'warmup' => 1,
               'fingerprint' => 'dataset-spec', 'checksum' => 'answer', 'rows_total' => 3, 'ns_per_op' => 100 }
    report = { 'mode' => 'canonical_file_workloads', 'corpus_sha256' => 'corpus',
      'dataset_fingerprint' => 'dataset-spec', 'locking' => 'exclusive', 'memory_bytes' => 512,
      'host' => { 'cpu' => 'cpu', 'platform' => 'linux', 'node' => 'node' },
      'workspace_filesystem' => { 'type' => 'ext4', 'device' => 1 },
      'workloads' => { 'lookup' => { 'dataset' => 'small', 'iterations' => 3, 'warmup' => 1, 'sql' => 'SELECT 1', 'source_sha256' => 'workload' } },
      'datasets' => { 'small' => { 'sha256' => 'data', 'bytes' => 4096 } },
      'statuses' => { 'go' => { 'status' => 'ready' }, 'ts' => { 'status' => 'ready' } },
      'results' => [{ 'lane' => 'go', 'workload' => 'lookup', 'status' => 'completed', 'result' => result },
                    { 'lane' => 'ts', 'workload' => 'lookup', 'status' => 'failed', 'reason' => 'missing native ABI' }] }
    retry_report = report.merge('statuses' => { 'ts' => { 'status' => 'ready' } },
      'results' => [{ 'lane' => 'ts', 'workload' => 'lookup', 'status' => 'completed', 'result' => result.dup }])
    write(original, report)
    write(retry_path, retry_report)
    merged = NativeRubyFileCompareMerge.build([original, retry_path])
    assert(merged.fetch('summary')['all_cells_match'], 'matching retry did not complete comparison')
    assert(merged.fetch('summary')['historical_status_counts'] == { 'completed' => 2, 'failed' => 1 }, 'failed attempt disappeared')
    assert(merged.fetch('inputs').first.fetch('report') == report, 'original report or failure was changed')
    assert(merged.fetch('inputs').first.fetch('sha256') == Digest::SHA256.file(original).hexdigest, 'input hash was lost')
    assert(merged.fetch('results').last['selected_attempt'] == { 'input' => 1, 'entry' => 0 }, 'retry provenance was lost')
    assert(!merged.fetch('compatibility').fetch('warnings').empty?, 'unrecorded work_mem was assumed')

    retry_report['results'].first['result']['checksum'] = 'different'
    write(retry_path, retry_report)
    mismatch = NativeRubyFileCompareMerge.build([original, retry_path])
    assert(mismatch.fetch('results').last['status'] == 'checksum_mismatch', 'cross-report checksum mismatch was accepted')

    %w[corpus_sha256 dataset_fingerprint locking memory_bytes].each do |key|
      changed = retry_report.merge(key => 'different')
      write(retry_path, changed)
      begin
        NativeRubyFileCompareMerge.build([original, retry_path])
        raise "incompatible #{key} accepted"
      rescue ArgumentError
        nil
      end
    end
    [['host', 'cpu'], ['datasets', 'small'], ['workloads', 'lookup'], ['workspace_filesystem', 'device']].each do |section, key|
      changed = Marshal.load(Marshal.dump(retry_report))
      changed[section][key] = section == 'datasets' ? { 'sha256' => 'changed', 'bytes' => 4096 } : section == 'workloads' ? { 'sql' => 'changed' } : 'changed'
      write(retry_path, changed)
      begin
        NativeRubyFileCompareMerge.build([original, retry_path])
        raise "incompatible #{section} accepted"
      rescue ArgumentError
        nil
      end
    end
    write(original, report.merge('work_mem_bytes' => 256))
    write(retry_path, retry_report.merge('work_mem_bytes' => 16))
    begin
      NativeRubyFileCompareMerge.build([original, retry_path])
      raise 'incompatible work_mem accepted'
    rescue ArgumentError
      nil
    end
  end
end

puts 'file comparison merge tests passed'
