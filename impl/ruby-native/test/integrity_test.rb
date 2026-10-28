# frozen_string_literal: true

# The integrity check (spec/design/integrity-check.md). The damaged fixtures pin the cross-core
# report (spec/fileformat/integrity/*.json); every clean golden checks clean; the rest is host-API
# surface the fixtures cannot express: 42704, truncation, in-memory and attached databases, a check
# beside an open writer, and the live-only kinds that need damage done after open.
require 'json'
require 'tmpdir'
require 'fileutils'
require_relative '../lib/jed_native'

module RubyIntegrityTest
  ROOT = File.expand_path('../../../spec', __dir__)
  FIXTURES = File.join(ROOT, 'fileformat', 'integrity')
  GOLDENS = File.join(ROOT, 'fileformat', 'fixtures')

  def self.assert(value, message = 'assertion failed')
    raise message unless value
  end

  def self.error(code)
    yield
  rescue JedNative::Error => exception
    assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}: #{exception.message}")
  else
    raise "expected #{code}"
  end

  def self.contract(report)
    { 'findings' => report.findings.map do |f|
        { 'kind' => f.kind, 'database' => f.database, 'object' => f.object, 'page' => f.page,
          'key' => f.key&.unpack1('H*') }
      end,
      'truncated' => report.truncated,
      'skipped' => report.skipped.map { |s| { 'object' => s.object, 'reason' => s.reason } },
      'pages' => report.pages, 'rows' => report.rows }
  end

  def self.golden_environment
    environment = JedNative::Environment.new
                                         .load_unicode_data(File.binread(File.join(ROOT, 'collation', 'fixtures', 'unicode.jucd')))
                                         .load_time_zone_data(File.binread(File.join(ROOT, 'tz', 'fixtures', 'tzdata.jtz')))
    environment.register_function(name: 'geo_hash', args: ['i64'], returns: 'i64', volatility: :immutable,
                                  component_id: 'com.example/geo_hash', semantic_version: 1) { |value| value * 10 }
  end

  def self.with_copy(source, dir, **options)
    path = File.join(dir, File.basename(source))
    FileUtils.cp(source, path)
    database = JedNative.open(path, read_only: true, **options)
    yield database
  ensure
    database&.close
    FileUtils.rm_f(path)
    FileUtils.rm_rf("#{path}.lock")
  end

  def self.registry
    text = File.read(File.join(ROOT, 'integrity', 'findings.toml'))
    kinds = text.scan(/^kind = "([a-z_]+)"/).flatten
    reasons = text.scan(/^reason = "([a-z_]+)"/).flatten
    assert(kinds.sort == JedNative::INTEGRITY_KINDS.sort, "finding kinds differ from the registry: #{kinds - JedNative::INTEGRITY_KINDS} / #{JedNative::INTEGRITY_KINDS - kinds}")
    assert(reasons.sort == JedNative::INTEGRITY_SKIP_REASONS.sort, 'skip reasons differ from the registry')
  end

  def self.fixtures
    count = 0
    Dir.mktmpdir('jed-ruby-integrity') do |dir|
      Dir[File.join(FIXTURES, '*.json')].sort.each do |spec|
        fixture = JSON.parse(File.read(spec))
        with_copy(File.join(FIXTURES, fixture['fixture']), dir) do |database|
          fixture['runs'].each do |run|
            report = database.integrity_check('main', level: run['level'], max_findings: run['max_findings'],
                                                      constraints: run.fetch('constraints', false))
            label = "#{fixture['fixture']} #{run['level']} #{run['max_findings']} #{run.fetch('constraints', false)}"
            actual = contract(report)
            assert(actual == run['report'], "#{label}:\nexpected #{JSON.generate(run['report'])}\n     got #{JSON.generate(actual)}")
            assert(report.ok? == (run['report']['findings'].empty? && !run['report']['truncated']), label)
            count += 1
          end
        end
      end
    end
    assert(count.positive?)
    count
  end

  NOT_CLEAN = ->(name) { name.start_with?('malformed_') || %w[cow_invalid_free_list.jed collation_skew_corrupt.jed].include?(name) }

  def self.clean_goldens
    environment = golden_environment
    checked = 0
    unopened = []
    Dir.mktmpdir('jed-ruby-integrity-goldens') do |dir|
      Dir[File.join(GOLDENS, '*.jed')].sort.each do |path|
        name = File.basename(path)
        next if NOT_CLEAN.call(name)

        begin
          credential = name.start_with?('encrypted_') ? JedNative::Credential.key("\x4A".b * 32) : nil
          with_copy(path, dir, environment: environment, credential: credential) do |database|
            %i[quick full].each do |level|
              report = database.integrity_check(level: level, max_findings: 0)
              assert(report.findings.empty?, "#{name} #{level}: #{contract(report)['findings']}")
              assert(report.skipped.empty?, "#{name} #{level} skipped #{contract(report)['skipped']}") unless name == 'unparsable_definitions.jed' && level == :full
            end
            report = database.integrity_check(max_findings: 0, constraints: true)
            assert(report.findings.empty?, "#{name} constraints: #{contract(report)['findings']}")
            next if name == 'unparsable_definitions.jed'

            assert(report.skipped.empty?, "#{name} constraints skipped #{contract(report)['skipped']}")
          end
        rescue JedNative::Error => e
          # Files this experimental core does not open (pinned time-zone index data).
          raise unless e.sqlstate == 'XX002'

          unopened << name
          next
        end
        checked += 1
      end
    end
    assert(checked > 80, "only #{checked} goldens checked")
    [checked, unopened]
  end

  # The definition-level skips (§3.5) need this binary's parser and an environment, which the
  # reference lacks, so no shared fixture expresses them.
  def self.definition_skips
    Dir.mktmpdir('jed-ruby-integrity-skips') do |dir|
      skips = ->(report) { report.skipped.map { |s| [s.object, s.reason] } }
      with_copy(File.join(GOLDENS, 'unparsable_definitions.jed'), dir, environment: golden_environment) do |database|
        assert(database.integrity_check(level: :quick).skipped.empty?)
        full = database.integrity_check
        assert(full.findings.empty?)
        expected = [%w[e_r_excl unparsable_definition], %w[p_future_idx unparsable_definition], %w[x_future_idx unparsable_definition]]
        assert(skips.call(full) == expected, "unparsable full: #{skips.call(full)}")
        constrained = database.integrity_check(constraints: true)
        assert(skips.call(constrained) == expected + [%w[c_v_check unparsable_definition]], "unparsable constraints: #{skips.call(constrained)}")
      end
      geo = lambda do |version|
        JedNative::Environment.new.register_function(name: 'geo_hash', args: ['i64'], returns: 'i64', volatility: :immutable,
                                                     component_id: 'com.example/geo_hash', semantic_version: version) { |value| value }
      end
      [JedNative::Environment.new, geo.call(2)].each do |environment|
        with_copy(File.join(GOLDENS, 'hostfunc_index_table.jed'), dir, environment: environment) do |database|
          report = database.integrity_check
          assert(report.findings.empty? && skips.call(report) == [%w[t_geo_idx missing_host_function]], "hostfunc: #{skips.call(report)}")
        end
      end
      with_copy(File.join(GOLDENS, 'collation_skew_corrupt.jed'), dir, environment: golden_environment) do |database|
        assert(database.integrity_check(level: :quick).ok?)
        full = database.integrity_check
        assert(full.findings.empty? && skips.call(full) == [%w[t_name_idx missing_loaded_data]], "skew: #{contract(full)}")
      end
    end
  end

  def self.host_surface
    Dir.mktmpdir('jed-ruby-integrity-host') do |dir|
      path = File.join(dir, 'host.jed')
      database = JedNative.create(path, page_size: 512)
      database.execute('CREATE TABLE t (id i32 PRIMARY KEY, v text, n i32)')
      database.execute('CREATE INDEX t_v ON t (v)')
      database.execute('CREATE INDEX t_n2 ON t ((n * 2)) WHERE n > 3')
      database.execute_script((1..60).map { |i| "INSERT INTO t VALUES (#{i}, 'value #{i}', #{i % 7});" }.join)
      error('42704') { database.integrity_check('nope') }
      %i[quick full].each { |level| assert(database.integrity_check(level: level).ok?, "fresh file #{level}") }
      assert(database.integrity_check(constraints: true).ok?)

      # A check beside an open writer sees the committed generation and does not wait.
      writer = database.write_session
      writer.execute("INSERT INTO t VALUES (100, 'pending', 1)")
      report = database.integrity_check
      assert(report.ok? && report.rows == 60, "beside a writer: #{contract(report)}")
      writer.commit
      writer.close
      assert(database.integrity_check.rows == 61)

      # In-memory attachment and in-memory main.
      database.attach('mem')
      database.execute('CREATE TABLE mem.m (id i32 PRIMARY KEY, a i32[])')
      database.execute('CREATE INDEX m_a ON mem.m USING gin (a)')
      database.execute_script((1..30).map { |i| "INSERT INTO mem.m VALUES (#{i}, ARRAY[#{i}, #{i + 1}]);" }.join)
      report = database.integrity_check('mem')
      assert(report.ok? && report.rows == 30 && report.findings.all? { |f| f.database == 'mem' }, "memory attachment: #{contract(report)}")
      database.close

      JedNative.memory do |memory|
        assert(memory.integrity_check.ok?, 'empty memory database')
        memory.execute('CREATE TABLE r (id i64 PRIMARY KEY, s text UNIQUE)')
        memory.execute_script((1..50).map { |i| "INSERT INTO r VALUES (#{i}, '#{'x' * 40}#{i}');" }.join)
        report = memory.integrity_check(constraints: true)
        assert(report.ok? && report.rows == 50, "memory main: #{contract(report)}")
      end

      # max_findings truncation, and live damage after open: a flipped leaf byte, then a damaged meta.
      database = JedNative.open(path)
      storage = database.engine.instance_variable_get(:@storage)
      meta = storage.meta
      leaf = (2...meta[:page_count]).find do |page|
        bytes = File.binread(path, 512, page * 512)
        bytes.getbyte(0) == JedNative::FileFormat::PAGE_LEAF && !(meta[:protected] || []).include?(page)
      end
      File.open(path, 'r+b') do |file|
        byte = file.pread(1, leaf * 512 + 40).getbyte(0)
        file.pwrite([byte ^ 0xFF].pack('C'), leaf * 512 + 40)
      end
      storage.instance_variable_get(:@image).invalidate([leaf])
      report = database.integrity_check
      assert(report.findings.map(&:kind).include?('page_corrupt') && report.findings.any? { |f| f.page == leaf }, "live leaf damage: #{contract(report)}")
      one = database.integrity_check(max_findings: 1)
      assert(one.truncated == (report.findings.length > 1) && one.findings.length == 1)
      File.open(path, 'r+b') do |file|
        slot = meta[:slot]
        byte = file.pread(1, slot * 512 + 20).getbyte(0)
        file.pwrite([byte ^ 1].pack('C'), slot * 512 + 20)
      end
      storage.instance_variable_get(:@image).invalidate([meta[:slot]])
      report = database.integrity_check
      assert(report.findings.first&.kind == 'meta_invalid' && report.findings.first.page == meta[:slot], "live meta damage: #{contract(report)}")
      database.close
    end
  end

  def self.run
    registry
    puts 'ruby-native integrity: registry passed'
    runs = fixtures
    puts "ruby-native integrity: #{runs} fixture runs passed"
    checked, unopened = clean_goldens
    puts "ruby-native integrity: #{checked} clean goldens passed#{unopened.empty? ? '' : " (not opened: #{unopened.join(', ')})"}"
    definition_skips
    puts 'ruby-native integrity: definition skips passed'
    host_surface
    puts 'ruby-native integrity: host surface passed'
  end
end

RubyIntegrityTest.run
