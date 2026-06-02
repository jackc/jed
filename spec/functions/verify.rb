#!/usr/bin/env ruby
# frozen_string_literal: true

# Validate the function/operator catalog (catalog.toml) for internal coherence and
# cross-references into the type tables and error registry. Test-time only
# (CLAUDE.md §5); run via `rake verify`. This is a COHERENCE checker — it does NOT
# re-implement three-valued logic. Checks, with no engine required:
#
#   1. schema_version == 1
#   2. each operator has the fields required for its kind; arity == arg_families
#      length; null_test => arity 1; comparison => arity 2
#   3. every arg_families entry is "any" or a real `family` in scalars.toml
#   4. result is a scalar id in scalars.toml ("boolean"), or a reserved id (promoted)
#   5. arg_resolution is "promote" | "none"; "promote" requires the operand pair
#      to be comparable and the family to have a promotion rule (compare.toml)
#   6. null is "propagates" | "detects" | "kleene" (null_safe reserved for IS [NOT]
#      DISTINCT FROM)
#   7. every code in `errors` exists in registry.toml
#   8. kind is a known kind (comparison | null_test | arithmetic | logical; function
#      reserved)
#   9. name unique across the catalog; symbol unique per (kind, arity)
#  10. precedence, if present, is an integer (the parser precedence tower; absent for
#      operators with no infix/prefix precedence, e.g. future named functions)
#
# Exit 0 = catalog is internally coherent and cross-references resolve; nonzero =
# the offending problem.

require "bundler/setup"
require "toml-rb"
require "set"

FUNC_DIR = __dir__
SPEC_DIR = File.expand_path("..", FUNC_DIR)

RESERVED_RESULTS = %w[promoted].to_set
KNOWN_KINDS      = %w[comparison null_test arithmetic logical function].to_set
NULL_BEHAVIORS   = %w[propagates detects null_safe kleene].to_set
RESOLUTIONS      = %w[promote none].to_set
REQUIRED_FIELDS  = %w[name kind arity arg_families arg_resolution result null errors].freeze

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

def main
  catalog = TomlRB.load_file(File.join(FUNC_DIR, "catalog.toml"))
  scalars = TomlRB.load_file(File.join(SPEC_DIR, "types", "scalars.toml"))
  compare = TomlRB.load_file(File.join(SPEC_DIR, "types", "compare.toml"))
  registry = TomlRB.load_file(File.join(SPEC_DIR, "errors", "registry.toml"))

  # (1) schema_version
  fail!("catalog.toml: schema_version must be 1") unless catalog["schema_version"] == 1

  # reference sets drawn from the canonical type/error tables
  families   = (scalars["type"] || []).map { |t| t["family"] }.to_set
  scalar_ids = (scalars["type"] || []).map { |t| t["id"] }.to_set
  error_codes = (registry["error"] || []).map { |e| e["code"] }.to_set
  promotion_families = (compare["promotion"] || []).map { |p| p["family"] }.to_set
  comparable_pairs = (compare["comparable"] || []).map { |c| [c["left_family"], c["right_family"]] }.to_set

  operators = catalog["operator"] || []
  fail!("catalog.toml: no [[operator]] entries") if operators.empty?

  names = []
  symbols_by_kind_arity = Hash.new { |h, k| h[k] = [] }

  operators.each do |op|
    id = op["name"] || "(unnamed)"

    # (2) required fields, arity vs arg_families, kind vs arity
    REQUIRED_FIELDS.each do |f|
      fail!("operator #{id}: missing field `#{f}`") unless op.key?(f)
    end
    args = op["arg_families"]
    fail!("operator #{id}: arg_families must be an array") unless args.is_a?(Array)
    fail!("operator #{id}: arity #{op['arity']} != arg_families length #{args.length}") unless op["arity"] == args.length

    kind = op["kind"]
    # (8) known kind
    fail!("operator #{id}: unknown kind #{kind.inspect}") unless KNOWN_KINDS.include?(kind)
    fail!("operator #{id}: null_test must have arity 1") if kind == "null_test" && op["arity"] != 1
    fail!("operator #{id}: comparison must have arity 2") if kind == "comparison" && op["arity"] != 2
    fail!("operator #{id}: comparison must have a `symbol`") if kind == "comparison" && !op.key?("symbol")

    # (3) arg_families reference real families (or "any")
    args.each do |fam|
      next if fam == "any"
      fail!("operator #{id}: arg family #{fam.inspect} is not a family in scalars.toml") unless families.include?(fam)
    end

    # (4) result is a scalar id or a reserved id
    result = op["result"]
    unless scalar_ids.include?(result) || RESERVED_RESULTS.include?(result)
      fail!("operator #{id}: result #{result.inspect} is neither a scalar id nor reserved (#{RESERVED_RESULTS.to_a.join('|')})")
    end

    # (5) arg_resolution; promote must reference a comparable pair + promotion rule
    res = op["arg_resolution"]
    fail!("operator #{id}: arg_resolution #{res.inspect} not in (#{RESOLUTIONS.to_a.join('|')})") unless RESOLUTIONS.include?(res)
    if res == "promote"
      fail!("operator #{id}: arg_resolution=promote needs exactly two operand families") unless args.length == 2
      fail!("operator #{id}: operand pair #{args.inspect} is not comparable in compare.toml") unless comparable_pairs.include?(args)
      args.uniq.each do |fam|
        fail!("operator #{id}: no promotion rule for family #{fam.inspect} in compare.toml") unless promotion_families.include?(fam)
      end
    end

    # (6) null behavior
    fail!("operator #{id}: null #{op['null'].inspect} not in (#{NULL_BEHAVIORS.to_a.join('|')})") unless NULL_BEHAVIORS.include?(op["null"])

    # (10) precedence is an integer if present
    if op.key?("precedence")
      fail!("operator #{id}: precedence #{op['precedence'].inspect} must be an integer") unless op["precedence"].is_a?(Integer)
    end

    # (7) declared errors exist in the registry
    (op["errors"] || []).each do |code|
      fail!("operator #{id}: error code #{code.inspect} is not in registry.toml") unless error_codes.include?(code)
    end

    # (9) collect for uniqueness
    names << op["name"]
    symbols_by_kind_arity[[kind, op["arity"]]] << op["symbol"] if op.key?("symbol")
  end

  # (9) uniqueness
  dup_names = names.tally.select { |_, n| n > 1 }.keys
  fail!("duplicate operator names: #{dup_names.join(', ')}") unless dup_names.empty?
  symbols_by_kind_arity.each do |(kind, arity), syms|
    dup = syms.tally.select { |_, n| n > 1 }.keys
    fail!("duplicate symbol(s) for #{kind}/arity-#{arity}: #{dup.join(', ')}") unless dup.empty?
  end

  puts "OK: #{operators.length} operators — catalog coherent"
end

main
