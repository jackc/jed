# frozen_string_literal: true

module Jed
  # Coerce a rendered cell (a String, or nil for SQL NULL) into a native Ruby value, keyed by the
  # column's canonical type name (spec/design/ruby.md §3). Only the unambiguous scalars are coerced;
  # everything else stays its canonical String — lossless, and free of surprise (a BigDecimal or
  # Time coercion is a documented follow-on, ruby.md §6). NULL is always nil.
  module Coerce
    module_function

    def value(type, raw)
      return nil if raw.nil?

      case type
      when "i16", "i32", "i64" then Integer(raw, 10)
      when "boolean" then raw == "true"
      when "f32", "f64" then float(raw)
      else raw # text, decimal, timestamp(tz), date, interval, uuid, bytea, range, array, composite, unknown
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
  end
end
