#!/usr/bin/env ruby
# frozen_string_literal: true

# Explicit opt-in comparison of the two Ruby packages' available public APIs.
require 'json'
require 'optparse'
require 'rbconfig'
require 'fileutils'
require 'time'
require 'open3'
require 'tmpdir'
require_relative 'source'
require_relative '../../impl/ruby-native/tools/bounded_process'

root = File.expand_path('../..', __dir__)
options = { ruby: RbConfig.ruby, jit: 'yjit',
            lanes: %w[wrapper native-prepared native-query native-eager] }
OptionParser.new do |parser|
  parser.banner = 'Usage: mise run ruby-native:binding-compare -- --output PATH [options]'
  parser.on('--ruby PATH') { |value| options[:ruby] = File.expand_path(value) }
  parser.on('--native-source PATH') { |value| options[:native_source] = File.expand_path(value) }
  parser.on('--workloads LIST', Array) { |value| options[:workloads] = value }
  parser.on('--fresh-sql') { options[:fresh_sql] = true }
  parser.on('--jit NAME', %w[yjit zjit none]) { |value| options[:jit] = value }
  parser.on('--output PATH') { |value| options[:output] = File.expand_path(value) }
  parser.on('--file PATH') { |value| options[:file] = File.expand_path(value) }
  parser.on('--lanes LIST', Array) { |value| options[:lanes] = value }
  parser.on('--verify-only') { options[:verify_only] = true }
  parser.on('--disable-pipeline') { options[:disable_pipeline] = true }
end.parse!
abort 'Specify --output PATH' unless options[:output]
abort 'Unknown lane' unless (options[:lanes] - %w[wrapper native-prepared native-query native-eager]).empty?
abort 'Specify unique, nonempty lanes' if options[:lanes].empty? || options[:lanes].uniq != options[:lanes]
abort '--fresh-sql requires unprepared lanes' if options[:fresh_sql] && options[:lanes].include?('native-prepared')

native_source = options.fetch(:native_source, File.join(root, 'impl/ruby-native'))
wrapper_source = File.join(root, 'impl/ruby')
wrapper_library = ENV.fetch('JED_RUBY_LIB', File.join(wrapper_source, 'ext/target/release/libjed_ruby.so'))
abort 'Build the release binding first: mise run ruby:build' unless File.file?(wrapper_library)
source_hashes = lambda do
  {
    native: NativeRubySource.digest(native_source),
    wrapper: Digest::SHA256.hexdigest(Dir[File.join(wrapper_source, 'lib/**/*.rb'),
      File.join(wrapper_source, 'ext/src/**/*.rs')].sort.map { |path| File.binread(path) }.join),
    rust: Digest::SHA256.hexdigest(Dir[File.join(root, 'impl/rust/src/**/*.rs')].sort.map { |path| File.binread(path) }.join),
    wrapper_library: Digest::SHA256.file(wrapper_library).hexdigest,
    worker: Digest::SHA256.file(File.join(__dir__, 'binding_compare_lane.rb')).hexdigest,
    plans: %w[plan.json compiled-plan.json pipeline-plan.json].to_h { |name| [name, Digest::SHA256.file(File.join(__dir__, 'compare', name)).hexdigest] },
    database: options[:file] && Digest::SHA256.file(options[:file]).hexdigest
  }
end
report = { created_at: Time.now.utc.iso8601, options: options,
           git_commit: Open3.capture2('git', 'rev-parse', 'HEAD', chdir: root).first.strip,
           cpu: File.read('/proc/cpuinfo')[/^model name\s*:\s*(.+)$/, 1],
           source_sha256: source_hashes.call, lanes: {}, errors: [] }
flags = options[:jit] == 'none' ? %w[--disable-yjit --disable-zjit] : ["--#{options[:jit]}"]
options[:lanes].each do |lane|
  warn "binding comparison: #{lane}, #{options[:jit]}#{options[:file] ? ', file' : ', memory'}"
  command = [options[:ruby], *flags, File.join(__dir__, 'binding_compare_lane.rb'), lane]
  workspace = nil
  if options[:file]
    workspaces = File.join(root, '.dev/ruby-native-binding-compare')
    FileUtils.mkdir_p(workspaces)
    workspace = Dir.mktmpdir("#{lane}-", workspaces)
    private_file = File.join(workspace, 'large.jed')
    FileUtils.cp(options[:file], private_file)
    command += ['--file', private_file]
  end
  command << '--verify-only' if options[:verify_only]
  command += ['--workloads', options[:workloads].join(',')] if options[:workloads]
  command << '--fresh-sql' if options[:fresh_sql]
  command << '--disable-pipeline' if options[:disable_pipeline]
  result = JedNativeTools::BoundedProcess.run(command, timeout: 300,
    memory_bytes: 512 * 1024**2, address_space_bytes: 2 * 1024**3,
    env: { 'RUBYOPT' => nil, 'RUBY_YJIT_ENABLE' => nil, 'RUBY_ZJIT_ENABLE' => nil,
           'RUBY_NATIVE_SOURCE' => native_source, 'JED_RUBY_LIB' => wrapper_library }, chdir: root)
  entry = { success: result.success?, reason: result.reason, peak_rss_bytes: result.peak_rss_bytes,
            memory_limit_bytes: result.memory_bytes, address_space_bytes: result.address_space_bytes,
            command: command, stderr: result.stderr }
  begin
    entry[:result] = JSON.parse(result.stdout) if result.success?
  rescue JSON::ParserError => error
    entry[:success] = false
    entry[:reason] = error.message
  end
  report[:lanes][lane] = entry
  report[:errors] << "#{lane}: #{entry[:reason]}" unless entry[:success]
  FileUtils.mkdir_p(File.dirname(options[:output]))
  File.write(options[:output], JSON.pretty_generate(report) + "\n")
  break if result.interrupted
ensure
  FileUtils.remove_entry(workspace) if workspace && File.directory?(workspace)
end
report[:source_sha256_after] = source_hashes.call
report[:errors] << 'Sources changed during measurement' unless report[:source_sha256] == report[:source_sha256_after]
expected = {}
expected_names = nil
report[:lanes].each do |lane, entry|
  next unless entry[:success]
  result = entry.fetch(:result)
  expected_jits = { 'yjit' => [true, false], 'zjit' => [false, true], 'none' => [false, false] }
  actual_jits = [!!result['yjit'], !!result['zjit']]
  report[:errors] << "#{lane}: requested JIT unavailable" unless actual_jits == expected_jits.fetch(options[:jit])
  jobs = result.fetch('workloads')
  names = jobs.map { |job| job.fetch('name') }
  report[:errors] << "#{lane}: workload set differs" if expected_names && expected_names != names
  expected_names ||= names
  jobs.each do |job|
    name = job.fetch('name')
    answer = job.values_at('validation_sha256', 'validation_rows', 'validation_costs', 'rows_per_sample')
    report[:errors] << "#{lane}: #{name} answer/cost mismatch" if expected.key?(name) && expected[name] != answer
    expected[name] ||= answer
    callbacks = job.fetch('callback_counts')
    rows = job.fetch('validation_rows').sum
    if name == 'host_scalar' || name.match?(/\Ahost_.*_row\z/)
      report[:errors] << "#{lane}: #{name} callback count differs" unless callbacks.fetch('row_calls') == rows
    elsif name.match?(/\Ahost_.*_batch\z/)
      sizes = callbacks.fetch('batch_sizes')
      unless sizes.sum == rows && sizes.max.to_i > 1 && sizes.length == callbacks.fetch('batch_calls')
        report[:errors] << "#{lane}: #{name} did not batch all rows"
      end
    end
    if name == 'file_pk'
      report[:errors] << "#{lane}: file probes missed rows" unless job.fetch('validation_rows').all?(1)
      unless options[:verify_only] || job.fetch('rows_per_sample').all?(job.fetch('iterations'))
        report[:errors] << "#{lane}: measured file probes missed rows"
      end
    end
  end
  %w[buffered streamed].each do |site|
    row = jobs.find { |job| job['name'] == "host_#{site}_row" }
    batch = jobs.find { |job| job['name'] == "host_#{site}_batch" }
    next unless row && batch
    report[:errors] << "#{lane}: #{site} row/batch values differ" unless row['answer_sha256'] == batch['answer_sha256']
  end
end
File.write(options[:output], JSON.pretty_generate(report) + "\n")
warn "Saved #{options[:output]}"
abort report[:errors].join("\n") unless report[:errors].empty?
