# frozen_string_literal: true

require "bigdecimal"
require "date"

module Jed
  # Coerce a rendered cell (a String, or nil for SQL NULL) into a native Ruby value, keyed by the
  # column's canonical type name (spec/design/ruby.md §3). The mapping mirrors ActiveRecord's
  # PostgreSQL adapter: coerce whenever Ruby has a faithful native type, and — like AR — represent a
  # `timestamp`/`date` `±infinity` (which `Time`/`Date` cannot hold) as `±Float::INFINITY`. NULL is
  # always nil; anything without a clean native target stays its canonical String.
  module Coerce
    module_function

    POS_INF = "infinity"
    NEG_INF = "-infinity"

    # `date`: "YYYY-MM-DD" with an optional " BC" era (PG/jed render); the year is the displayed
    # value, BC mapping it to astronomical (`1 - displayed`).
    DATE_RE = /\A(\d+)-(\d{2})-(\d{2})( BC)?\z/
    # `timestamp[tz]`: date + " HH:MM:SS", optional ".frac" (≤6 digits), optional "+00" (tz marker —
    # jed renders timestamptz in UTC), optional " BC".
    TS_RE = /\A(\d+)-(\d{2})-(\d{2}) (\d{2}):(\d{2}):(\d{2})(?:\.(\d{1,6}))?(?:\+00)?( BC)?\z/

    def value(type, raw)
      return nil if raw.nil?

      case type
      when "i16", "i32", "i64" then Integer(raw, 10)
      when "boolean" then raw == "true"
      when "f32", "f64" then float(raw)
      when "decimal" then BigDecimal(raw) # jed decimal is finite-only — always a clean BigDecimal
      when "date" then date(raw)
      when "timestamp", "timestamptz" then timestamp(raw)
      else raw # interval, uuid, bytea, range, array, composite, unknown → canonical String
      end
    end

    # Parse a rendered float, honoring the engine's PG-style special spellings.
    def float(raw)
      case raw
      when "Infinity" then Float::INFINITY
      when "-Infinity" then -Float::INFINITY
      when "NaN" then Float::NAN
      else Float(raw)
      end
    end

    def date(raw)
      return Float::INFINITY if raw == POS_INF
      return -Float::INFINITY if raw == NEG_INF

      m = DATE_RE.match(raw) or return raw # unknown shape → fall back to the String, never crash
      Date.new(astro_year(m[1], m[4]), m[2].to_i, m[3].to_i)
    end

    def timestamp(raw)
      return Float::INFINITY if raw == POS_INF
      return -Float::INFINITY if raw == NEG_INF

      m = TS_RE.match(raw) or return raw
      usec = m[7] ? m[7].ljust(6, "0").to_i : 0
      # jed renders both timestamp (wall-clock) and timestamptz (the UTC instant) in UTC, so building
      # a UTC Time is faithful for both — matching AR's default_timezone = :utc adapter behavior.
      Time.utc(astro_year(m[1], m[8]), m[2].to_i, m[3].to_i, m[4].to_i, m[5].to_i, m[6].to_i, usec)
    end

    # The astronomical year for a displayed year + optional " BC" era. PG/jed render a BC year as the
    # positive displayed value with a " BC" suffix; astronomical numbering is `1 - displayed` (1 BC =
    # year 0). Ruby `Date`/`Time` use astronomical numbering, so this maps directly.
    def astro_year(displayed, era)
      y = displayed.to_i
      era ? 1 - y : y
    end
  end
end
