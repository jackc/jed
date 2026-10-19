#!/usr/bin/env ruby
# frozen_string_literal: true

require 'json'
require 'open3'
require 'fileutils'
require 'rbconfig'
require 'tmpdir'
require 'time'
require 'digest'
require_relative 'source'
require_relative '../../impl/ruby-native/tools/bounded_process'

module NativeRubyCompare
  ROOT = File.expand_path('../..', __dir__)
  SOURCES = File.join(__dir__, 'compare')
  BUILD = File.join(Dir.tmpdir, 'jed-ruby-native-compare')
  RESULTS = File.join(__dir__, 'results')

  module_function

  def capture(*command, directory: ROOT, environment: {})
    output, status = Open3.capture2e(environment, *command, chdir: directory)
    [output, status.success?]
  end

  def build(lanes)
    FileUtils.mkdir_p(BUILD)
    status = {}
    if lanes.include?('go')
      output, okay = capture('go', 'build', '-o', File.join(BUILD, 'go-lane'), '.',
                             directory: File.join(SOURCES, 'go'), environment: { 'GOCACHE' => File.join(BUILD, 'go-cache') })
      status['go'] = { status: okay ? 'ready' : 'build_failed', detail: output }
    end
    if lanes.include?('rust')
      output, okay = capture('cargo', 'build', '--release', '--offline', '--manifest-path', File.join(SOURCES, 'rust/Cargo.toml'),
                             environment: { 'CARGO_TARGET_DIR' => File.join(BUILD, 'rust-target') })
      status['rust'] = { status: okay ? 'ready' : 'build_failed', detail: output }
    end
    lanes.grep(/\Aruby/).each do |lane|
      flag = { 'ruby-yjit' => '--yjit', 'ruby-zjit' => '--zjit' }[lane]
      check = case lane
              when 'ruby-yjit' then 'abort "YJIT unavailable" unless defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?; puts RUBY_DESCRIPTION'
              when 'ruby-zjit' then 'abort "ZJIT unavailable" unless defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?; puts RUBY_DESCRIPTION'
              else 'puts RUBY_DESCRIPTION'
              end
      flags = flag ? [flag] : ['--disable-yjit', '--disable-zjit']
      output, okay = capture(RbConfig.ruby, *flags, '-e', check, environment: { 'RUBYOPT' => nil })
      status[lane] = { status: okay ? 'ready' : 'unavailable', detail: output }
    end
    if lanes.include?('ts')
      output, okay = capture('node', '--input-type=module', '-e', "await import('./impl/ts/src/file.ts'); console.log(process.version)")
      status['ts'] = { status: okay ? 'ready' : 'unavailable', detail: output }
    end
    status
  end

  def plan_file(plan)
    path = File.join(BUILD, 'plan.tsv')
    File.open(path, 'w') do |file|
      file.puts ['config', plan.fetch('repetitions'), plan.fetch('validation_iterations')].join("\t")
      file.puts ['setup', 'CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)'].join("\t")
      values = plan.fetch('rows').times.map { |i| "(#{i},#{i % 10},#{(i * 37) % 10000},'row#{i}')" }.join(',')
      file.puts ['setup', "INSERT INTO items VALUES #{values}"].join("\t")
      plan.fetch('workloads').each do |workload|
        values = workload.values_at('name', 'iterations', 'warmup', 'offset', 'step', 'span', 'sql')
        raise 'tabs and newlines are not permitted in the benchmark plan' if values.any? { |value| value.to_s.match?(/[\t\r\n]/) }

        file.puts ['workload', *values].join("\t")
      end
    end
    path
  end

  def command_for(lane, plan, verify)
    command = case lane
              when 'ruby' then [RbConfig.ruby, '--disable-yjit', '--disable-zjit', File.join(SOURCES, 'lane.rb')]
              when 'ruby-yjit' then [RbConfig.ruby, '--yjit', File.join(SOURCES, 'lane.rb')]
              when 'ruby-zjit' then [RbConfig.ruby, '--zjit', File.join(SOURCES, 'lane.rb')]
              when 'go' then [File.join(BUILD, 'go-lane')]
              when 'rust' then [File.join(BUILD, 'rust-target/release/jed-ruby-experiment-compare')]
              when 'ts' then ['node', File.join(SOURCES, 'lane.ts')]
              else raise "unknown lane #{lane}"
              end
    command + [plan] + (verify ? ['--verify-only'] : [])
  end

  def host
    cpu = File.exist?('/proc/cpuinfo') ? File.read('/proc/cpuinfo')[/^model name\s*:\s*(.+)$/, 1] : nil
    {
      platform: RUBY_PLATFORM, cpu: cpu, processors: Etc.nprocessors,
      git_commit: capture('git', 'rev-parse', 'HEAD').first.strip,
      rust: capture('rustc', '--version').first.strip,
      go: capture('go', 'version').first.strip,
      node: capture('node', '--version').first.strip,
      ruby: RUBY_DESCRIPTION,
      openssl: capture(RbConfig.ruby, '-ropenssl', '-e', 'puts OpenSSL::OPENSSL_VERSION').first.strip
    }
  end

  def run
    require 'etc'
    lanes = ENV.fetch('RUBY_NATIVE_LANES', 'ruby,ruby-yjit,ruby-zjit,go,rust,ts').split(',')
    phase = ENV.fetch('RUBY_NATIVE_PHASE', 'baseline')
    raise 'invalid phase name' unless phase.match?(/\A[a-zA-Z0-9_-]+\z/)

    plan_path = File.expand_path(ENV.fetch('RUBY_NATIVE_COMPARE_PLAN', File.join(SOURCES, 'plan.json')))
    plan_json = File.binread(plan_path)
    plan = JSON.parse(plan_json)
    statuses = build(lanes)
    statuses.each { |lane, status| warn "compare #{lane}: #{status[:status]}#{status[:status] == 'ready' ? '' : "\n#{status[:detail]}"}" }
    if ARGV.include?('--build-only')
      puts JSON.pretty_generate(statuses)
      return statuses.values.any? { |status| status[:status] == 'build_failed' } ? 1 : 0
    end
    verify = ARGV.include?('--verify-only')
    ENV['RUBY_NATIVE_SOURCE'] = NativeRubySource.resolve(ROOT, phase: verify ? 'verification' : phase)
    source_hash = NativeRubySource.digest(ENV.fetch('RUBY_NATIVE_SOURCE'))
    path = plan_file(plan)
    results = {}
    resources = {}
    lanes.each do |lane|
      next unless statuses[lane][:status] == 'ready'

      warn "compare #{lane}: #{verify ? 'validating' : 'measuring'}"
      worker = JedNativeTools::BoundedProcess.run(command_for(lane, path, verify),
        timeout: Integer(ENV.fetch('RUBY_NATIVE_COMPARE_TIMEOUT', '1800')),
        # Go and V8 reserve large virtual regions; resident memory is capped
        # identically, while Ruby also has a tighter allocation ceiling.
        address_space_bytes: lane.start_with?('ruby') ? 1024**3 : 16 * 1024**3,
        chdir: ROOT, env: { 'RUBYOPT' => nil })
      output, okay = worker.stdout, worker.success?
      resources[lane] = { peak_rss_bytes: worker.peak_rss_bytes, memory_limit_bytes: worker.memory_bytes }
      entries = output.lines.filter_map do |line|
        JSON.parse(line) if line.start_with?('{')
      end
      results[lane] = entries
      unless okay && entries.length == plan['workloads'].length
        detail = [worker.reason, worker.stderr.lines.first(10).join, output].compact.join("\n")
        statuses[lane] = { status: 'failed', detail: detail }
        warn "compare #{lane}: FAILED\n#{detail}"
      end
      break if worker.interrupted
    end
    expected = {}
    results.each do |lane, workloads|
      workloads.each do |workload|
        answer = workload.values_at('checksum', 'validation_rows')
        name = workload.fetch('workload')
        if expected[name] && expected[name] != answer
          statuses[lane] = { status: 'checksum_mismatch', detail: "#{name}: #{answer.inspect}, expected #{expected[name].inspect}" }
        end
        expected[name] ||= answer
      end
    end
    FileUtils.mkdir_p(RESULTS)
    artifact = File.join(RESULTS, "#{phase}#{verify ? '-verify' : ''}.json")
    report = { schema: 1, phase: phase, verification_only: verify, created_at: Time.now.utc.iso8601,
               host: host, plan_path: plan_path, plan_sha256: Digest::SHA256.hexdigest(plan_json), plan: plan,
               ruby_source: ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')),
               statuses: statuses, results: results, resources: resources }
    report[:ruby_source_sha256] = source_hash
    report[:ruby_source_sha256_after] = NativeRubySource.digest(report[:ruby_source])
    statuses['source'] = { status: 'failed', detail: 'Ruby source changed during measurement' } unless source_hash == report[:ruby_source_sha256_after]
    File.write(artifact, JSON.pretty_generate(report) + "\n")
    warn "comparison artifact: #{artifact}"
    unless verify
      plan['workloads'].each do |workload|
        timings = results.filter_map do |lane, entries|
          entry = entries.find { |candidate| candidate['workload'] == workload['name'] }
          next unless entry && !entry['samples_ns'].empty?

          sorted = entry['samples_ns'].sort
          median = sorted[(sorted.length - 1) / 2].fdiv(entry['iterations'])
          "#{lane}=#{format('%.1f', median)}ns"
        end
        puts "#{workload['name']}: #{timings.join(' ')}"
      end
    end
    statuses.values.any? { |status| %w[failed build_failed checksum_mismatch].include?(status[:status]) } ? 1 : 0
  end
end

exit NativeRubyCompare.run
