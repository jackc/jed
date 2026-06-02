#!/usr/bin/env ruby
# frozen_string_literal: true

# Validate the cost unit schedule (schedule.toml) for internal coherence. Test-time
# only (CLAUDE.md §5); run via `rake verify`. This is a COHERENCE checker — it does
# NOT re-implement the accrual logic (that is hand-written per core). Checks, with no
# engine required:
#
#   1. schema_version == 1
#   2. at least one [[unit]]
#   3. each unit has a non-empty string `id` and `event`, and an integer `weight` >= 0
#   4. unit ids are unique
#   5. the three phase-1 unit ids are present (so a rename is caught as a regression;
#      a new unit such as a future `page_read` may be ADDED freely — cost.md)
#
# Exit 0 = schedule is internally coherent; nonzero = the offending problem.

require "bundler/setup"
require "toml-rb"
require "set"

COST_DIR = __dir__

# The unit ids the cost seam threads through the executor/evaluator/storage today.
# Adding units is fine; renaming/removing one of these is a breaking regression.
REQUIRED_UNIT_IDS = %w[storage_row_read row_produced operator_eval].to_set

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

def main
  schedule = TomlRB.load_file(File.join(COST_DIR, "schedule.toml"))

  # (1) schema_version
  fail!("schedule.toml: schema_version must be 1") unless schedule["schema_version"] == 1

  # (2) at least one unit
  units = schedule["unit"] || []
  fail!("schedule.toml: no [[unit]] entries") if units.empty?

  ids = []
  units.each do |u|
    id = u["id"]
    # (3) field shapes
    fail!("unit #{id.inspect}: `id` must be a non-empty string") unless id.is_a?(String) && !id.empty?
    fail!("unit #{id}: `event` must be a non-empty string") unless u["event"].is_a?(String) && !u["event"].empty?
    w = u["weight"]
    fail!("unit #{id}: `weight` must be an integer") unless w.is_a?(Integer)
    fail!("unit #{id}: `weight` must be >= 0 (got #{w})") if w.negative?
    ids << id
  end

  # (4) unique ids
  dup = ids.tally.select { |_, n| n > 1 }.keys
  fail!("duplicate unit ids: #{dup.join(', ')}") unless dup.empty?

  # (5) the phase-1 units are all present
  missing = (REQUIRED_UNIT_IDS - ids.to_set).to_a
  fail!("schedule.toml: missing required unit id(s): #{missing.join(', ')}") unless missing.empty?

  puts "OK: #{units.length} cost units — schedule coherent"
end

main
