# frozen_string_literal: true

require 'tmpdir'
require_relative 'shared_smoke_report'

module NativeRubySharedSmokeReportTest
  extend self

  def assert(value, message)
    raise message unless value
  end

  def write(path, data)
    File.write(path, JSON.generate(data))
  end

  def success(name, checksum = 'answer')
    { 'bench' => name, 'status' => 'smoke_pass', 'checksum' => checksum, 'rows' => 3, 'iterations' => 1 }
  end

  def failure(name, reason)
    { 'bench' => name, 'status' => 'failed', 'reason' => reason,
      'active_sql' => { 'sql' => 'SELECT expensive_work()' }, 'peak_rss_bytes' => 123 }
  end

  Dir.mktmpdir('jed-smoke-report-test-') do |directory|
    reference = File.join(directory, 'go.json')
    names = %w[recovered later_failure regressed missing only_failed]
    go = { 'mode' => NativeRubySharedSmokeReport::MODE, 'completed' => true,
           'workload_fingerprint' => 'workloads',
           'outcomes' => names.map do |name|
             { 'bench' => name, 'checksum' => 'answer', 'rows_total' => 3,
               'iterations' => 1, 'warmup' => 0, 'fingerprint' => 'dataset' }
           end }
    write(reference, go)
    first = File.join(directory, 'first.status.json')
    second = File.join(directory, 'second.status.json')
    old_failure = failure('recovered', 'memory exceeded')
    first_results = [old_failure, success('later_failure'), success('regressed'), failure('only_failed', 'timeout')]
    write(first, first_results)
    write(second, [success('recovered'), failure('later_failure', 'timeout'), success('regressed', 'wrong'), failure('only_failed', 'SQL error')])
    metadata = { 'mode' => NativeRubySharedSmokeReport::MODE, 'source_sha256' => 'original',
                 'source_sha256_after' => 'original', 'dataset_fingerprint' => 'dataset',
                 'workload_fingerprint' => 'workloads', 'jit' => 'yjit' }
    write(first.sub('.status.json', '.metadata.json'), metadata)
    changed = metadata.merge('source_sha256' => 'before', 'source_sha256_after' => 'after', 'work_mem_bytes' => 16 * 1024**2)
    write(second.sub('.status.json', '.metadata.json'), changed)

    report = NativeRubySharedSmokeReport.build(reference: reference, attempts: [first, second], root: directory)
    workloads = report.fetch('workloads').to_h { |entry| [entry.fetch('bench'), entry] }
    recovered = workloads.fetch('recovered')
    assert(recovered['status'] == 'matches_go' && recovered['selected_attempt'] == { 'input' => 1, 'entry' => 0 }, 'retry was not selected in explicit input order')
    assert(recovered.fetch('attempts').first.fetch('result') == old_failure, 'original failure or SQL/memory evidence was erased')
    assert(workloads.fetch('later_failure')['selected_attempt'] == { 'input' => 0, 'entry' => 1 }, 'a later failure replaced a successful answer')
    assert(workloads.fetch('later_failure')['failure_after_selected_success'], 'later failure was hidden')
    assert(workloads.fetch('regressed')['status'] == 'mismatch_go', 'selection silently chose the older matching checksum')
    assert(workloads.fetch('missing')['status'] == 'missing_attempt', 'unattempted canonical workload disappeared')
    assert(workloads.fetch('only_failed')['status'] == 'no_success', 'failures counted as completed coverage')
    assert(report.fetch('inputs').first.fetch('warnings').include?('work_mem_bytes_not_recorded'), 'unknown historical configuration was invented')
    assert(report.fetch('inputs').last.fetch('metadata') == changed, 'source/configuration provenance changed')
    assert(report.fetch('inputs').last.fetch('warnings').include?('source_changed_during_run'), 'changing source was not flagged')
    assert(report.fetch('inputs').first.fetch('status_sha256') == Digest::SHA256.file(first).hexdigest, 'source artifact hash is wrong')
    assert(report.fetch('summary')['attempt_status_counts'] == { 'failed' => 4, 'smoke_pass' => 4 }, 'historical outcomes were flattened')
    assert(!report.fetch('summary')['all_canonical_workloads_match'], 'partial or mismatched coverage reported as complete')

    # Matching bytes from a different dataset or workload definition are not
    # comparable, even when the checksum happens to be the same.
    bad = File.join(directory, 'bad.status.json')
    write(bad, [success('recovered')])
    bad_metadata = bad.sub('.status.json', '.metadata.json')
    %w[dataset_fingerprint workload_fingerprint].each do |key|
      write(bad_metadata, metadata.merge(key => 'different'))
      incompatible = NativeRubySharedSmokeReport.build(reference: reference, attempts: [first, bad], root: directory)
      assert(incompatible.fetch('workloads').first.fetch('status') == 'incompatible_input', "#{key} mismatch was compared as valid")
    end

    # Recovery can establish all-workload answer coverage while still retaining
    # the failed attempt and an explicit warning that the sources differed.
    go['outcomes'] = go.fetch('outcomes').take(1)
    write(reference, go)
    write(first, [old_failure])
    write(second, [success('recovered')])
    complete = NativeRubySharedSmokeReport.build(reference: reference, attempts: [first, second], root: directory)
    assert(complete.fetch('summary')['all_canonical_workloads_match'], 'successful retry did not establish answer coverage')
    assert(complete.fetch('summary')['attempts'] == 2 && complete.fetch('inputs').length == 2, 'successful report lost prior evidence')
    assert(complete.fetch('summary')['selected_source_sha256'] == ['before'], 'selected source revision was lost')
    assert(complete.fetch('summary')['selected_source_sha256_after'] == ['after'], 'selected ending source revision was lost')
  end
end

puts 'shared smoke report tests passed'
