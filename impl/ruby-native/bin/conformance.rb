#!/usr/bin/env ruby
# frozen_string_literal: true

require 'json'
require 'digest'
require 'tmpdir'
require 'fileutils'
require 'timeout'
require 'time'
require File.join(ENV.fetch('RUBY_NATIVE_SOURCE', File.expand_path('..', __dir__)), 'lib/jed_native')
require_relative '../../../spec/conformance/params'

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
  SUPPORTED = %w[requires names types cost max_cost params max_scalar_bytes max_sql_length lifetime_max_cost clock clock_advance seed attach timezone load-timezone load-collation collation unicode allow_ddl allow_temp_ddl default_privileges grant revoke max_query_memory_bytes fixture upgrade-collations work_mem max_storage_bytes max_temp_storage_bytes].freeze
  DIRECTIVES = %w[requires names types cost max_cost format fixture params clock clock_advance seed timezone collation unicode attach lifetime_max_cost max_query_memory_bytes max_scalar_bytes max_sql_length max_storage_bytes max_temp_storage_bytes work_mem allow_ddl allow_temp_ddl default_privileges grant revoke set load-collation load-timezone upgrade-collations].freeze
  attr_reader :summary

  def initialize(filter: '', mode: 'memory', output: nil)
    @filter, @mode, @output = filter, mode, output
    @summary = { files: 0, records: 0, passed: 0, failed: 0, unsupported_files: 0, skipped_files: 0, timed_out_files: 0, failures: [] }
  end

  def run
    corpus = ENV.fetch('JED_NATIVE_CORPUS_ROOT', File.join(ROOT, 'spec/conformance/suites'))
    paths = Dir[File.join(corpus, '**/*.test')].sort.select { |p| p.include?(@filter) }
    abort "no corpus paths match #{@filter.inspect}" if paths.empty?
    abort "unknown storage mode #{@mode.inspect}" unless %w[memory disk].include?(@mode)
    @summary[:mode], @summary[:filter], @summary[:started_at] = @mode, @filter, Time.now.utc.iso8601
    @summary[:corpus_root] = corpus
    @summary[:available_records] = paths.sum { |path| File.read(path).scan(/^(?:statement|query) /).length }
    @summary[:source_sha256] = source_digest
    paths.each do |path|
      begin
        Timeout.timeout(10) { run_file(path) }
      rescue Timeout::Error
        @summary[:timed_out_files] += 1
        fail_record(path, 0, 'harness timeout: file exceeded 10 seconds')
      rescue SystemStackError => error
        fail_record(path, 0, "unbounded recursion: #{error.message}")
      end
    end
    @summary[:finished_at], @summary[:final_source_sha256] = Time.now.utc.iso8601, source_digest
    File.write(@output, JSON.pretty_generate(@summary) + "\n") if @output
    puts JSON.generate(@summary.reject { |key, _| key == :failures })
    @summary[:failed].zero? && @summary[:unsupported_files].zero?
  end

  def run_file(path)
    @summary[:files] += 1
    contents = File.read(path)
    if contents.match?(/^#\s*skip:\s*#{@mode}\b/)
      @summary[:skipped_files] += 1
      return
    end
    directives = (contents.scan(/^#\s*([a-z_-]+):/).flatten.uniq & DIRECTIVES) - SUPPORTED
    unless directives.empty?
      @summary[:unsupported_files] += 1
      @summary[:failures] << { file: relative(path), error: "unsupported harness directives: #{directives.join(', ')}" }
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
          db = JedNative.open(file, environment: environment)
        else
          db = JedNative.from_image(bytes, environment: environment)
        end
      rescue JedNative::Error => error
        raise unless expected_error && error.sqlstate == expected_error.strip
        return
      end
      raise "expected fixture open error #{expected_error}, got success" if expected_error
    else
      db = file ? JedNative.create(file, environment: environment) : JedNative.memory(environment: environment)
    end
    contents.scan(/^#\s*attach:\s*(.+)$/).flatten.each { |name| db.attach(name.strip) }
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
      if (match = /^#\s*(names|types|cost|max_cost|params|work_mem|max_temp_storage_bytes|max_query_memory_bytes|max_scalar_bytes|max_sql_length|lifetime_max_cost|clock|clock_advance|seed|timezone|allow_ddl|allow_temp_ddl|default_privileges|grant|revoke):\s*(.*)$/.match(line))
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
      begin
        if pending['clock_advance']
          clock.reset(*pending['clock_advance'].split(',').map { |part| Integer(part) })
        else
          clock.reset(pending['clock'] && Integer(pending['clock']))
        end
        random.reset(pending['seed'] && Integer(pending['seed']))
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
        result = begin
          db.query(sql.join("\n"), *params)
        rescue JedNative::Error => e
          error = e
          nil
        end
        if line.start_with?('statement error')
          want = line.split[2]
          raise "expected #{want}, got #{error ? error.sqlstate : 'success'}" unless error && error.sqlstate == want
        else
          raise error if error
          if line.start_with?('query')
            tags, sort = line.split[1, 2]
            tags *= result.columns.length if tags.length == 1
            raise "column count #{result.columns.length}, expected #{tags.length}" unless result.columns.length == tags.length
            actual = result.rows.map { |row| row.each_with_index.map { |v, index| db.render(v, result.column_types[index]) } }
            actual.sort! if sort == 'rowsort'
            actual = actual.flatten
            actual.sort! if sort == 'valuesort'
            if expected.length == 1 && (hash = /\A(\d+) values hashing to ([0-9a-f]+)\z/.match(expected.first))
              raise 'hash mismatch' unless actual.length == hash[1].to_i && Digest::MD5.hexdigest(actual.join("\n") + "\n") == hash[2]
            else
              expected = expected.each_slice(tags.length).to_a.sort.flatten if sort == 'rowsort'
              expected.sort! if sort == 'valuesort'
              equal = actual.length == expected.length && actual.each_index.all? { |n| cell_equal?(actual[n], expected[n], tags[n % tags.length]) }
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
        raise if error.is_a?(Timeout::Error)
        fail_record(path, start, "#{error.class}: #{error.message}", sql.join("\n"))
        warn "#{relative(path)}:#{start}: #{error.message}" if ENV['JED_NATIVE_VERBOSE'] == '1'
      ensure
        pending = {}
        if file && !db.engine.transaction_open?
          db.close
          db = JedNative.open(file, environment: environment)
          storage_limits.each { |name, bytes| db.set_max_storage_bytes(bytes, name) }
        end
      end
    end
  rescue StandardError => error
    raise if error.is_a?(Timeout::Error)
    fail_record(path, 0, "file failure #{error.class}: #{error.message}")
  ensure
    db&.close
    FileUtils.remove_entry(directory) if directory
  end

  def relative(path) = path.delete_prefix(ROOT + '/')
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
  def fail_record(path, line, error, sql = nil)
    @summary[:failed] += 1
    @summary[:failures] << { file: relative(path), line: line, error: error, sql: sql }
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

if $PROGRAM_NAME == __FILE__
  filter = ARGV[0] || ENV.fetch('JED_CONFORMANCE_FILTER', '')
  mode = ENV.fetch('JED_NATIVE_STORAGE', 'memory')
  exit(NativeCorpus.new(filter: filter, mode: mode, output: ENV['JED_NATIVE_REPORT']).run ? 0 : 1)
end
