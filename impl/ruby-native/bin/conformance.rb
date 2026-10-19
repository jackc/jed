#!/usr/bin/env ruby
# frozen_string_literal: true

require 'json'
require 'digest'
require 'tmpdir'
require 'fileutils'
require 'time'
require 'rbconfig'
require_relative '../tools/bounded_process'
require File.join(ENV.fetch('RUBY_NATIVE_SOURCE', File.expand_path('..', __dir__)), 'lib/jed_native')
require_relative '../../../spec/conformance/params'

# Force admission in an explicit audit lane: ordinary corpus statements often
# run only once, before the production second-use compilation threshold.
module NativeCorpusCompilation
  MODE = ENV.fetch('JED_NATIVE_COMPILATION', 'auto')
  raise 'JED_NATIVE_COMPILATION must be auto, immediate, or disabled' unless %w[auto immediate disabled].include?(MODE)
  class << self
    attr_accessor :kernels, :pipelines, :orderings, :aggregates, :relations, :text_kernels

    def record_text(kernel)
      self.text_kernels += 1 if kernel && kernel.source.include?('# C text specialization')
    end
  end
  self.kernels = 0
  self.pipelines = 0
  self.orderings = 0
  self.aggregates = 0
  self.relations = 0
  self.text_kernels = 0
  module Engines
    def initialize(**options)
      super
      @native_compilation_immediate = NativeCorpusCompilation::MODE == 'immediate'
      @native_compilation_disabled = NativeCorpusCompilation::MODE == 'disabled'
    end
  end
  module Compiler
    def materialize
      result = super
      NativeCorpusCompilation.kernels += 1 if result
      NativeCorpusCompilation.record_text(result)
      result
    end
  end
  module PipelineCompiler
    def materialize
      result = super
      if result
        NativeCorpusCompilation.kernels += 1
        NativeCorpusCompilation.pipelines += 1
        NativeCorpusCompilation.record_text(result)
      end
      result
    end
  end
  module OrderingCompiler
    def materialize
      result = super
      if result
        NativeCorpusCompilation.kernels += 1
        NativeCorpusCompilation.orderings += 1
        NativeCorpusCompilation.record_text(result)
      end
      result
    end
  end
  module AggregateCompiler
    def materialize
      result = super
      if result
        NativeCorpusCompilation.kernels += 1
        NativeCorpusCompilation.aggregates += 1
        NativeCorpusCompilation.record_text(result)
      end
      result
    end
  end
  module RelationCompiler
    def materialize
      result = super
      if result
        NativeCorpusCompilation.kernels += 1
        NativeCorpusCompilation.relations += 1
        NativeCorpusCompilation.record_text(result)
      end
      result
    end
  end
end
JedNative::Engine.prepend(NativeCorpusCompilation::Engines)
if defined?(JedNative::CompiledExpression::Program)
  JedNative::CompiledExpression::Program.prepend(NativeCorpusCompilation::Compiler)
elsif NativeCorpusCompilation::MODE == 'immediate'
  raise 'selected Ruby source has no runtime compiler'
end
JedNative::CompiledPipeline::Program.prepend(NativeCorpusCompilation::PipelineCompiler) if defined?(JedNative::CompiledPipeline::Program)
JedNative::CompiledOrdering::Program.prepend(NativeCorpusCompilation::OrderingCompiler) if defined?(JedNative::CompiledOrdering::Program)
JedNative::CompiledAggregate::Program.prepend(NativeCorpusCompilation::AggregateCompiler) if defined?(JedNative::CompiledAggregate::Program)
JedNative::CompiledRelation::Program.prepend(NativeCorpusCompilation::RelationCompiler) if defined?(JedNative::CompiledRelation::Program)

# The environment keeps these source identities for its entire lifetime. Only
# the harness changes their controlled state between corpus records.
class NativeCorpusClock
  def reset(start = nil, step = 0)
    @source = start ? JedNative::AdvancingClock.new(start, step) : JedNative::WallClock.new
  end
  def now_micros = @source.now_micros
end

class NativeCorpusRandom
  def reset(seed = nil) = (@source = seed ? JedNative::SeededRandom.new(seed) : JedNative::OSRandom.new)
  def bytes(length) = @source.bytes(length)
end

# This audit walks the authored corpus directly and fails for unsupported harness
# directives as well as SQL mismatches. No capability filter hides missing work.
class NativeCorpus
  ROOT = File.expand_path('../../..', __dir__)
  DIAGNOSTIC_BYTES = 64 * 1024
  FAILURE_DETAIL_BYTES = 4 * 1024 * 1024
  SUPPORTED = %w[format set requires names types cost max_cost params max_scalar_bytes max_sql_length lifetime_max_cost clock clock_advance seed attach timezone load-timezone load-collation collation unicode allow_ddl allow_temp_ddl default_privileges grant revoke max_query_memory_bytes fixture upgrade-collations work_mem max_storage_bytes max_temp_storage_bytes].freeze
  DIRECTIVES = %w[requires names types cost max_cost format fixture params clock clock_advance seed timezone collation unicode attach lifetime_max_cost max_query_memory_bytes max_scalar_bytes max_sql_length max_storage_bytes max_temp_storage_bytes work_mem allow_ddl allow_temp_ddl default_privileges grant revoke set load-collation load-timezone upgrade-collations].freeze
  attr_reader :summary

  def initialize(filter: '', mode: 'memory', output: nil)
    @filter, @mode, @output = filter, mode, output
    @query_api = ENV.fetch('JED_NATIVE_QUERY_API', 'execute')
    raise 'JED_NATIVE_QUERY_API must be execute or query' unless %w[execute query].include?(@query_api)
    @query_memory_api = ENV.fetch('JED_NATIVE_QUERY_MEMORY_API', @query_api)
    raise 'JED_NATIVE_QUERY_MEMORY_API must be execute or query' unless %w[execute query].include?(@query_memory_api)
    @failure_detail_bytes = 0
    @summary = { files: 0, records: 0, passed: 0, failed: 0, unsupported_files: 0, skipped_files: 0, timed_out_files: 0, memory_limited_files: 0, omitted_failure_details: 0, failures: [] }
    @summary[:scalar_cursor_records] = 0 if @query_api == 'query'
    @summary[:compiled_kernels] = 0
    @summary[:compiled_pipelines] = 0
    @summary[:compiled_orderings] = 0
    @summary[:compiled_aggregates] = 0
    @summary[:compiled_relations] = 0
    @summary[:compiled_text_kernels] = 0
    @summary[:query_api_records] = @summary[:execute_api_records] = 0
    @summary[:materialized_memory_records] = 0
  end

  def run
    corpus = ENV.fetch('JED_NATIVE_CORPUS_ROOT', File.join(ROOT, 'spec/conformance/suites'))
    paths = Dir[File.join(corpus, '**/*.test')].sort.select { |p| p.include?(@filter) }
    abort "no corpus paths match #{@filter.inspect}" if paths.empty?
    abort "unknown storage mode #{@mode.inspect}" unless %w[memory disk].include?(@mode)
    locking = ENV.fetch('JED_NATIVE_LOCKING', 'auto')
    abort "unknown corpus locking mode #{locking.inspect}" unless %w[auto exclusive].include?(locking)
    @summary[:mode], @summary[:filter], @summary[:started_at] = @mode, @filter, Time.now.utc.iso8601
    @summary[:query_api] = @query_api
    @summary[:query_memory_api] = @query_memory_api
    @summary[:compilation] = NativeCorpusCompilation::MODE
    @summary[:ruby], @summary[:ruby_executable] = RUBY_DESCRIPTION, RbConfig.ruby
    @summary[:jit] = if defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled? then 'yjit'
                    elsif defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled? then 'zjit'
                    else 'interpreter'
                    end
    @summary[:locking] = locking if @mode == 'disk'
    @summary[:corpus_root] = corpus
    @summary[:available_records] = paths.sum { |path| File.read(path).scan(/^(?:statement|query) /).length }
    @summary[:source_sha256] = source_digest
    file_timeout = Float(ENV.fetch('JED_NATIVE_FILE_TIMEOUT', '10'))
    memory_bytes = Integer(ENV.fetch('JED_NATIVE_MEMORY_BYTES', JedNativeTools::BoundedProcess::DEFAULT_MEMORY_BYTES.to_s))
    address_space_bytes = Integer(ENV.fetch('JED_NATIVE_ADDRESS_SPACE_BYTES', ENV.fetch('RUBY_NATIVE_ADDRESS_SPACE_BYTES', (memory_bytes * 2).to_s)))
    address_space_bytes = [address_space_bytes, Process.getrlimit(Process::RLIMIT_AS).last].min
    @summary[:worker_memory_bytes], @summary[:worker_timeout_seconds] = memory_bytes, file_timeout
    @summary[:worker_address_space_bytes] = address_space_bytes
    @summary[:peak_worker_rss_bytes] = 0
    paths.each do |path|
      result = JedNativeTools::BoundedProcess.run(timeout: file_timeout, memory_bytes: memory_bytes, address_space_bytes: address_space_bytes) do
        worker = self.class.new(filter: @filter, mode: @mode)
        worker.run_file(path)
        worker.summary[:compiled_kernels] = NativeCorpusCompilation.kernels
        worker.summary[:compiled_pipelines] = NativeCorpusCompilation.pipelines
        worker.summary[:compiled_orderings] = NativeCorpusCompilation.orderings
        worker.summary[:compiled_aggregates] = NativeCorpusCompilation.aggregates
        worker.summary[:compiled_relations] = NativeCorpusCompilation.relations
        worker.summary[:compiled_text_kernels] = NativeCorpusCompilation.text_kernels
        worker.summary
      end
      @summary[:peak_worker_rss_bytes] = [@summary[:peak_worker_rss_bytes], result.peak_rss_bytes].max
      if result.success? && result.value
        merge_worker_summary(result.value)
      else
        active = result.breadcrumb || {}
        merge_worker_summary(active['summary'] || { 'files' => 1 })
        @summary[:timed_out_files] += 1 if result.timed_out
        @summary[:memory_limited_files] += 1 if result.memory_exceeded
        fail_record(path, active['line'] || 0, "harness #{result.reason}: #{result.stderr.to_s.lines.first.to_s.strip}", active['sql'], sql_original_bytes: active['sql_original_bytes'])
      end
      write_report
      if result.interrupted
        @summary[:interrupted] = result.interrupted
        break
      end
    end
    @summary[:finished_at], @summary[:final_source_sha256] = Time.now.utc.iso8601, source_digest
    write_report
    puts JSON.generate(@summary.reject { |key, _| key == :failures })
    @summary[:failed].zero? && @summary[:unsupported_files].zero?
  end

  def run_file(path)
    @summary[:files] += 1
    worker_progress(path, 0, nil)
    contents = File.read(path)
    if contents.match?(/^#\s*skip:\s*#{@mode}\b/)
      @summary[:skipped_files] += 1
      return
    end
    return run_concurrency_file(path, contents) if contents.match?(/^#\s*format:\s*concurrency/)
    directives = (contents.scan(/^#\s*([a-z_-]+):/).flatten.uniq & DIRECTIVES) - SUPPORTED
    unless directives.empty?
      @summary[:unsupported_files] += 1
      append_failure(file: relative(path), error: "unsupported harness directives: #{directives.join(', ')}")
      return
    end
    directory = @mode == 'disk' ? Dir.mktmpdir('jed-ruby-native') : nil
    file = directory && File.join(directory, 'database.jed')
    clock, random = NativeCorpusClock.new, NativeCorpusRandom.new
    clock.reset
    random.reset
    environment = JedNative::Environment.new(clock: clock, random: random)
    environment = environment.load_time_zone_data(File.binread(File.join(ROOT, 'spec/tz/fixtures/tzdata.jtz'))) if contents.match?(/^#\s*load-timezone:/)
    environment = environment.load_unicode_data(File.binread(File.join(ROOT, 'spec/collation/fixtures/unicode.jucd'))) if contents.match?(/^#\s*load-collation:/)
    fixture = contents[/^#\s*fixture:\s*(.+)$/, 1]
    if fixture
      fixture_path, expected_error = fixture.split(/\s+error\s+/, 2)
      bytes = File.binread(File.join(ROOT, 'spec', fixture_path.strip))
      begin
        if file
          File.binwrite(file, bytes)
          db = JedNative.open(file, environment: environment, locking: ENV.fetch('JED_NATIVE_LOCKING', 'auto').to_sym)
        else
          db = JedNative.from_image(bytes, environment: environment)
        end
      rescue JedNative::Error => error
        raise unless expected_error && error.sqlstate == expected_error.strip
        return
      end
      raise "expected fixture open error #{expected_error}, got success" if expected_error
    else
      db = file ? JedNative.create(file, environment: environment, locking: ENV.fetch('JED_NATIVE_LOCKING', 'auto').to_sym) : JedNative.memory(environment: environment)
    end
    contents.scan(/^#\s*attach:\s*(.+)$/).flatten.each { |name| db.attach(name.strip) }
    core = db
    db = core.session
    pending = {}
    storage_limits = {}
    lines = contents.lines.map(&:chomp)
    i = 0
    while i < lines.length
      line = lines[i]
      i += 1
      if line.match?(/^#\s*upgrade-collations:/)
        db.upgrade_collations
        next
      end
      if (match = /^#\s*max_storage_bytes:\s*(-?\d+)(?:\s+(\S+))?/.match(line))
        name, bytes = match[2] || 'main', match[1].to_i
        storage_limits[name] = bytes
        db.set_max_storage_bytes(bytes, name)
        next
      end
      if (match = /^#\s*(set|names|types|cost|max_cost|params|work_mem|max_temp_storage_bytes|max_query_memory_bytes|max_scalar_bytes|max_sql_length|lifetime_max_cost|clock|clock_advance|seed|timezone|allow_ddl|allow_temp_ddl|default_privileges|grant|revoke):\s*(.*)$/.match(line))
        if %w[grant revoke].include?(match[1])
          (pending[match[1]] ||= []) << match[2]
        else
          pending[match[1]] = match[2]
        end
        next
      end
      next if line.empty? || line.start_with?('#')
      unless line.start_with?('statement ', 'query ')
        fail_record(path, i, "unsupported control record: #{line}")
        next
      end
      start = i
      sql = []
      while i < lines.length && !lines[i].empty? && lines[i] != '----'
        sql << lines[i] unless lines[i].start_with?('#')
        i += 1
      end
      expected = []
      if i < lines.length && lines[i] == '----'
        i += 1
        while i < lines.length && !lines[i].empty?
          expected << lines[i]
          i += 1
        end
      end
      @summary[:records] += 1
      worker_progress(path, start, sql.join("\n"))
      begin
        if pending['clock_advance']
          clock.reset(*pending['clock_advance'].split(',').map { |part| Integer(part) })
        else
          clock.reset(pending['clock'] && Integer(pending['clock']))
        end
        random.reset(pending['seed'] && Integer(pending['seed']))
        db.engine.reset_vars
        pending.fetch('set', '').split(',').each do |entry|
          name, value = entry.split('=', 2)
          raise 'invalid set directive' unless value
          db.set_var(name.strip, value.strip)
        end
        db.timezone = pending.fetch('timezone', 'UTC')
        db.max_cost = pending.fetch('max_cost', '0').to_i
        db.max_sql_length = pending.fetch('max_sql_length', '1048576').to_i
        db.engine.lifetime_max_cost = pending['lifetime_max_cost'].to_i if pending['lifetime_max_cost']
        db.engine.max_scalar_bytes = pending.fetch('max_scalar_bytes', '67108864').to_i
        db.max_query_memory_bytes = pending.fetch('max_query_memory_bytes', '0').to_i
        db.max_temp_storage_bytes = pending.fetch('max_temp_storage_bytes', '0').to_i
        db.work_mem = pending.fetch('work_mem', ENV.fetch('JED_NATIVE_WORK_MEM', JedNative::WorkMemory::DEFAULT_WORK_MEM.to_s)).to_i
        db.engine.allow_ddl = pending.fetch('allow_ddl', 'on') == 'on'
        db.engine.allow_temp_ddl = pending.key?('allow_temp_ddl') ? pending['allow_temp_ddl'] == 'on' : nil
        db.engine.default_privileges = pending.fetch('default_privileges', 'ALL')
        db.reset_privileges
        %w[grant revoke].each do |action|
          (pending[action] || []).each do |entry|
            privilege, name = entry.split(/\s+ON\s+/i, 2)
            raise "invalid #{action} directive" unless name
            db.public_send(action, privilege, name)
          end
        end
        params = pending['params'] ? CorpusParams.parse(pending['params']).map { |value| bind_value(value) } : []
        error = nil
        result = nil
        # The authored query-memory thresholds exercise the materialized drive.
        # An explicitly selected mixed audit keeps those exact checks on execute
        # while exercising the lazy API elsewhere. Rows#rows has an additional
        # admitted collector (memory.md §5.1/§5.4), tested at its own host-API
        # thresholds. The default still runs every record through query_api.
        api = @query_api == 'query' && db.max_query_memory_bytes.positive? ? @query_memory_api : @query_api
        @summary[:materialized_memory_records] += 1 if api != @query_api
        @summary[api == 'query' ? :query_api_records : :execute_api_records] += 1
        result = begin
          result = db.public_send(api, sql.join("\n"), *params)
          if api == 'query'
            @summary[:scalar_cursor_records] += 1 if defined?(JedNative::ScalarRows) && result.is_a?(JedNative::ScalarRows)
            # Pull inside the error boundary: query() defers execution errors.
            result.rows
          end
          result
        rescue JedNative::Error => e
          error = e
          nil
        ensure
          result&.close if api == 'query'
        end
        if line.start_with?('statement error')
          want = line.split[2]
          raise "expected #{want}, got #{error ? error.sqlstate : 'success'}" unless error && error.sqlstate == want
        else
          raise error if error
          if line.start_with?('query')
            tags, sort = line.split[1, 2]
            # The shared runners use tags to group flat values for comparison;
            # result metadata is asserted separately by names/types directives.
            width = [tags.length, 1].max
            actual = result.rows.flat_map { |row| row.each_with_index.map { |v, index| db.render(v, result.column_types[index]) } }
            actual = actual.each_slice(width).to_a.sort.flatten if sort == 'rowsort'
            actual.sort! if sort == 'valuesort'
            if expected.length == 1 && (hash = /\A(\d+) values hashing to ([0-9a-f]+)\z/.match(expected.first))
              raise 'hash mismatch' unless actual.length == hash[1].to_i && Digest::MD5.hexdigest(actual.join("\n") + "\n") == hash[2]
            else
              expected = expected.each_slice(width).to_a.sort.flatten if sort == 'rowsort'
              expected.sort! if sort == 'valuesort'
              equal = actual.length == expected.length && actual.each_index.all? { |n| cell_equal?(actual[n], expected[n], tags[n % width]) }
              raise "rows expected #{expected.take(20).inspect}; got #{actual.take(20).inspect}" unless equal
            end
            %w[names types].each do |key|
              next unless pending[key]
              actual_names = key == 'names' ? result.columns : result.column_types
              raise "#{key}: expected #{pending[key]}, got #{actual_names.join(', ')}" unless pending[key].split(',').map(&:strip) == actual_names
            end
          end
          if pending['cost'] && result.cost != pending['cost'].to_i
            raise "cost expected #{pending['cost']}, got #{result.cost}"
          end
        end
        @summary[:passed] += 1
      rescue StandardError => error
        fail_record(path, start, "#{error.class}: #{error.message}", sql.join("\n"))
        warn "#{relative(path)}:#{start}: #{error.message}" if ENV['JED_NATIVE_VERBOSE'] == '1'
      ensure
        pending = {}
        if file && !db.engine.transaction_open?
          db.close
          core.close
          core = JedNative.open(file, environment: environment, locking: ENV.fetch('JED_NATIVE_LOCKING', 'auto').to_sym)
          db = core.session
          storage_limits.each { |name, bytes| db.set_max_storage_bytes(bytes, name) }
        end
      end
    end
  rescue StandardError => error
    fail_record(path, 0, "file failure #{error.class}: #{error.message}")
  ensure
    db&.close
    core&.close
    FileUtils.remove_entry(directory) if directory
  end

  def relative(path) = path.delete_prefix(ROOT + '/')
  def worker_progress(path, line, sql)
    target = ENV['JED_NATIVE_BREADCRUMB']
    return unless target
    temporary = "#{target}.tmp"
    sql, original = truncate_diagnostic(sql)
    File.write(temporary, JSON.generate(file: relative(path), line: line, sql: sql, sql_original_bytes: original, summary: @summary))
    File.rename(temporary, target)
  end
  def merge_worker_summary(value)
    value.each do |key, count|
      name = key.to_sym
      if name == :failures
        count.each { |record| append_failure(record.transform_keys(&:to_sym)) }
      elsif @summary[name].is_a?(Numeric)
        @summary[name] += count
      end
    end
  end
  def write_report
    return unless @output
    temporary = "#{@output}.tmp.#{Process.pid}"
    File.write(temporary, JSON.pretty_generate(@summary) + "\n")
    File.rename(temporary, @output)
  end
  def source_digest
    source = ENV.fetch('RUBY_NATIVE_SOURCE', File.join(ROOT, 'impl/ruby-native'))
    Digest::SHA256.hexdigest(Dir[File.join(source, 'lib/**/*.rb')].sort.map { |path| File.binread(path) }.join)
  end
  def bind_value(value)
    case value.kind
    when :decimal then JedNative::Decimal.parse(value.value)
    when :array then JedNative::ArrayValue.new(value.value.map { |v| bind_value(v) })
    when :row then JedNative::CompositeValue.new(value.value.map { |v| bind_value(v) })
    else value.value
    end
  end
  def fail_record(path, line, error, sql = nil, sql_original_bytes: nil)
    @summary[:failed] += 1
    record = { file: relative(path), line: line, error: error, sql: sql }
    record[:sql_original_bytes] = sql_original_bytes if sql_original_bytes
    append_failure(record)
  end

  def truncate_diagnostic(value)
    return [value, nil] if value.nil? || value.bytesize <= DIAGNOSTIC_BYTES
    bytes = value.bytesize
    suffix = "\n[truncated; original #{bytes} bytes]"
    prefix = value.byteslice(0, DIAGNOSTIC_BYTES - suffix.bytesize).force_encoding(Encoding::UTF_8).scrub('')
    [prefix + suffix, bytes]
  end

  def append_failure(record)
    %i[error sql].each do |key|
      record[key], original = truncate_diagnostic(record[key])
      record["#{key}_original_bytes".to_sym] = original if original
    end
    bytes = JSON.generate(record).bytesize
    if @failure_detail_bytes + bytes > FAILURE_DETAIL_BYTES
      @summary[:omitted_failure_details] += 1
    else
      @failure_detail_bytes += bytes
      @summary[:failures] << record
    end
  end

  def cell_equal?(actual, expected, tag)
    return true if actual == expected
    return false unless tag == 'R'
    a, b = Float(actual), Float(expected)
    (a.nan? && b.nan?) || (a.finite? && b.finite? && (a - b).abs <= 1e-12 * [a.abs, b.abs, 1].max)
  rescue ArgumentError
    false
  end
end

require_relative 'concurrency'

if $PROGRAM_NAME == __FILE__
  filter = ARGV[0] || ENV.fetch('JED_CONFORMANCE_FILTER', '')
  mode = ENV.fetch('JED_NATIVE_STORAGE', 'memory')
  exit(NativeCorpus.new(filter: filter, mode: mode, output: ENV['JED_NATIVE_REPORT']).run ? 0 : 1)
end
