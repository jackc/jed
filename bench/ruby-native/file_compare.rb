#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit experiment tooling. Preparation never runs SQL; --run is required
# to measure the original shared workloads on private copies of real files.
require 'json'
require 'fileutils'
require 'open3'
require 'rbconfig'
require 'optparse'
require 'shellwords'
require 'time'
require 'etc'
require_relative 'source'
require_relative '../ruby/lib/bench'
require_relative '../../impl/ruby-native/tools/bounded_process'

module NativeRubyFileCompare
  ROOT = File.expand_path('../..', __dir__)
  BUILD = File.join(ROOT, '.dev/ruby-native-file-compare')
  WORKSPACES = File.join(BUILD, 'workspaces')
  CORPUS = File.join(ROOT, 'bench/corpus')
  DATA = File.join(ROOT, '.dev/ruby-native-bench-data')
  DEFAULT_WORKLOADS = %w[point_lookup_pk cold_open_pk_small insert_commit_durable].freeze
  ALLOWED_WORKLOADS = (DEFAULT_WORKLOADS + ['full_scan_agg']).freeze
  DEFAULT_LANES = %w[ruby ruby-yjit go rust ts].freeze
  ALLOWED_LANES = (DEFAULT_LANES + %w[baseline-ruby baseline-yjit baseline-zjit ruby-zjit]).freeze
  MEMORY = 512 * 1024**2
  module_function

  def sha(path) = Digest::SHA256.file(path).hexdigest

  def baseline_source
    source = ENV.fetch('RUBY_NATIVE_BASELINE_SOURCE') do
      raise 'Historical file lanes require RUBY_NATIVE_BASELINE_SOURCE; see bench/ruby-native/ARCHIVE.md'
    end
    NativeRubySource.resolve(ROOT, phase: 'working-baseline', source: source)
  end

  def host
    versions = { go: ['go', 'version'], rust: ['rustc', '--version'], node: ['node', '--version'],
                 commit: ['git', 'rev-parse', 'HEAD'] }.transform_values do |command|
      output, status = Open3.capture2e(*command, chdir: ROOT)
      status.success? ? output.strip : nil
    end
    versions.merge(ruby: RUBY_DESCRIPTION, platform: RUBY_PLATFORM, processors: Etc.nprocessors,
      cpu: File.read('/proc/cpuinfo')[/^model name\s*:\s*(.+)$/, 1])
  end

  def write_report(path, report)
    FileUtils.mkdir_p(File.dirname(path))
    temporary = "#{path}.tmp"
    File.write(temporary, JSON.pretty_generate(report) + "\n")
    File.rename(temporary, path)
  end

  def workspace_filesystem
    FileUtils.mkdir_p(WORKSPACES)
    output, status = Open3.capture2e('df', '-PT', WORKSPACES)
    fields = output.lines.last.to_s.strip.split(/\s+/, 7)
    raise "cannot identify benchmark filesystem: #{output}" unless status.success? && fields.length == 7
    raise "durable file benchmarks require disk-backed workspaces, got #{fields[1]}" if %w[tmpfs ramfs devtmpfs].include?(fields[1])

    { directory: WORKSPACES, source: fields[0], type: fields[1], mount_point: fields[6],
      device: File.stat(WORKSPACES).dev }
  end

  def run_on_disk(**options, &block)
    original_tmpdir = ENV['TMPDIR']
    FileUtils.mkdir_p(WORKSPACES)
    ENV['TMPDIR'] = WORKSPACES
    # The supervisor chooses its workspace before applying child environment
    # options. Its child then inherits that workspace as TMPDIR, so Ruby's
    # nested supervisor and scratch databases stay on the same filesystem.
    JedNativeTools::BoundedProcess.run(**options, &block)
  ensure
    original_tmpdir.nil? ? ENV.delete('TMPDIR') : ENV['TMPDIR'] = original_tmpdir
  end

  def checked_replace(source, before, after)
    raise "canonical adapter changed: missing #{before.inspect}" unless source.scan(before).length == 1
    source.sub(before, after)
  end

  def capture(command, chdir: ROOT, env: {}, timeout: 600)
    result = JedNativeTools::BoundedProcess.run(command, timeout: timeout,
      memory_bytes: MEMORY, address_space_bytes: 16 * 1024**3, chdir: chdir, env: env)
    { status: result.success? ? 'ready' : 'unavailable', command: command,
      detail: [result.reason, result.stdout, result.stderr].compact.join("\n"),
      peak_rss_bytes: result.peak_rss_bytes }
  end

  def prepare_corpora(names)
    source = File.read(File.join(CORPUS, 'benchmarks.toml'), encoding: Encoding::UTF_8)
    chunks = source.split(/(?=^\[\[bench\]\]\s*$)/)
    header = chunks.shift
    chunks = chunks.to_h do |chunk|
      document = TomlRB.parse(header + chunk)
      [document.fetch('bench').fetch(0).fetch('name'), chunk]
    end
    names.to_h do |name|
      path = File.join(BUILD, 'corpus', name)
      FileUtils.mkdir_p(path)
      File.binwrite(File.join(path, 'benchmarks.toml'), header + chunks.fetch(name))
      FileUtils.cp(File.join(CORPUS, 'datasets.toml'), path)
      workload = Bench.load_corpus(path).fetch(0)
      canonical = Bench.load_corpus(CORPUS).find { |item| item.name == name }
      raise "selected workload differs from canonical #{name}" unless workload.to_h == canonical.to_h
      if workload.dataset != 'scratch'
        raise "missing prepared dataset #{workload.dataset}" unless File.file?(File.join(DATA, "#{workload.dataset}.jed"))
        raise "stale prepared dataset #{workload.dataset}" unless Bench.read_sidecar(DATA, workload.dataset, 'jed') == Bench.corpus_fingerprint(CORPUS)
      end
      [name, { corpus: path, dataset: workload.dataset, iterations: workload.iterations,
               warmup: workload.warmup, sql: workload.sql, source_sha256: sha(File.join(path, 'benchmarks.toml')) }]
    end
  end

  def prepare_go
    original = File.join(ROOT, 'bench/go/cmd/bench-jed/main.go')
    source = checked_replace(File.read(original), 'Environment: env, Synchronous: synchronous}',
      'Environment: env, Synchronous: synchronous, Locking: jed.LockingExclusive}')
    source = checked_replace(source, 'jed.OpenOptions{Environment: env}',
      'jed.OpenOptions{Environment: env, Locking: jed.LockingExclusive}')
    adapted = File.join(BUILD, 'go-exclusive.go')
    File.write(adapted, source)
    overlay = File.join(BUILD, 'go-overlay.json')
    File.write(overlay, JSON.generate('Replace' => { original => adapted }))
    binary = File.join(BUILD, 'bench-go')
    status = capture(['go', 'build', '-overlay', overlay, '-o', binary, './cmd/bench-jed'],
      chdir: File.join(ROOT, 'bench/go'), env: { 'GOPROXY' => 'off', 'GOSUMDB' => 'off',
        'GOCACHE' => File.join(ROOT, '.dev/ruby-native-bench-build/cache') })
    status.merge(command: [binary], adapter_sha256: sha(original), overlay_sha256: sha(adapted))
  end

  def prepare_rust
    directory = File.join(BUILD, 'rust')
    FileUtils.mkdir_p(File.join(directory, 'src'))
    %w[lib.rs bin/bench_jed.rs].each do |name|
      destination = File.join(directory, 'src', name)
      FileUtils.mkdir_p(File.dirname(destination))
      FileUtils.cp(File.join(ROOT, 'bench/rust/src', name), destination)
    end
    # The same approved dependencies used by the canonical jed adapter. The
    # PostgreSQL/SQLite binaries and their unrelated drivers are not built.
    canonical = File.read(File.join(ROOT, 'bench/rust/Cargo.toml'))
    dependencies = canonical.lines.grep(/\A(?:jed|toml|sha2) = /).join
    dependencies = dependencies.sub('"../../impl/rust"', JSON.generate(File.join(ROOT, 'impl/rust')))
    File.write(File.join(directory, 'Cargo.toml'), <<~TOML)
      [package]
      name = "jed-bench"
      version = "0.0.0"
      edition = "2024"
      [dependencies]
      #{dependencies}
      [[bin]]
      name = "bench-jed"
      path = "src/bin/bench_jed.rs"
    TOML
    # Keep the core's dependency resolution identical to the companion memory
    # comparison, allowing its already-built release core to be reused offline.
    FileUtils.cp(File.join(__dir__, 'compare/rust/Cargo.lock'), directory)
    target = File.join(Dir.tmpdir, 'jed-ruby-native-compare/rust-target')
    status = capture(['cargo', 'build', '--release', '--offline', '--manifest-path', File.join(directory, 'Cargo.toml'), '--bin', 'bench-jed'],
      env: { 'CARGO_TARGET_DIR' => target, 'CARGO_BUILD_JOBS' => '1' })
    status.merge(command: [File.join(target, 'release/bench-jed')],
      adapter_sha256: sha(File.join(directory, 'src/bin/bench_jed.rs')),
      library_sha256: sha(File.join(directory, 'src/lib.rs')),
      manifest_sha256: sha(File.join(directory, 'Cargo.toml')),
      lock_sha256: sha(File.join(directory, 'Cargo.lock')))
  end

  def ts_lock_probe
    <<~JS
      import { mkdtempSync, rmSync } from 'node:fs';
      import { tmpdir } from 'node:os';
      import { join } from 'node:path';
      const { osLocks } = await import(#{JSON.generate(File.join(ROOT, 'impl/ts/src/coordinator.ts'))});
      const directory = mkdtempSync(join(tmpdir(), 'jed-ts-lock-preflight-'));
      let files;
      try {
        // openBundle invokes the production native loader and its ABI check.
        files = osLocks.openBundle(join(directory, 'probe.jed'));
        if (!osLocks.tryLock(files.presence, true)) throw new Error('exclusive lock unavailable');
        osLocks.unlock(files.presence);
        console.log('Matching native lock ABI loaded; exclusive lock/unlock passed');
      } finally {
        if (files) osLocks.closeFiles(files);
        rmSync(directory, { recursive: true, force: true });
      }
    JS
  end

  def prepare_ts
    lock_check = capture(['node', '--input-type=module', '-e', ts_lock_probe])
    lock_artifact = File.join(ROOT, 'impl/ts/jed_lock.node')
    lock_check[:lock_artifact_sha256] = sha(lock_artifact) if File.file?(lock_artifact)
    lock_check[:coordinator_sha256] = sha(File.join(ROOT, 'impl/ts/src/coordinator.ts'))
    return lock_check unless lock_check[:status] == 'ready'

    directory = File.join(BUILD, 'ts')
    FileUtils.mkdir_p(File.join(directory, 'src'))
    %w[package.json package-lock.json].each { |name| FileUtils.cp(File.join(ROOT, 'bench/ts', name), directory) }
    install = capture(['npm', 'ci', '--offline', '--ignore-scripts', '--no-audit', '--no-fund'],
      chdir: directory, env: { 'npm_config_logs_dir' => File.join(BUILD, 'npm-logs') })
    return install unless install[:status] == 'ready'

    original = File.join(ROOT, 'bench/ts/src/bench-jed.ts')
    source = checked_replace(File.read(original), "    synchronous,\n", "    synchronous,\n    locking: \"exclusive\",\n")
    source = checked_replace(source, '{ environment: benchEnvironment() }', '{ environment: benchEnvironment(), locking: "exclusive" }')
    source = source.gsub(/from "(\.\.\/\.\.\/\.\.\/impl\/ts\/src\/[^\"]+)"/) do
      "from #{JSON.generate(File.expand_path(Regexp.last_match(1), File.dirname(original)))}"
    end
    adapted = File.join(directory, 'src/bench-jed.ts')
    File.write(adapted, source)
    FileUtils.cp(File.join(ROOT, 'bench/ts/src/lib.ts'), File.join(directory, 'src/lib.ts'))
    probe = "await import('smol-toml'); await import(#{JSON.generate(File.join(ROOT, 'impl/ts/src/lib.ts'))}); console.log(process.version)"
    status = capture(['node', '--input-type=module', '-e', probe], chdir: directory)
    status.merge(command: ['node', adapted], adapter_sha256: sha(original), overlay_sha256: sha(adapted),
      library_sha256: sha(File.join(directory, 'src/lib.ts')), lock_preflight: lock_check)
  end

  def prepare_ruby(lane, sources)
    baseline = lane.start_with?('baseline-')
    source = sources.fetch(baseline ? :baseline : :current)
    jit = lane.end_with?('yjit') ? 'yjit' : lane.end_with?('zjit') ? 'zjit' : nil
    flags = jit ? ["--#{jit}"] : ['--disable-yjit', '--disable-zjit']
    probe = "require #{JSON.generate(File.join(source, 'lib/jed_native'))}; "
    probe += "abort #{JSON.generate("#{jit.upcase} unavailable")} unless defined?(RubyVM::#{jit.upcase}) && RubyVM::#{jit.upcase}.enabled?; " if jit
    probe += 'puts RUBY_DESCRIPTION'
    status = capture([RbConfig.ruby, *flags, '-e', probe], env: { 'RUBYOPT' => nil })
    status.merge(command: [RbConfig.ruby, *flags, File.join(__dir__, 'bench_jed.rb')],
      source: source, source_sha256: NativeRubySource.digest(source), jit: jit || 'interpreter',
      adapter_sha256: sha(File.join(__dir__, 'bench_jed.rb')), library_sha256: sha(File.join(ROOT, 'bench/ruby/lib/bench.rb')))
  end

  def run_workload(lane, status, name, workload, timeout)
    address_space = status[:source] ? 2 * 1024**3 : 16 * 1024**3
    worker = run_on_disk(timeout: timeout, memory_bytes: MEMORY, address_space_bytes: address_space) do |workspace|
      if workload[:dataset] != 'scratch'
        %W[#{workload[:dataset]}.jed #{workload[:dataset]}.jed.fingerprint].each do |filename|
          FileUtils.cp(File.join(DATA, filename), workspace)
        end
      end
      output = File.join(workspace, 'result.jsonl')
      environment = { 'RUBYOPT' => nil, 'RUBY_NATIVE_BENCH_SMOKE' => nil, 'JED_BENCH_LOCKING' => 'exclusive',
        'RUBY_NATIVE_WORK_MEM' => nil, 'RUBY_NATIVE_BENCH_NAMES' => nil,
        'RUBY_NATIVE_SOURCE' => status[:source], 'RUBY_NATIVE_BENCH_TIMEOUT' => timeout.to_s,
        'RUBY_NATIVE_MEMORY_BYTES' => MEMORY.to_s, 'RUBY_NATIVE_ADDRESS_SPACE_BYTES' => address_space.to_s }
      okay = system(environment, *status.fetch(:command), workload[:corpus], workspace, output)
      rows = File.file?(output) ? File.readlines(output).reject { |line| line.strip.empty? }.map { |line| JSON.parse(line) } : []
      nested = File.file?(output + '.status.json') ? JSON.parse(File.read(output + '.status.json')) : []
      { completed: okay, results: rows, ruby_workers: nested }
    end
    data = worker.value || {}
    results = data['results'] || []
    row = results.first
    okay = worker.success? && data['completed'] && results.length == 1 && row['bench'] == name &&
      row['iterations'] == workload[:iterations] && row['warmup'] == workload[:warmup]
    peak = [worker.peak_rss_bytes, *(data['ruby_workers'] || []).map { |entry| entry['peak_rss_bytes'].to_i }].max
    { lane: lane, workload: name, status: okay ? 'completed' : 'failed', result: row,
      reason: okay ? nil : [worker.reason, worker.stderr, data['ruby_workers']].compact.join("\n"),
      sampled_peak_worker_rss_bytes: peak, memory_bytes: MEMORY, address_space_bytes: address_space,
      timeout_seconds: timeout, interrupted: worker.interrupted }
  end

  def main(arguments)
    options = { run: false, lanes: DEFAULT_LANES, workloads: DEFAULT_WORKLOADS, timeout: 1800,
      report: File.join(__dir__, 'results/file-comparison.json') }
    parser = OptionParser.new do |cli|
      cli.banner = 'Usage: mise run ruby-native:file-compare [--prepare-only | --run] [options]'
      cli.on('--prepare-only', 'Build/check cached adapters; execute no SQL (default)') { options[:run] = false }
      cli.on('--run', 'Run canonical workload iterations sequentially after preparation') { options[:run] = true }
      cli.on('--lanes NAMES', 'Comma-separated lanes') { |value| options[:lanes] = value.split(',') }
      cli.on('--workloads NAMES', 'Canonical names; full_scan_agg is optional') { |value| options[:workloads] = value.split(',') }
      cli.on('--timeout SECONDS', Integer, 'Maximum seconds per complete lane (default 1800)') { |value| options[:timeout] = value }
      cli.on('--report PATH', 'Measured result artifact') { |value| options[:report] = File.expand_path(value) }
      cli.on('-h', '--help') { puts cli; return 0 }
    end
    parser.parse!(arguments)
    raise 'unexpected positional arguments' unless arguments.empty?
    raise 'unknown or empty lane selection' if options[:lanes].empty? || !(options[:lanes] - ALLOWED_LANES).empty?
    raise 'unknown or empty workload selection' if options[:workloads].empty? || !(options[:workloads] - ALLOWED_WORKLOADS).empty?
    raise 'timeout must be positive' unless options[:timeout].positive?
    FileUtils.mkdir_p(BUILD)
    filesystem = workspace_filesystem
    sources = { current: File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native'))) }
    sources[:baseline] = baseline_source if options[:lanes].any? { |lane| lane.start_with?('baseline-') }
    workloads = prepare_corpora(options[:workloads])
    statuses = options[:lanes].to_h do |lane|
      warn "file comparison #{lane}: preparing (no workloads)"
      status = case lane
               when 'go' then prepare_go
               when 'rust' then prepare_rust
               when 'ts' then prepare_ts
               else prepare_ruby(lane, sources)
               end
      warn "file comparison #{lane}: #{status[:status]}"
      [lane, status]
    end
    report = { schema: 1, mode: options[:run] ? 'canonical_file_workloads' : 'preparation_only',
      created_at: Time.now.utc.iso8601, host: host, locking: 'exclusive', memory_bytes: MEMORY,
      lane_timeout_seconds: options[:timeout], workspace_filesystem: filesystem,
      corpus_sha256: sha(File.join(CORPUS, 'benchmarks.toml')),
      dataset_fingerprint: Bench.corpus_fingerprint(CORPUS), workloads: workloads,
      statuses: statuses, results: [], notes: [
        'Original SQL, warmup, measured iterations, parameter streams, and checksums; one workload per process.',
        'Each workload uses a private dataset copy; cold_open measures a fresh database handle, not a cold operating-system file cache.',
        'All lane datasets and nested Ruby scratch workers use the recorded disk-backed workspace filesystem.',
      'Peak RSS is sampled; Ruby engine workers have their own supervisor inside the lane supervisor.' ] }
    report[:datasets] = workloads.values.map { |entry| entry[:dataset] }.uniq.reject { |name| name == 'scratch' }.to_h do |name|
      path = File.join(DATA, "#{name}.jed")
      [name, { path: path, bytes: File.size(path), sha256: sha(path) }]
    end
    prepared = File.join(BUILD, 'prepared.json')
    write_report(prepared, report)
    unless options[:run]
      puts "Prepared without running SQL: #{prepared}"
      return statuses.values.any? { |status| status[:status] != 'ready' } ? 1 : 0
    end
    write_report(options[:report], report)
    expected = {}
    statuses.each do |lane, status|
      next unless status[:status] == 'ready'
      deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + options[:timeout]
      workloads.each do |name, workload|
        remaining = deadline - Process.clock_gettime(Process::CLOCK_MONOTONIC)
        if remaining <= 0
          report[:results] << { lane: lane, workload: name, status: 'failed', reason: 'lane deadline exhausted' }
          next
        end
        warn "file comparison #{lane}: #{name}"
        entry = run_workload(lane, status, name, workload, remaining.ceil)
        if entry[:status] == 'completed'
          answer = entry[:result].values_at('checksum', 'rows_total')
          entry[:status] = 'checksum_mismatch' if expected.key?(name) && expected[name] != answer
          expected[name] ||= answer
        end
        report[:results] << entry
        write_report(options[:report], report)
        break if entry[:interrupted]
      end
      if status[:source]
        status[:final_source_sha256] = NativeRubySource.digest(status[:source])
        status[:status] = 'source_changed' unless status[:source_sha256] == status[:final_source_sha256]
      end
      break if report[:results].last&.dig(:interrupted)
    end
    report[:finished_at] = Time.now.utc.iso8601
    write_report(options[:report], report)
    puts "File comparison: #{options[:report]}"
    statuses.values.any? { |status| status[:status] != 'ready' } || report[:results].any? { |entry| entry[:status] != 'completed' } ? 1 : 0
  end
end

if $PROGRAM_NAME == __FILE__
  arguments = ARGV.empty? && ENV['usage_args'] ? Shellwords.split(ENV['usage_args']) : ARGV
  exit NativeRubyFileCompare.main(arguments)
end
