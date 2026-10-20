#!/usr/bin/env ruby
# frozen_string_literal: true

require 'fileutils'
require 'tmpdir'
require 'open3'

# Explicit-only disk checkpoint. Run under ruby-native:bounded; setup and all
# TracePoint probes are outside the samples, and every query is fully drained.
require 'json'
require 'optparse'
require 'rbconfig'
require 'time'
require_relative 'source'

module NativeBlockingDiagnostics
  ROOT = File.expand_path('../..', __dir__)
  PLAN = File.join(__dir__, 'compare/blocking-plan.json')
  APIS = %w[prepared query execute execute-prepared].freeze
  ADDRESS_SPACE_BYTES = 2 * 1024**3
  MEMORY_BYTES = 512 * 1024**2
  module_function

  def clock = Process.clock_gettime(Process::CLOCK_MONOTONIC, :nanosecond)
  def sha(value) = Digest::SHA256.hexdigest(JSON.generate(value))

  def filesystem(path)
    output, status = Open3.capture2e('df', '-PT', path)
    fields = output.lines.last.to_s.strip.split(/\s+/, 7)
    raise "cannot identify fixture filesystem: #{output}" unless status.success? && fields.length == 7
    raise "disk fixture cannot use #{fields[1]}" if %w[tmpfs ramfs devtmpfs].include?(fields[1])
    { path: path, source: fields[0], type: fields[1], mount_point: fields[6], device: File.stat(path).dev }
  end

  def setup(path, plan)
    JedNative.create(path, locking: :exclusive) do |database|
      current = database.session
      current.execute('BEGIN')
      current.execute('CREATE TABLE items(id i32 PRIMARY KEY, grp i32, score i32, label text)')
      plan.fetch('rows').times.each_slice(128) do |indexes|
        values = indexes.map { |index| "(#{index},#{index % 1024},#{(index * 37) % 10000 - 5000},'row#{index}')" }
        current.execute("INSERT INTO items VALUES #{values.join(',')}")
      end
      %w[left right].each do |side|
        current.execute("CREATE TABLE join_#{side}(id i32 PRIMARY KEY, k i32, score i32)")
        multiplier = side == 'left' ? 17 : 31
        plan.fetch('join_rows').times.each_slice(128) do |indexes|
          values = indexes.map { |index| "(#{index},#{index % 256},#{index * multiplier % 997})" }
          current.execute("INSERT INTO join_#{side} VALUES #{values.join(',')}")
        end
      end
      current.execute('COMMIT')
    ensure
      current&.close
    end
  end

  def runner(current, job, api)
    sql = job.fetch('sql')
    case api
    when 'prepared'
      statement = current.prepare(sql)
      ->(params) { current.query_prepared(statement, *params) }
    when 'execute-prepared'
      statement = current.prepare(sql)
      ->(params) { current.execute_prepared(statement, *params) }
    when 'query' then ->(params) { current.query(sql, *params) }
    when 'execute' then ->(params) { current.execute(sql, *params) }
    else raise "unknown API #{api}"
    end
  end

  def collect(run, params)
    result = run.call(params)
    rows = result.rows
    { rows: rows, columns: result.columns, column_types: result.column_types, cost: result.cost }
  ensure
    result&.close if result.respond_to?(:close)
  end

  def drain(run, params)
    result = run.call(params)
    count = 0
    values = result.is_a?(JedNative::Rows) ? result : result.rows
    values.each { count += 1 }
    count
  ensure
    result&.close if result.respond_to?(:close)
  end

  # Observe actual IO#write/IO#read returns for the private files created by the
  # spill classes. No method wrappers or counters remain on the timed path.
  def scratch_probe(directory)
    files, io_files, operators = [], {}, Hash.new(0)
    scratch_classes = [JedNative::ScratchFile, JedNative::ScratchBuckets]
    trace = TracePoint.new(:call, :return, :c_return) do |event|
      object = event.self
      if event.event == :return && event.method_id == :initialize && scratch_classes.include?(object.class)
        file = object.instance_variable_get(:@file)
        path = File.expand_path(file.path)
        raise 'scratch file escaped private disk directory' unless File.dirname(path) == directory
        entry = { kind: object.class.name, path: path, write_calls: 0, write_bytes: 0, read_calls: 0, read_bytes: 0 }
        files << entry
        io_files[file.to_io.object_id] = entry
      elsif event.event == :c_return && (entry = io_files[object.object_id])
        if event.method_id == :write
          entry[:write_calls] += 1
          entry[:write_bytes] += event.return_value
        elsif event.method_id == :read
          entry[:read_calls] += 1
          entry[:read_bytes] += event.return_value&.bytesize || 0
        end
      elsif event.event == :call && event.method_id == :push && object.is_a?(JedNative::SpillHashRows)
        operators['hash_rows_push'] += 1
      end
    end
    answer = trace.enable { yield }
    files.each do |entry|
      entry[:removed_after_drain] = !File.exist?(entry[:path])
      raise 'scratch file survived completed query' unless entry[:removed_after_drain]
    end
    [answer, { created: files.length, files: files, operators: operators,
      write_calls: files.sum { |entry| entry[:write_calls] }, write_bytes: files.sum { |entry| entry[:write_bytes] },
      read_calls: files.sum { |entry| entry[:read_calls] }, read_bytes: files.sum { |entry| entry[:read_bytes] } }]
  ensure
    trace&.disable
  end

  def measure(run, job, repetitions, expected_rows)
    params = job.fetch('parameters')
    job.fetch('warmup').times { |index| drain(run, params[index % params.length]) }
    count = job.fetch('iterations')
    expected = count.times.sum { |index| expected_rows[index % params.length] }
    samples = Array.new(repetitions) do
      rows = 0
      allocated, gc = GC.stat(:total_allocated_objects), GC.stat(:count)
      started = clock
      count.times { |index| rows += drain(run, params[index % params.length]) }
      elapsed = clock - started
      allocations = GC.stat(:total_allocated_objects) - allocated
      raise 'timed result row count changed' unless rows == expected
      { elapsed_ns: elapsed, rows: rows, allocations: allocations, gc_count: GC.stat(:count) - gc }
    end
    { samples_ns: samples.map { |sample| sample[:elapsed_ns] }, rows_per_sample: samples.map { |sample| sample[:rows] },
      allocations_per_sample: samples.map { |sample| sample[:allocations] }, gc_counts: samples.map { |sample| sample[:gc_count] },
      median_ns: samples.map { |sample| sample[:elapsed_ns] }.sort[repetitions / 2].fdiv(count) }
  end

  def run_fixture(path, directory, plan, options)
    expected, tasks, sessions = {}, [], []
    budgets = options[:work_mem] ? [options[:work_mem]] : plan.fetch('work_mem_bytes')
    apis = options[:api] == 'all' ? APIS : [options[:api]]
    JedNative.open(path, read_only: true, locking: :exclusive) do |database|
      budgets.each do |budget|
        database.work_mem = budget
        database.max_query_memory_bytes = 0
        apis.each do |api|
          current = database.session
          sessions << current
          plan.fetch('workloads').each do |job|
            run = runner(current, job, api)
            answers = job.fetch('parameters').map { |params| collect(run, params) }
            name = job.fetch('name')
            raise "#{name} values/types/cost differ across APIs or work_mem" if expected.key?(name) && expected[name] != answers
            expected[name] ||= answers
            entry = { name: name, sql: job.fetch('sql'), api: api, work_mem: budget,
              parameters: job.fetch('parameters'), iterations: job.fetch('iterations'), warmup: job.fetch('warmup'),
              validation_sha256: sha(answers), validation_values_sha256: sha(answers.map { |answer| answer[:rows] }),
              validation_rows: answers.map { |answer| answer[:rows].length }, validation_costs: answers.map { |answer| answer[:cost] },
              validation_columns: answers.first[:columns], validation_column_types: answers.first[:column_types],
              samples_ns: [], rows_per_sample: [], allocations_per_sample: [], median_ns: nil }
            tasks << { run: run, job: job, entry: entry }
          end
        end
      end
      # Finish every sample before enabling TracePoint even once. Disabling a
      # trace does not itself promise restoration of the VM's prior JIT state.
      unless options[:verify_only]
        tasks.each do |task|
          entry = task.fetch(:entry)
          entry.merge!(measure(task.fetch(:run), task.fetch(:job), plan.fetch('repetitions'), entry[:validation_rows]))
        end
      end
      tasks.each do |task|
        run, job, entry = task.values_at(:run, :job, :entry)
        name, budget = entry.values_at(:name, :work_mem)
        repeated, spill = scratch_probe(directory) { collect(run, job.fetch('parameters').first) }
        raise "#{name} probe changed values/types/cost" unless repeated == expected.fetch(name).first
        if budget == plan.fetch('work_mem_bytes').min && job['low_work_mem_must_spill']
          raise "#{name} did not write/read actual scratch under low work_mem" unless spill[:write_bytes].positive? && spill[:read_bytes].positive?
        elsif budget == plan.fetch('work_mem_bytes').max
          raise "#{name} unexpectedly spilled under high work_mem" unless spill[:created].zero? && spill[:write_bytes].zero?
        end
        raise "#{name} did not use the hash-join row structure" if job['must_use_hash_rows'] && spill[:operators]['hash_rows_push'].zero?
        entry[:spill] = spill
      end
    ensure
      sessions.each(&:close)
    end
    tasks.map { |task| task.fetch(:entry) }
  end

  def main
    previous_tmpdir = ENV['TMPDIR']
    options = { api: 'all', verify_only: false }
    OptionParser.new do |parser|
      parser.on('--report PATH') { |value| options[:report] = File.expand_path(value) }
      parser.on('--verify-only') { options[:verify_only] = true }
      parser.on('--api NAME', APIS + ['all']) { |value| options[:api] = value }
      parser.on('--work-mem BYTES', Integer) { |value| options[:work_mem] = value }
      parser.on('--jit NAME', %w[none yjit zjit]) { |value| options[:jit] = value }
    end.parse!
    abort 'Specify --report PATH --jit none|yjit|zjit under ruby-native:bounded' unless ARGV.empty? && options[:report] && options[:jit]
    abort '--work-mem must be positive' if options[:work_mem] && !options[:work_mem].positive?
    abort 'report already exists; choose a fresh path' if File.exist?(options[:report])
    address_space = Process.getrlimit(:AS).first
    abort 'run under a bounded worker with at most 2 GiB address space' unless address_space.positive? && address_space <= ADDRESS_SPACE_BYTES
    source = File.expand_path(ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native')))
    source_hash = NativeRubySource.digest(source)
    require File.join(source, 'lib/jed_native')
    jit = defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled? ? 'yjit' : defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled? ? 'zjit' : 'none'
    raise 'wrong JIT runtime' unless jit == options[:jit]
    plan = JSON.parse(File.read(PLAN))
    raise 'unexpected checkpoint schedule' unless plan.fetch('repetitions') == 5 && plan.fetch('rows') == 4096 && plan.fetch('join_rows') == 1024
    parent = File.join(ROOT, '.dev/ruby-native-blocking-workspaces')
    FileUtils.mkdir_p(parent)
    report = nil
    Dir.mktmpdir('checkpoint-', parent) do |directory|
      ENV['TMPDIR'] = directory
      location = filesystem(directory)
      path = File.join(directory, 'blocking.jed')
      setup(path, plan)
      file_hash = Digest::SHA256.file(path).hexdigest
      report = { schema: 1, mode: 'file_blocking_checkpoint', created_at: Time.now.utc.iso8601,
        source: source, source_sha256: source_hash, ruby: RUBY_DESCRIPTION, executable: RbConfig.ruby,
        executable_sha256: Digest::SHA256.file(RbConfig.ruby).hexdigest, jit: jit, options: options,
        script_sha256: Digest::SHA256.file(__FILE__).hexdigest, plan_path: PLAN, plan_sha256: Digest::SHA256.file(PLAN).hexdigest,
        helpers_sha256: { 'source.rb' => Digest::SHA256.file(File.join(__dir__, 'source.rb')).hexdigest },
        fixture: { path: path, file_sha256: file_hash, file_bytes: File.size(path), rows: plan.fetch('rows'), join_rows: plan.fetch('join_rows'),
          filesystem: location, locking: 'exclusive', read_only_during_measurement: true, scratch_directory: directory },
        limits: { address_space_bytes: address_space, required_supervisor_rss_bytes: MEMORY_BYTES,
          supervisor_timeout_seconds: ENV['JED_NATIVE_TIMEOUT'], max_query_memory_bytes: 0 }, repetitions: plan.fetch('repetitions'),
        notes: ['Small private file and scratch directory on the recorded disk filesystem; fixture construction is outside timing.',
          'Validation compares exact values, names, types and costs across requested APIs/work_mem budgets. Cross-source comparisons use the retained full-answer digests.',
          'Scratch-file IO byte counts and hash-row calls use TracePoint only after every timed sample has finished; no wrappers or tracing run during samples.',
          'All query outputs are fully drained; GC remains enabled. Timings include result iteration. Prepared parsing is outside timing.',
          'The external bounded-process report supplies observed RSS and enforced timeout; this worker also verifies its address-space limit.'] }
      report[:workloads] = run_fixture(path, directory, plan, options)
      raise 'read-only fixture changed' unless Digest::SHA256.file(path).hexdigest == file_hash
      report[:final_source_sha256] = NativeRubySource.digest(source)
      raise 'library changed during checkpoint' unless report[:final_source_sha256] == source_hash
    end
    report[:fixture][:removed_after_run] = !File.exist?(report[:fixture][:path]) && !File.exist?(report[:fixture][:scratch_directory])
    raise 'fixture cleanup failed' unless report[:fixture][:removed_after_run]
    FileUtils.mkdir_p(File.dirname(options[:report]))
    File.write(options[:report], JSON.pretty_generate(report) + "\n")
    puts JSON.generate(report: options[:report], jit: jit, source_sha256: source_hash, workloads: report[:workloads].length)
  ensure
    previous_tmpdir ? ENV['TMPDIR'] = previous_tmpdir : ENV.delete('TMPDIR')
  end
end

NativeBlockingDiagnostics.main if $PROGRAM_NAME == __FILE__
