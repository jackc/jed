# frozen_string_literal: true

require_relative "test_helper"

# Tests for the host-bundle loaders (spec/design/ruby.md §5a): Jed.load_unicode_data /
# Jed.load_time_zone_data. These cover the GEM's seam — passing bundle bytes to the engine-global
# loaders and surfacing a malformed bundle as Jed::Error. The collation/tz SEMANTICS are the
# engine's (tested in the corpus + the Rust core); here we just prove the seam wires through.
#
# Loading is process-global and idempotent, so these are order-independent under minitest's random
# order: a "load then use" test re-loads (replacing) the global set each run.
class HostBundlesTest < Minitest::Test
  SPEC = File.expand_path("../../../spec", __dir__)
  JUCD = File.join(SPEC, "collation/fixtures/unicode.jucd")
  JTZ  = File.join(SPEC, "tz/fixtures/tzdata.jtz")

  def test_load_unicode_data_enables_collation
    skip "no JUCD fixture at #{JUCD}" unless File.exist?(JUCD)

    Jed.load_unicode_data(File.binread(JUCD))
    Jed.memory do |db|
      # Under UCA "unicode", 'a' sorts before 'B' (case-insensitive primary weight); under the
      # built-in C collation it does not (byte order: 'B'=0x42 < 'a'=0x61).
      row = db.query(%(SELECT ('a' < 'B' COLLATE "unicode") AS u, ('a' < 'B') AS c)).first
      assert_equal true, row[:u]
      assert_equal false, row[:c]
    end
  end

  def test_load_time_zone_data_enables_named_zones
    skip "no JTZ fixture at #{JTZ}" unless File.exist?(JTZ)

    Jed.load_time_zone_data(File.binread(JTZ))
    Jed.memory do |db|
      # 2020-06-01 12:00 UTC is 08:00 in America/New_York (EDT, UTC-4). AT TIME ZONE on a
      # timestamptz yields the local wall-clock as a (zoneless) timestamp → a UTC Time in the gem.
      t = db.query(
        %(SELECT (TIMESTAMPTZ '2020-06-01 12:00:00+00' AT TIME ZONE 'America/New_York') AS t)
      ).first[:t]
      assert_equal Time.utc(2020, 6, 1, 8, 0, 0), t
    end
  end

  def test_unknown_zone_is_22023
    skip "no JTZ fixture at #{JTZ}" unless File.exist?(JTZ)

    Jed.load_time_zone_data(File.binread(JTZ))
    Jed.memory do |db|
      err = assert_raises(Jed::Error) do
        db.query(%(SELECT TIMESTAMPTZ '2020-06-01 12:00:00+00' AT TIME ZONE 'Mars/Phobos'))
      end
      assert_equal "22023", err.sqlstate
    end
  end

  def test_malformed_unicode_bundle_raises
    err = assert_raises(Jed::Error) { Jed.load_unicode_data("not a JUCD bundle") }
    refute_nil err.sqlstate
  end

  def test_malformed_timezone_bundle_raises
    err = assert_raises(Jed::Error) { Jed.load_time_zone_data("not a JTZ bundle") }
    refute_nil err.sqlstate
  end
end
