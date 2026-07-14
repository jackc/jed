#!/usr/bin/env ruby
# frozen_string_literal: true

# Validate the mechanical plan-estimator facts (estimator.toml). Test-time only; run through
# `rake verify`. This checks constants and total tie orders, never planner control flow — the
# estimator stays hand-written in every core (CLAUDE.md §5, estimator.md).

require "bundler/setup"
require "toml-rb"
require "set"

ESTIMATOR_PATH = File.join(__dir__, "estimator.toml")
VECTORS_PATH = File.join(__dir__, "estimator_vectors.toml")
SCHEDULE_PATH = File.join(__dir__, "schedule.toml")

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
EXPECTED_ACCESS_METHOD = {
  "gist_equal" => "equality",
  "gist_range" => "matching",
  "gin_contains" => "matching",
  "gin_overlaps" => "matching",
  "gin_member" => "matching",
  "gin_equal" => "matching",
  "unsupported" => "opaque",
}.freeze

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
  expect_equal(data, "statistics_target", 100)
  expect_equal(data, "statistics_sample_rows", 30_000)
  expect_equal(data, "statistics_kmv_hashes", 4_096)
  expect_equal(data, "statistics_mcv_entries", 100)
  expect_equal(data, "statistics_histogram_bounds", 101)
  expect_equal(data, "statistics_max_value_bytes", 128)
  expect_equal(data, "statistics_ndv_scale_numerator", 1)
  expect_equal(data, "statistics_ndv_scale_denominator", 10)

  entries = data["selectivity"] || []
  selectivity_count = entries.length
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

  access_method = data["access_method"] || {}
  fail!("estimator.toml: access_method classification drift") unless access_method == EXPECTED_ACCESS_METHOD

  vectors = TomlRB.load_file(VECTORS_PATH)
  expect_equal(vectors, "schema_version", 1)
  schedule = TomlRB.load_file(SCHEDULE_PATH).fetch("unit")
  weights = schedule.to_h { |unit| [unit.fetch("id"), unit.fetch("weight")] }
  access_rank = EXPECTED_ACCESS_ORDER.each_with_index.to_h

  ids = []
  %w[arithmetic predicate candidate].each do |section|
    entries = vectors[section] || []
    entries.each do |entry|
      id = entry["id"]
      fail!("estimator_vectors.toml: #{section} entry missing id") unless id.is_a?(String) && !id.empty?
      ids << "#{section}:#{id}"
    end
  end
  dup_ids = ids.tally.select { |_, count| count > 1 }.keys
  fail!("estimator_vectors.toml: duplicate ids: #{dup_ids.join(', ')}") unless dup_ids.empty?

  (vectors["arithmetic"] || []).each do |entry|
    fail!("arithmetic #{entry['id']}: unknown op") unless %w[scale_ceil sat_add sat_mul].include?(entry["op"])
    %w[a b expected].each do |field|
      fail!("arithmetic #{entry['id']}: #{field} must be a nonnegative integer") unless entry[field].is_a?(Integer) && entry[field] >= 0
    end
    if entry["op"] == "scale_ceil"
      fail!("arithmetic #{entry['id']}: c must be positive") unless entry["c"].is_a?(Integer) && entry["c"].positive?
    end
  end

  token_ids = EXPECTED_SELECTIVITY.keys + %w[all zero unique and or not]
  (vectors["predicate"] || []).each do |entry|
    tokens = entry["tokens"]
    fail!("predicate #{entry['id']}: tokens must be a nonempty string array") unless tokens.is_a?(Array) && !tokens.empty? && tokens.all?(String)
    unknown = tokens - token_ids
    fail!("predicate #{entry['id']}: unknown tokens #{unknown.join(', ')}") unless unknown.empty?
    %w[n expected].each do |field|
      fail!("predicate #{entry['id']}: #{field} must be a nonnegative integer") unless entry[field].is_a?(Integer) && entry[field] >= 0
    end
  end

  (vectors["candidate"] || []).each do |entry|
    id = entry.fetch("id")
    kind = entry["kind"]
    fail!("candidate #{id}: unknown kind #{kind.inspect}") unless access_rank.key?(kind)
    name = entry["index_name"]
    fail!("candidate #{id}: index_name must be a string") unless name.is_a?(String)
    expected_tie = "#{access_rank.fetch(kind)}:#{name}"
    fail!("candidate #{id}: tie_key must be #{expected_tie.inspect}") unless entry["tie_key"] == expected_tie
    %w[scan_rows output_rows access_pages table_height filter_nodes access_work est_rows est_cost].each do |field|
      fail!("candidate #{id}: #{field} must be a nonnegative integer") unless entry[field].is_a?(Integer) && entry[field] >= 0
    end
    fail!("candidate #{id}: produces_rows must be boolean") unless [true, false].include?(entry["produces_rows"])
    fail!("candidate #{id}: est_rows must equal output_rows") unless entry["est_rows"] == entry["output_rows"]
    units = entry["units"] || {}
    unknown_units = units.keys - weights.keys
    fail!("candidate #{id}: unknown units #{unknown_units.join(', ')}") unless unknown_units.empty?
    unless units.values.all? { |count| count.is_a?(Integer) && count >= 0 }
      fail!("candidate #{id}: unit counts must be nonnegative integers")
    end
    cost = weights.sum { |unit, weight| units.fetch(unit, 0) * weight }
    cost = [cost, data.fetch("max_estimate")].min
    fail!("candidate #{id}: est_cost expected #{cost}, got #{entry['est_cost']}") unless entry["est_cost"] == cost
  end

  puts "OK: #{selectivity_count} estimator selectivities + #{ids.length} P4 vectors + total tie orders coherent"
end

main
