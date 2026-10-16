# frozen_string_literal: true

require_relative "jed/version"
require_relative "jed/error"
require_relative "jed/ffi"
require_relative "jed/codec"
require_relative "jed/coerce"
require_relative "jed/params"
require_relative "jed/result"
require_relative "jed/host_function"
require_relative "jed/environment"
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
    # Open a new in-memory database (see {Database.memory}). `environment:` is a {Jed::Environment}
    # (loaded bundles, host functions, random and clock sources — spec/design/ruby.md §5a); without
    # one the database gets a fresh empty environment.
    def memory(environment: nil, &) = Database.memory(environment: environment, &)

    # Create a new file-backed database at `path` (see {Database.create}).
    def create(path, environment: nil, &) = Database.create(path, environment: environment, &)

    # Open an existing file-backed database at `path` (see {Database.open}).
    def open(path, read_only: false, environment: nil, &)
      Database.open(path, read_only: read_only, environment: environment, &)
    end
  end
end
