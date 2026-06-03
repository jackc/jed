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
#   6. null is "propagates" | "detects" | "kleene" | "null_safe" (null_safe = the
#      NULL-safe equality discipline of IS [NOT] DISTINCT FROM)
#   7. every code in `errors` exists in registry.toml
#   8. kind is a known kind (comparison | null_test | arithmetic | logical; function
#      reserved)
#   9. each (name, arg_families) signature is unique, and each punctuation symbol is
#      unique per (kind, arity, arg_families). Operators may be OVERLOADED across
#      operand families — one row per family signature sharing name+symbol (e.g. `=`
#      for integer×integer and text×text) — so name/symbol alone need not be unique.
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

  name_sigs = []   # [name, arg_families] — unique per overload
  symbol_sigs = [] # [symbol, kind, arity, arg_families] — unique per overload

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
    # A comparison spelled with punctuation (= < > <= >=) must carry its `symbol` (catches
    # a forgotten spelling). The keyword-form NULL-safe comparisons (IS [NOT] DISTINCT
    # FROM, null = "null_safe") have no punctuation symbol — exempt them, like null tests.
    if kind == "comparison" && op["null"] != "null_safe" && !op.key?("symbol")
      fail!("operator #{id}: comparison must have a `symbol`")
    end

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

    # (9) collect for uniqueness — keyed by operand-family signature, so an operator
    # overloaded across families (one row per arg_families) is allowed; a true
    # duplicate (same name AND same operand families) is still rejected.
    name_sigs << [op["name"], args]
    symbol_sigs << [op["symbol"], kind, op["arity"], args] if op.key?("symbol")
  end

  # (9) uniqueness of overload signatures
  dup_names = name_sigs.tally.select { |_, n| n > 1 }.keys
  unless dup_names.empty?
    fail!("duplicate operator (name, arg_families): #{dup_names.map { |n, a| "#{n}#{a.inspect}" }.join(', ')}")
  end
  dup_syms = symbol_sigs.tally.select { |_, n| n > 1 }.keys
  unless dup_syms.empty?
    fail!("duplicate (symbol, kind, arity, arg_families): #{dup_syms.map { |s, k, ar, a| "#{s} #{k}/arity-#{ar} #{a.inspect}" }.join(', ')}")
  end

  puts "OK: #{operators.length} operators — catalog coherent"
end

main
