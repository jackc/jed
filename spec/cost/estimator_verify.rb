#!/usr/bin/env ruby
# frozen_string_literal: true

# Validate the mechanical plan-estimator facts (estimator.toml). Test-time only; run through
# `rake verify`. This checks constants and total tie orders, never planner control flow — the
# estimator stays hand-written in every core (CLAUDE.md §5, estimator.md).

require "bundler/setup"
require "toml-rb"
require "set"

ESTIMATOR_PATH = File.join(__dir__, "estimator.toml")

EXPECTED_SELECTIVITY = {
  "equality" => [1, 200],
  "inequality" => [1, 3],
  "paired_range" => [1, 200],
  "null_test" => [1, 200],
  "match" => [1, 200],
  "matching" => [1, 100],
  "boolean" => [1, 2],
  "opaque" => [1, 3],
}.freeze

EXPECTED_ACCESS_ORDER = %w[pk btree gist gin pk_interval index_interval full].freeze
EXPECTED_JOIN_ORDER = %w[index_nested_loop hash nested_loop].freeze

def fail!(message)
  warn "FAIL: #{message}"
  exit 1
end

def expect_equal(data, key, expected)
  actual = data[key]
  fail!("estimator.toml: #{key} must be #{expected.inspect}, got #{actual.inspect}") unless actual == expected
end

def main
  data = TomlRB.load_file(ESTIMATOR_PATH)

  expect_equal(data, "schema_version", 1)
  expect_equal(data, "max_estimate", 9_223_372_036_854_775_807)
  expect_equal(data, "max_selectivity_denominator", 1_000_000)
  expect_equal(data, "row_rounding", "ceiling")
  expect_equal(data, "parameter_strategy", "generic_before_bind")
  expect_equal(data, "join_dp_limit", 8)
  expect_equal(data, "large_join_strategy", "greedy_cheapest_next")
  expect_equal(data, "default_distinct_values", 200)
  expect_equal(data, "default_srf_rows", 1000)
  expect_equal(data, "default_variable_key_bytes", 1)

  entries = data["selectivity"] || []
  ids = entries.map { |entry| entry["id"] }
  duplicates = ids.tally.select { |_, count| count > 1 }.keys
  fail!("estimator.toml: duplicate selectivity ids: #{duplicates.join(', ')}") unless duplicates.empty?

  missing = EXPECTED_SELECTIVITY.keys.to_set - ids.to_set
  extra = ids.to_set - EXPECTED_SELECTIVITY.keys.to_set
  fail!("estimator.toml: missing selectivities: #{missing.to_a.sort.join(', ')}") unless missing.empty?
  fail!("estimator.toml: unknown selectivities: #{extra.to_a.sort.join(', ')}") unless extra.empty?

  entries.each do |entry|
    id = entry["id"]
    numerator = entry["numerator"]
    denominator = entry["denominator"]
    unless numerator.is_a?(Integer) && denominator.is_a?(Integer)
      fail!("selectivity #{id}: numerator and denominator must be integers")
    end
    fail!("selectivity #{id}: denominator must be positive") unless denominator.positive?
    if denominator > data["max_selectivity_denominator"]
      fail!("selectivity #{id}: denominator exceeds max_selectivity_denominator")
    end
    unless numerator.between?(0, denominator)
      fail!("selectivity #{id}: numerator must be in 0..denominator")
    end
    fail!("selectivity #{id}: fraction must be reduced") unless numerator.gcd(denominator) == 1
    expected = EXPECTED_SELECTIVITY.fetch(id)
    actual = [numerator, denominator]
    fail!("selectivity #{id}: expected #{expected.join('/')}, got #{actual.join('/')}") unless actual == expected
  end

  tie = data["tie_break"] || {}
  expect_equal(tie, "access_path", EXPECTED_ACCESS_ORDER)
  expect_equal(tie, "join_algorithm", EXPECTED_JOIN_ORDER)
  expect_equal(tie, "relation_order", "source_ordinal_lexicographic")
  expect_equal(tie, "index_name_order", "lowercase_utf8_bytewise")
  fail!("tie_break.access_path must contain unique values") unless tie["access_path"].uniq == tie["access_path"]
  fail!("tie_break.join_algorithm must contain unique values") unless tie["join_algorithm"].uniq == tie["join_algorithm"]

  puts "OK: #{entries.length} estimator selectivities + total tie orders coherent"
end

main
