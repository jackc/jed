# frozen_string_literal: true
require 'json'
require 'digest'
require_relative '../../source'

read = ->(name) { JSON.parse(File.read(File.join(__dir__, name))) }
summary = { statistic: 'Mean of two per-process medians per ABBA state; microseconds per complete query.',
            controls: {}, before: {}, binding: {}, cross_core: {}, validation: {} }
answers = {}
source = nil
%w[4.0-none 4.0-yjit 4.1-zjit].each do |runtime|
  runs = %w[1-disabled 2-enabled 3-enabled 4-disabled].map { |state| read.call("control-#{runtime}-#{state}.json") }
  summary[:controls][runtime] = runs.first.fetch('lanes').to_h do |lane, entry|
    [lane, entry.fetch('result').fetch('workloads').to_h do |job|
      name = job.fetch('name')
      medians = runs.map { |run| run.fetch('lanes').fetch(lane).fetch('result').fetch('workloads').find { |other| other['name'] == name }.fetch('median_ns') }
      disabled, enabled = medians.values_at(0, 3), medians.values_at(1, 2)
      [name, { disabled_us: disabled.sum / 2000.0, enabled_us: enabled.sum / 2000.0,
               speedup: disabled.sum.fdiv(enabled.sum), disabled_medians_ns: disabled, enabled_medians_ns: enabled,
               enabled_medians_below_both_disabled: enabled.max < disabled.min }]
    end]
  end
end
Dir[File.join(__dir__, '{before,control,binding}-{4.0,4.1}-*.json')].sort.each do |path|
  report = JSON.parse(File.read(path))
  raise "measurement failed: #{path}" unless report.fetch('errors').empty?
  raise 'source/harness changed' unless report.fetch('source_sha256') == report.fetch('source_sha256_after')
  filename = File.basename(path)
  report.fetch('lanes').each do |lane, entry|
    raise "lane failed: #{path} #{lane}" unless entry.fetch('success')
    result = entry.fetch('result')
    raise 'worker source changed' unless result['source_sha256'] == result['final_source_sha256']
    if filename.start_with?('control-')
      source ||= result.fetch('source_sha256')
      raise 'control source differs' unless source == result['source_sha256']
      raise 'control switch differs' unless result['aggregate_disabled'] == filename.include?('disabled')
    end
    result.fetch('workloads').each do |job|
      name, digest = job.fetch('name'), job.fetch('validation_sha256')
      raise "values/metadata/cost mismatch: #{name}" if answers[name] && answers[name] != digest
      answers[name] = digest
    end
    values = result.fetch('workloads').to_h { |job| [job.fetch('name'), job.fetch('median_ns') / 1000.0] }
    if filename.start_with?('before-')
      runtime = filename.delete_prefix('before-').delete_suffix('.json')
      (summary[:before][runtime] ||= {})[lane] = values
    elsif filename.start_with?('binding-')
      summary[:binding][lane] = values
    end
  end
end
summary[:source_sha256] = source
root = File.expand_path('../../../..', __dir__)
raise 'current source differs from measured source' unless NativeRubySource.digest(File.join(root, 'impl/ruby-native')) == source
summary[:validation][:matching_values_metadata_costs] = answers
profiles = %w[before final].to_h do |state|
  profile = read.call("#{state}-profile.json")
  raise 'profile source changed' unless profile['source_sha256'] == profile['final_source_sha256']
  raise 'final profile source differs' if state == 'final' && profile['source_sha256'] != source
  [state, profile.fetch('results').to_h do |job|
    allocations = job.fetch('samples').map { |sample| sample.fetch('allocations_per_query') }.sort
    [job.fetch('workload'), { allocations_per_query: allocations[allocations.length / 2],
                              answer_sha256: job.fetch('first_answer_sha256') }]
  end]
end
summary[:allocations] = profiles.fetch('before').to_h do |name, before|
  after = profiles.fetch('final').fetch(name)
  raise 'profile answers differ' unless before[:answer_sha256] == after[:answer_sha256]
  [name, { before: before[:allocations_per_query], final: after[:allocations_per_query] }]
end
cross = read.call('cross-core.json')
raise 'cross-core source changed' unless cross['ruby_source_sha256'] == source && cross['ruby_source_sha256_after'] == source && cross['source_sha256'] == cross['source_sha256_after']
raise 'cross-core failure' unless cross.fetch('statuses').values.all? { |entry| entry.fetch('status') == 'ready' }
checksums = {}
summary[:cross_core] = cross.fetch('results').to_h do |lane, jobs|
  [lane, jobs.to_h do |job|
    name = job.fetch('workload')
    signature = job.values_at('checksum', 'validation_rows', 'rows_total', 'iterations', 'warmup')
    raise 'cross-core answer/schedule differs' if checksums[name] && checksums[name] != signature
    checksums[name] = signature
    samples = job.fetch('samples_ns').sort
    [name, samples[samples.length / 2].fdiv(job.fetch('iterations')) / 1000.0]
  end]
end
%w[memory-zjit disk-yjit mixed-query-yjit pure-query-yjit].each do |name|
  report = read.call("#{name}.json")
  raise 'corpus source differs' unless report['source_sha256'] == source && report['final_source_sha256'] == source
  raise 'incomplete corpus' unless report['finished_at'] && report['files'] == 679
  %w[timed_out_files memory_limited_files unsupported_files].each { |key| raise "corpus #{key}" unless report[key].zero? }
  summary[:validation][name] = report.slice('passed', 'failed', 'records', 'compiled_aggregates', 'peak_worker_rss_bytes')
end
previous = JSON.parse(File.read(File.join(__dir__, '../ordering-followup/pure-query-yjit.json')))
raise 'pure-query failure set changed' unless read.call('pure-query-yjit.json').fetch('failures') == previous.fetch('failures')
diagnostics = read.call('diagnostic-summary.json')
raise 'diagnostic source differs' unless diagnostics.fetch('source_sha256') == source
%w[4.0-none 4.0-yjit 4.1-zjit].each do |runtime|
  %w[diagnostic retention].each do |kind|
    report = read.call("#{kind}-#{runtime}.json")
    raise 'diagnostic/probe source differs' unless report['source_sha256'] == source && report['final_source_sha256'] == source
  end
  probe = read.call("retention-#{runtime}.json")
  raise 'kernels retained after helper return' unless probe.fetch('after_helper_return_and_gc').all? { |snapshot| snapshot.fetch('live_kernels').zero? }
end
log_hash = Digest::SHA256.file(File.join(__dir__, 'diagnostic-4.1-zjit.log')).hexdigest
raise 'final ZJIT log differs' unless diagnostics.fetch('zjit_log_audit').fetch('final_sha256') == log_hash
summary[:diagnostics] = { summary: 'diagnostic-summary.json',
                          zero_live_kernels_after_helper_return: true,
                          vm_class_retention: 'ZJIT retains all 256 generated classes in the separate probe; cache bounds do not bound JIT memory.' }
File.write(File.join(__dir__, 'summary.json'), JSON.pretty_generate(summary) + "\n")
manifest = Dir[File.join(__dir__, '*')].sort.filter_map do |path|
  next unless File.file?(path) && File.basename(path) != 'manifest.json'
  [File.basename(path), { bytes: File.size(path), sha256: Digest::SHA256.file(path).hexdigest }]
end.to_h
File.write(File.join(__dir__, 'manifest.json'), JSON.pretty_generate(manifest) + "\n")
puts 'Aggregate source, complete corpus audits, unchanged cursor diagnostics, and all measured values/metadata/exact costs verified.'
