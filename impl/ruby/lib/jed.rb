# frozen_string_literal: true

require_relative "jed/version"
require_relative "jed/error"
require_relative "jed/ffi"
require_relative "jed/codec"
require_relative "jed/coerce"
require_relative "jed/params"
require_relative "jed/result"
require_relative "jed/database"

# jed — an embedded SQL database with PostgreSQL behavior and a strict, static type system.
#
# The Ruby gem wraps the safe Rust core (CLAUDE.md §2/§13; spec/design/ruby.md): the engine runs at
# Rust speed and conforms by construction. This module is the convenience entry point; {Jed::Database}
# is the handle.
#
#   Jed.memory do |db|
#     db.execute("CREATE TABLE t (id i32 PRIMARY KEY, name text)")
#     db.execute("INSERT INTO t VALUES (1, 'alice'), (2, 'bob')")
#     db.query("SELECT name FROM t ORDER BY id").each { |row| puts row[:name] }
#   end
module Jed
  class << self
    # Open a new in-memory database (see {Database.memory}).
    def memory(&) = Database.memory(&)

    # Create a new file-backed database at `path` (see {Database.create}).
    def create(path, &) = Database.create(path, &)

    # Open an existing file-backed database at `path` (see {Database.open}).
    def open(path, read_only: false, &) = Database.open(path, read_only: read_only, &)

    # Load a Unicode collation bundle (a `JUCD` byte string) into the **engine-global** collation
    # set (spec/design/collation.md). The bare engine ships `C`-collation only; this adds the
    # linguistic collations the bundle provides (e.g. `COLLATE "unicode"`, case folding, `ILIKE`).
    # Process-global (the SQLite model) — affects every open and future database. Raises
    # {Jed::Error} on a malformed bundle. Typical use: `Jed.load_unicode_data(File.binread(path))`.
    def load_unicode_data(bytes) = load_bundle(Jed::FFI::LOAD_UNICODE, bytes)

    # Load an IANA time-zone bundle (a `JTZ` byte string) into the **engine-global** zone set
    # (spec/design/timezones.md). The bare engine ships `UTC` + fixed offsets only; this adds the
    # named zones the bundle provides (`AT TIME ZONE 'America/New_York'`, `date_trunc(…, zone)`, the
    # session `time_zone` setting). Process-global. Raises {Jed::Error} on a malformed bundle.
    def load_time_zone_data(bytes) = load_bundle(Jed::FFI::LOAD_TIMEZONE, bytes)

    private

    def load_bundle(fn, bytes)
      raw = bytes.to_s.b # a binary copy; Fiddle passes a pointer to its bytes
      result = Jed::Codec.take(fn.call(raw, raw.bytesize))
      raise Jed::Error.new(result[:sqlstate], result[:message]) if result[:kind] == :error

      nil
    end
  end
end
