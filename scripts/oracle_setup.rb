#!/usr/bin/env ruby
# frozen_string_literal: true

# scripts/oracle_setup.rb — provision the oracle database declared in
# spec/conformance/oracle_profile.toml. Idempotent: creates it if absent, verifies it if present.
#
#   rake oracle:setup   # create if absent, verify if present
#   rake oracle:reset   # drop every replayed object, keeping the database (and its locale)
#
# Why a dedicated database rather than the cluster's default `postgres`:
#
#   1. LOCALE. The corpus's expected output is calibrated to one collation. jed's is the single
#      defined `C` collation — UTF-8 byte / code-point order (types.md §11) — while a stock
#      cluster's default database is typically initdb'd under a LOCALE collation (en_US.utf8),
#      which folds case and orders differently. A database's locale is fixed at CREATE time, so
#      matching jed means creating one. PG 17+'s `builtin` provider is the right instrument: it
#      consults neither glibc nor macOS libc nor ICU, so it answers identically on every host —
#      which is what keeps the corpus tied to PostgreSQL rather than to one machine's PostgreSQL.
#   2. ISOLATION. Corpus probes replay CREATE/INSERT into the oracle. A dedicated database keeps
#      that off the cluster's default and makes a reset cheap (`rake oracle:reset`).
#
# psql-only, like the rest of the oracle path (no `pg` gem — no CLAUDE.md §14 dependency).

require "open3"
require_relative "lib/pg_oracle"

reset = ARGV.include?("--reset")

cluster = PgOracle.profile.fetch("cluster")
name = cluster.fetch("database")
provider = cluster.fetch("locale_provider")
locale = cluster.fetch("locale")
encoding = cluster.fetch("encoding")

PROVIDER_SQL = { "b" => "builtin", "i" => "icu", "c" => "libc" }.freeze
provider_word = PROVIDER_SQL.fetch(provider) do
  abort "oracle_profile.toml: unknown locale_provider #{provider.inspect} (expected c, b, or i)"
end

# Admin connections go to the cluster's default database — we may be about to create the target.
ADMIN = %w[psql -U postgres -d postgres -q -A -t -X -v ON_ERROR_STOP=1].freeze

def admin(sql)
  out, err, status = Open3.capture3(*ADMIN, "-c", sql)
  abort "oracle:setup failed\n  sql: #{sql}\n#{err.strip}" unless status.success?
  out
end

exists = admin("SELECT 1 FROM pg_database WHERE datname = '#{name}';").strip == "1"

if exists
  puts "  database #{name} exists — verifying"
else
  # template0 is required: a database may only take a locale differing from template1's when
  # cloned from the pristine template. The locale clause is provider-specific.
  clause =
    case provider
    when "b" then "LOCALE_PROVIDER builtin BUILTIN_LOCALE '#{locale}'"
    when "i" then "LOCALE_PROVIDER icu ICU_LOCALE '#{locale}' LOCALE 'C.UTF-8'"
    else          "LOCALE_PROVIDER libc LOCALE '#{locale}'"
    end
  puts "  creating #{name} (#{provider_word} / #{locale} / #{encoding})"
  admin("CREATE DATABASE #{name} TEMPLATE template0 ENCODING #{encoding} #{clause};")
end

# --reset clears replayed corpus state. Probes run inside BEGIN…ROLLBACK, so they normally leave
# nothing — but a .test that contains its OWN transaction control (suites/transactions/*.test) ends
# the probe's transaction with a real COMMIT, and whatever it created survives. Later files then
# replay their DDL onto a dirty database and can collide (the documented "run per-file, drop first"
# gotcha). Recreating the schema is the cheap fix, and it keeps the database's locale — which
# DROP DATABASE would force us to re-derive.
if reset
  out, err, status = Open3.capture3(*%W[psql -U postgres -d #{name} -q -A -t -X -v ON_ERROR_STOP=1],
                                    "-c", "DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
  abort "oracle:reset failed\n#{err.strip}" unless status.success?
  puts "  reset: schema public recreated in #{name}#{out.strip.empty? ? '' : " (#{out.strip})"}"
end

# The authoritative check is the same one every corpus/rqg task runs at connect. It reads the
# database the profile names, so it proves the thing we just created is the thing they will use.
PgOracle.new(label: "setup")

puts "  OK: #{name} matches spec/conformance/oracle_profile.toml " \
     "(PG #{cluster['pg_major']}, #{provider_word}, #{locale}, #{encoding})"
puts
puts "  Corpus authoring runs against this database automatically (the harness passes -d)."
puts "  For an interactive session against it: psql -d #{name}"
