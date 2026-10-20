# frozen_string_literal: true

require 'json'
require 'digest'
require_relative '../../file_compare'

module CheckpointFileEvidence
  JOBS = %w[point_lookup_pk cold_open_pk_small insert_commit_durable].freeze
  STATES = %w[1-before 2-final 3-final 4-before].freeze
  module_function

  def check_report(name, source, runtime, cross: false, rust_retry: false)
    report = JSON.parse(File.read(File.join(@directory, "#{name}.json")))
    identity = @runtime_info.fetch(runtime)
    failed_rust_preparation = cross && report.fetch('statuses').fetch('rust').fetch('status') != 'ready'
    unless rust_retry
      bound = JSON.parse(File.read(File.join(@directory, "#{name}-bounded.json")))
      expected_success, expected_reason = failed_rust_preparation ? [false, 'worker exited 1'] : [true, nil]
      raise 'file orchestration failed/source changed' unless bound.values_at('success', 'reason') == [expected_success, expected_reason] && bound.values_at('source_sha256', 'final_source_sha256') == [source, source]
      raise 'file orchestration bounds differ' unless bound.values_at('memory_bytes', 'address_space_bytes', 'timeout_seconds') == [536870912, 16 * 1024**3, cross ? 1800.0 : 900.0]
      raise 'file orchestration runtime differs' unless bound.fetch('command').first == identity.fetch(:executable)
    end
    lane = identity.fetch(:jit) == 'none' ? 'ruby' : "ruby-#{identity.fetch(:jit)}"
    lanes = rust_retry ? ['rust'] : cross ? [lane, 'go', 'rust', 'ts'] : [lane]
    raise 'file report policy differs' unless report.values_at('mode', 'locking', 'memory_bytes', 'lane_timeout_seconds') == ['canonical_file_workloads', 'exclusive', 536870912, 600]
    raise 'file orchestration runtime version differs' unless report.fetch('host').fetch('ruby').start_with?(identity.fetch(:version))
    filesystem = report.fetch('workspace_filesystem')
    raise 'file benchmark used memory filesystem' if %w[tmpfs ramfs devtmpfs].include?(filesystem.fetch('type'))
    raise 'file benchmark workspace differs' unless filesystem.fetch('directory') == File.join(@root, '.dev/ruby-native-file-compare/workspaces')
    raise 'file corpus changed' unless report['corpus_sha256'] == Digest::SHA256.file(File.join(@root, 'bench/corpus/benchmarks.toml')).hexdigest
    raise 'file corpus fingerprint differs' unless report['dataset_fingerprint'] == Bench.corpus_fingerprint(File.join(@root, 'bench/corpus'))
    raise 'file jobs differ' unless report.fetch('workloads').keys == JOBS
    report.fetch('workloads').each do |name, work|
      spec = @canonical.fetch(name)
      raise 'canonical file SQL/schedule differs' unless work.values_at('dataset', 'iterations', 'warmup', 'sql') == spec.values_at(:dataset, :iterations, :warmup, :sql)
      raise 'selected file corpus changed' unless work['source_sha256'] == Digest::SHA256.file(File.join(work.fetch('corpus'), 'benchmarks.toml')).hexdigest
    end
    report.fetch('datasets').each do |name, dataset|
      path = File.join(@root, '.dev/ruby-native-bench-data', "#{name}.jed")
      raise 'canonical dataset changed' unless dataset.values_at('path', 'bytes', 'sha256') == [path, File.size(path), Digest::SHA256.file(path).hexdigest]
    end
    raise 'file lanes differ' unless report.fetch('statuses').keys.sort == lanes.sort
    report.fetch('statuses').each do |name, status|
      failed_preparation = failed_rust_preparation && name == 'rust'
      if failed_preparation
        raise 'unexpected Rust preparation failure' unless status['status'] == 'unavailable' &&
          status.fetch('detail').include?('worker memory limit exceeded (536870912 bytes)') && status.fetch('peak_rss_bytes') > 536870912
      else
        raise "file adapter failed #{name}" unless status['status'] == 'ready'
      end
      adapter = case name
                when 'go' then 'bench/go/cmd/bench-jed/main.go'
                when 'rust' then 'bench/rust/src/bin/bench_jed.rs'
                when 'ts' then 'bench/ts/src/bench-jed.ts'
                end
      raise 'supported-core file adapter changed' if adapter && status['adapter_sha256'] != Digest::SHA256.file(File.join(@root, adapter)).hexdigest
      if %w[rust ts].include?(name)
        library = name == 'rust' ? 'bench/rust/src/lib.rs' : 'bench/ts/src/lib.ts'
        raise 'supported-core file harness changed' unless status['library_sha256'] == Digest::SHA256.file(File.join(@root, library)).hexdigest
      end
      next unless name == lane
      raise 'file Ruby source changed' unless status.values_at('source_sha256', 'final_source_sha256') == [source, source]
      raise 'file Ruby runtime/JIT differs' unless status['command'].first == identity.fetch(:executable) && status['jit'] == (identity[:jit] == 'none' ? 'interpreter' : identity[:jit])
      raise 'file Ruby JIT command differs' unless status['command'].include?('--yjit') == (identity[:jit] == 'yjit') && status['command'].include?('--zjit') == (identity[:jit] == 'zjit')
      raise 'file Ruby adapter changed' unless status['adapter_sha256'] == Digest::SHA256.file(File.join(@root, 'bench/ruby-native/bench_jed.rb')).hexdigest
      raise 'file shared harness changed' unless status['library_sha256'] == Digest::SHA256.file(File.join(@root, 'bench/ruby/lib/bench.rb')).hexdigest
    end
    measured_lanes = failed_rust_preparation ? lanes - ['rust'] : lanes
    raise 'file result count differs' unless report.fetch('results').length == JOBS.length * measured_lanes.length
    raise 'file result tuples differ' unless report.fetch('results').map { |entry| entry.values_at('lane', 'workload') }.sort == measured_lanes.product(JOBS).sort
    report.fetch('results').each do |entry|
      name = entry.fetch('workload')
      result = entry.fetch('result')
      raise 'file worker failed' unless entry['status'] == 'completed' && entry['reason'].nil? && !entry['interrupted']
      raise 'file worker limits differ' unless entry.values_at('memory_bytes', 'address_space_bytes') == [536870912, entry['lane'] == lane ? 2 * 1024**3 : 16 * 1024**3]
      raise 'file worker exceeded bounds' unless entry.fetch('sampled_peak_worker_rss_bytes').between?(1, 536870912) && entry.fetch('timeout_seconds').between?(1, 600)
      raise 'file worker schedule differs' unless result.values_at('bench', 'iterations', 'warmup') == [name, *@canonical.fetch(name).values_at(:iterations, :warmup)]
      raise 'file worker fingerprint differs' unless result['fingerprint'] == report['dataset_fingerprint']
      if entry['lane'] == lane
        raise 'file Ruby worker JIT identity differs' unless result.values_at('lang', 'variant') == ['ruby-native', identity[:jit] == 'none' ? 'interpreter' : identity[:jit]]
      end
      raise 'file latency invalid' unless %w[total_ns ns_per_op p50_ns].all? { |field| result.fetch(field).positive? }
      signature = result.slice('checksum', 'rows_total', 'iterations', 'warmup', 'fingerprint')
      raise "canonical checksums/rows differ #{name}" if @signatures[name] && @signatures[name] != signature
      @signatures[name] = signature
      @peaks << entry.fetch('sampled_peak_worker_rss_bytes')
    end
    report
  end

  def p50s(report)
    report.fetch('results').to_h { |entry| [entry.fetch('workload'), entry.fetch('result').fetch('p50_ns')] }
  end

  def build(directory:, root:, baseline:, final_source:, runtime_info:)
    @directory, @root, @runtime_info = directory, root, runtime_info
    @signatures, @peaks = {}, []
    @canonical = Bench.load_corpus(File.join(root, 'bench/corpus')).select { |job| JOBS.include?(job.name) }.to_h { |job| [job.name, job.to_h] }
    reports = STATES.map do |state|
      check_report("file-4.0-yjit-#{state}", state.end_with?('before') ? baseline : final_source, '4.0-yjit')
    end
    paired = JOBS.to_h do |name|
      data = reports.map { |report| p50s(report).fetch(name) }
      before, final = data.values_at(0, 3), data.values_at(1, 2)
      [name, { before_us: before.sum / 2000.0, final_us: final.sum / 2000.0,
        speedup: before.sum.fdiv(final.sum), elapsed_change_percent: (final.sum.fdiv(before.sum) - 1) * 100,
        before_p50_ns: before, final_p50_ns: final,
        final_medians_below_both_before: final.max < before.min,
        final_medians_above_both_before: final.min > before.max }]
    end
    characterizations = %w[4.0-none 4.1-zjit].to_h do |runtime|
      [runtime, p50s(check_report("file-#{runtime}-final", final_source, runtime)).transform_values { |value| value / 1000.0 }]
    end
    cross = check_report('cross-file', final_source, '4.0-yjit', cross: true)
    cross_results = cross.fetch('results')
    if cross.fetch('statuses').fetch('rust').fetch('status') != 'ready'
      retry_report = check_report('cross-file-rust-retry', final_source, '4.0-yjit', rust_retry: true)
      %w[adapter_sha256 library_sha256 manifest_sha256 lock_sha256].each do |field|
        raise 'Rust retry adapter changed' unless cross.fetch('statuses').fetch('rust').fetch(field) == retry_report.fetch('statuses').fetch('rust').fetch(field)
      end
      cross_results += retry_report.fetch('results')
    end
    cross_data = JOBS.to_h do |name|
      rows = cross_results.select { |entry| entry['workload'] == name }.to_h { |entry| [entry.fetch('lane'), entry.fetch('result')] }
      [name, { p50_us: rows.transform_values { |entry| entry.fetch('p50_ns') / 1000.0 },
        mean_us: rows.transform_values { |entry| entry.fetch('total_ns').fdiv(entry.fetch('iterations')) / 1000.0 },
        ruby_p50_over: %w[go rust ts].to_h { |lane| [lane, rows.fetch('ruby-yjit').fetch('p50_ns').fdiv(rows.fetch(lane).fetch('p50_ns'))] } }]
    end
    { statistic: 'Source ABBA uses mean of two process p50 values from unchanged canonical per-operation measurements. Characterization/cross-core are single process per lane. Cold-open means a fresh database handle, not cold OS caches.',
      validation_scope: 'Canonical rendered-value checksums and consumed row counts; this adapter does not prove exact cost or column metadata equality.',
      cross_core_retry: retry_report ? 'Ruby, Go and TypeScript retain the original cross-file measurements. Rust alone was measured in cross-file-rust-retry.json after a separately bounded offline prebuild; all measured queries retain the 512 MiB cap.' : nil,
      yjit_abba: paired, final_characterization_p50_us: characterizations, cross_core: cross_data,
      validation: @signatures, max_sampled_rss_bytes: @peaks.max }
  end
end
