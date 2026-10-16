# frozen_string_literal: true

require_relative "test_helper"

# Tests for environments (spec/design/ruby.md §5a/§5c): Jed::Environment / Jed::EnvironmentBuilder —
# loading bundles per environment, isolation between environments, derivation, and the controllable
# random and clock sources. These cover the GEM's seam — passing bundle bytes and source handles to
# the engine's builder and surfacing a malformed bundle as Jed::Error. The collation/tz/clock
# SEMANTICS are the engine's (tested in the corpus + the Rust core); here we prove the seam wires
# through. Host-function registration is in host_functions_test.rb.
class EnvironmentTest < Minitest::Test
  SPEC = File.expand_path("../../../spec", __dir__)
  JUCD = File.join(SPEC, "collation/fixtures/unicode.jucd")
  JTZ  = File.join(SPEC, "tz/fixtures/tzdata.jtz")

  COLLATION_SQL = %(SELECT ('a' < 'B' COLLATE "unicode") AS u, ('a' < 'B') AS c)
  ZONE_SQL = %(SELECT (TIMESTAMPTZ '2020-06-01 12:00:00+00' AT TIME ZONE 'America/New_York') AS t)

  def test_load_unicode_data_enables_collation
    skip "no JUCD fixture at #{JUCD}" unless File.exist?(JUCD)

    env = Jed::Environment.build { |b| b.load_unicode_data(File.binread(JUCD)) }
    Jed.memory(environment: env) do |db|
      # Under UCA "unicode", 'a' sorts before 'B' (case-insensitive primary weight); under the
      # built-in C collation it does not (byte order: 'B'=0x42 < 'a'=0x61).
      row = db.query(COLLATION_SQL).first
      assert_equal true, row[:u]
      assert_equal false, row[:c]
    end
  end

  def test_load_time_zone_data_enables_named_zones
    skip "no JTZ fixture at #{JTZ}" unless File.exist?(JTZ)

    env = Jed::Environment.build { |b| b.load_time_zone_data(File.binread(JTZ)) }
    Jed.memory(environment: env) do |db|
      # 2020-06-01 12:00 UTC is 08:00 in America/New_York (EDT, UTC-4). AT TIME ZONE on a
      # timestamptz yields the local wall-clock as a (zoneless) timestamp → a UTC Time in the gem.
      assert_equal Time.utc(2020, 6, 1, 8, 0, 0), db.query(ZONE_SQL).first[:t]
      err = assert_raises(Jed::Error) do
        db.query(%(SELECT TIMESTAMPTZ '2020-06-01 12:00:00+00' AT TIME ZONE 'Mars/Phobos'))
      end
      assert_equal "22023", err.sqlstate
    end
  end

  # Loading is per environment, not engine-global: a database opened without the environment (or
  # with another one) does not see what it loaded.
  def test_environments_are_isolated
    skip "no fixtures" unless File.exist?(JUCD) && File.exist?(JTZ)

    loaded = Jed::Environment.build do |b|
      b.load_unicode_data(File.binread(JUCD)).load_time_zone_data(File.binread(JTZ))
    end
    Jed.memory(environment: loaded) { |db| assert_equal true, db.query(COLLATION_SQL).first[:u] }
    [nil, Jed::Environment.build].each do |env|
      Jed.memory(environment: env) do |db|
        refute_equal "00000", assert_raises(Jed::Error) { db.query(COLLATION_SQL) }.sqlstate
        assert_equal "22023", assert_raises(Jed::Error) { db.query(ZONE_SQL) }.sqlstate
      end
    end
  end

  def test_file_backed_open_takes_the_environment
    skip "no JTZ fixture at #{JTZ}" unless File.exist?(JTZ)

    env = Jed::Environment.build { |b| b.load_time_zone_data(File.binread(JTZ)) }
    Dir.mktmpdir do |dir|
      path = File.join(dir, "e.jed")
      Jed.create(path, environment: env) { |db| assert_equal env, db.environment }
      Jed.open(path, environment: env) { |db| assert_equal Time.utc(2020, 6, 1, 8), db.query(ZONE_SQL).first[:t] }
      Jed.open(path, read_only: true) do |db|
        assert_nil db.environment
        assert_equal "22023", assert_raises(Jed::Error) { db.query(ZONE_SQL) }.sqlstate
      end
    end
  end

  def test_to_builder_adds_to_a_copy
    skip "no fixtures" unless File.exist?(JUCD) && File.exist?(JTZ)

    zones = Jed::Environment.build { |b| b.load_time_zone_data(File.binread(JTZ)) }
    both = zones.to_builder.load_unicode_data(File.binread(JUCD)).build
    Jed.memory(environment: both) do |db|
      assert_equal true, db.query(COLLATION_SQL).first[:u]
      assert_equal Time.utc(2020, 6, 1, 8), db.query(ZONE_SQL).first[:t]
    end
    Jed.memory(environment: zones) do |db|
      refute_equal "00000", assert_raises(Jed::Error) { db.query(COLLATION_SQL) }.sqlstate
    end
  end

  def test_an_environment_is_frozen
    env = Jed::Environment.build
    assert env.frozen?
    assert_raises(NoMethodError) { Jed::Environment.new }
    refute_respond_to env, :load_unicode_data
  end

  def test_malformed_bundles_raise_and_leave_the_builder_usable
    builder = Jed::EnvironmentBuilder.new
    assert_equal "XX001", assert_raises(Jed::Error) { builder.load_unicode_data("not a JUCD bundle") }.sqlstate
    assert_equal "XX001", assert_raises(Jed::Error) { builder.load_time_zone_data("not a JTZ bundle") }.sqlstate
    Jed.memory(environment: builder.build) { |db| assert_equal 1, db.query("SELECT 1").first[0] }
  end

  def test_environment_argument_is_type_checked
    assert_raises(ArgumentError) { Jed.memory(environment: Jed::EnvironmentBuilder.new) }
  end

  # --- random and clock sources (ruby.md §5c) ---

  def test_seeded_random_source_is_deterministic_and_reseedable
    random = Jed::SeededRandom.new(1)
    env = Jed::Environment.build { |b| b.random_source(random) }
    Jed.memory(environment: env) do |db|
      # The spec'd splitmix64 stream (spec/conformance/suites/expr/uuid_generate.test, `# seed: 1`).
      assert_equal "910a2dec-8902-4cc1-beeb-8da1658eec67", db.query("SELECT uuidv4()").first[0]
      refute_equal "910a2dec-8902-4cc1-beeb-8da1658eec67", db.query("SELECT uuidv4()").first[0]
      random.reseed(2)
      assert_equal "975835de-1c97-46ce-bfc8-46100bfc1e42", db.query("SELECT uuidv4()").first[0]
    end
  end

  def test_manual_clock_is_settable_and_advanceable
    clock = Jed::ManualClock.new(1_721_056_591_872_000)
    env = Jed::Environment.build { |b| b.clock_source(clock) }
    Jed.memory(environment: env) do |db|
      assert_equal Time.utc(2024, 7, 15, 15, 16, 31.872r), db.query("SELECT now()").first[0]
      clock.set(Time.utc(2030, 1, 1))
      assert_equal Time.utc(2030, 1, 1), db.query("SELECT now()").first[0]
      clock.advance(1_000_000)
      assert_equal Time.utc(2030, 1, 1, 0, 0, 1), db.query("SELECT clock_timestamp()").first[0]
    end
  end

  def test_advancing_clock_steps_per_read
    clock = Jed::AdvancingClock.new(Time.utc(2024, 1, 1), 1_000)
    env = Jed::Environment.build { |b| b.clock_source(clock) }
    Jed.memory(environment: env) do |db|
      a, b = db.query("SELECT clock_timestamp(), clock_timestamp()").first.to_a
      assert_equal 0.001, (b - a).round(6)
      clock.reset(Time.utc(2024, 1, 1), 0)
      assert_equal Time.utc(2024, 1, 1), db.query("SELECT clock_timestamp()").first[0]
    end
  end

  def test_sources_are_type_checked_and_resettable_to_the_defaults
    builder = Jed::EnvironmentBuilder.new
    assert_raises(ArgumentError) { builder.random_source(Jed::ManualClock.new(0)) }
    assert_raises(ArgumentError) { builder.clock_source(Jed::SeededRandom.new(0)) }
    assert_raises(ArgumentError) { Jed::SeededRandom.new(-1) }
    fixed = builder.clock_source(Jed::ManualClock.new(0)).build
    wall = fixed.to_builder { |b| b.clock_source(nil) }
    Jed.memory(environment: fixed) { |db| assert_equal Time.at(0).utc, db.query("SELECT now()").first[0] }
    Jed.memory(environment: wall) { |db| assert_operator db.query("SELECT now()").first[0], :>, Time.utc(2020) }
  end
end
