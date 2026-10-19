# frozen_string_literal: true
require 'json'
require 'digest'

directory = __dir__
read = ->(name) { JSON.parse(File.read(File.join(directory, name))) }
timing = read.call('timing-summary.json')
summary = {
  source_sha256: timing.fetch('inputs').fetch('source'),
  statistic: 'Mean of the two process medians in each ABBA state; microseconds per complete query.',
  controls: {}, before: {}, binding: {}, cross_core: {}, validation: {}
}
timing.fetch('controls').each do |runtime, lanes|
  summary[:controls][runtime] = lanes.to_h do |lane, jobs|
    [lane, jobs.to_h do |name, job|
      disabled = job.fetch('disabled').fetch('process_medians_ns')
      enabled = job.fetch('enabled').fetch('process_medians_ns')
      [name, { disabled_us: disabled.sum / 2000.0, enabled_us: enabled.sum / 2000.0,
        speedup: disabled.sum / enabled.sum,
        enabled_medians_below_both_disabled: enabled.max < disabled.min }]
    end]
  end
end

# The validation digest includes metadata, values, and exact costs. Verify it
# across source versions, runtime modes, native APIs, and the actual binding.
answers = {}
files = Dir[File.join(directory, '{before,control,binding}-*.json')].sort
files.each do |path|
  report = JSON.parse(File.read(path))
  raise "measurement failed: #{path}" unless report.fetch('errors').empty?
  report.fetch('lanes').each do |lane, entry|
    raise "lane failed: #{path} #{lane}" unless entry.fetch('success')
    result = entry.fetch('result')
    raise 'measurement source changed' unless result['source_sha256'] == result['final_source_sha256']
    jobs = result.fetch('workloads')
    jobs.each do |job|
      name, digest = job.fetch('name'), job.fetch('validation_sha256')
      raise "answer/cost mismatch: #{name}" if answers[name] && answers[name] != digest
      answers[name] = digest
    end
    values = jobs.to_h { |job| [job.fetch('name'), job.fetch('median_ns') / 1000.0] }
    filename = File.basename(path)
    if filename.start_with?('before-')
      runtime = filename.delete_prefix('before-').delete_suffix('.json')
      (summary[:before][runtime] ||= {})[lane] = values
    elsif filename.start_with?('binding-')
      summary[:binding][lane] = values
    end
  end
end
cross = read.call('cross-core.json')
raise 'cross-core failure' unless cross.fetch('statuses').values.all? { |entry| entry.fetch('status') == 'ready' }
summary[:cross_core] = cross.fetch('results').to_h do |lane, jobs|
  [lane, jobs.to_h do |job|
    samples = job.fetch('samples_ns').sort
    [job.fetch('workload'), samples[samples.length / 2].fdiv(job.fetch('iterations')) / 1000.0]
  end]
end
summary[:validation][:matching_answer_cost_metadata_digests] = answers
%w[memory-zjit disk-yjit mixed-query-yjit pure-query-yjit].each do |name|
  report = read.call("#{name}.json")
  raise 'corpus source mismatch' unless report['source_sha256'] == summary[:source_sha256] && report['final_source_sha256'] == summary[:source_sha256]
  raise 'incomplete corpus audit' unless report['finished_at'] && report['files'] == 679
  summary[:validation][name] = report.slice('passed', 'failed', 'records', 'compiled_orderings',
    'peak_worker_rss_bytes', 'timed_out_files', 'memory_limited_files', 'unsupported_files')
end
previous = JSON.parse(File.read(File.join(directory, '../correctness-followup/pure-query-yjit.json')))
raise 'new pure-query failures' unless read.call('pure-query-yjit.json').fetch('failures') == previous.fetch('failures')
File.write(File.join(directory, 'summary.json'), JSON.pretty_generate(summary) + "\n")
puts 'Ordering summaries: source, complete audits, unchanged collector diagnostics, and cross-run/API/binding values, metadata and costs verified.'
