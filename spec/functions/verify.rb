#!/usr/bin/env ruby
# frozen_string_literal: true

# Validate the function/operator catalog (catalog.toml) for internal coherence and
# cross-references into the type tables and error registry. Test-time only
# (CLAUDE.md §5); run via `rake verify`. This is a COHERENCE checker — it does NOT
# re-implement three-valued logic. Checks, with no engine required:
#
#   1. schema_version == 2
#   2. each operator has the fields required for its kind; arity == arg_families
#      length; null_test => arity 1; comparison => arity 2
#   3. every arg_families entry is "any", a real `family` in scalars.toml, or a
#      polymorphic pseudo-family (anyarray | anyelement — array-functions.md §2)
#   4. result is a scalar id in scalars.toml ("boolean"), a reserved id (promoted |
#      anyarray | anyelement), or a concrete array result `<scalar>[]` (array_positions → i32[])
#   5. arg_resolution is "promote" | "none"; "promote" requires the operand pair
#      to be comparable and the family to have a promotion rule (compare.toml)
#   6. null is "propagates" | "detects" | "kleene" | "null_safe" | "none" (null_safe = the
#      NULL-safe equality discipline of IS [NOT] DISTINCT FROM; none = the non-strict array
#      builders — the kernel handles NULL, the resolver does not short-circuit it)
#   7. every code in `errors` exists in registry.toml
#   8. kind is a known kind (comparison | null_test | arithmetic | logical | concat;
#      function reserved). "concat" is the `||` array concatenation operator (§8).
#   9. each (name, arg_families) signature is unique, and each punctuation symbol is
#      unique per (kind, arity, arg_families). Operators may be OVERLOADED across
#      operand families — one row per family signature sharing name+symbol (e.g. `=`
#      for integer×integer and text×text) — so name/symbol alone need not be unique.
#  10. precedence, if present, is an integer (the parser precedence tower; absent for
#      operators with no infix/prefix precedence, e.g. future named functions)
#  11. each [[aggregate]] (kind = "aggregate") has its own field set (name/kind/surface/
#      arg/result/null/errors, + arg_families unless arg = "star"); result is a scalar id
#      or a reserved aggregate result (sum_widen | same_as_input); null is "aggregate";
#      (name, arg_families) is unique. Aggregates are NOT operators (no symbol/precedence/
#      arg_resolution/arity), so they skip the operator-only checks above (functions.md).
#  12. each [[set_returning]] (kind = "set_returning") has its own field set (name/kind/
#      surface/arity/arg_families/arg_resolution/result/column/null/errors); arity ==
#      arg_families length; result is a scalar id or a reserved set id (set_of_promoted);
#      null is "empty_on_null"; column is a non-empty string. "promote" requires each
#      operand family to have a promotion rule (NOT a comparable pair — an SRF widens its
#      own args among themselves, it never compares two families). (name, arity) is unique.
#  13. OPTIONAL named/default args (scalar functions — functions.md §11): if present,
#      arg_names is a string array of length == arity with no duplicates; arg_defaults is a
#      string array of integer literals, length ≤ arity, filling only TRAILING slots. Across
#      a function's overloads a parameter name maps to one position (so named→slot resolution
#      is overload-independent).
#  14. OPTIONAL volatility (functions.md §12): if present, one of immutable|stable|volatile.
#      Absent ⇒ immutable. Marks a call non-foldable for a future constant-folding pass.
#
# Exit 0 = catalog is internally coherent and cross-references resolve; nonzero =
# the offending problem.

require "bundler/setup"
require "toml-rb"
require "set"

FUNC_DIR = __dir__
SPEC_DIR = File.expand_path("..", FUNC_DIR)

# Polymorphic pseudo-families (../design/array-functions.md §2). NOT real families in
# scalars.toml (not storable, no id/codec) — catalog CONTRACT TOKENS the hand-written resolver
# interprets: `anyarray` matches any array arg (binds ELEM := its element type); `anyelement`
# matches any arg (binds/checks ELEM). Admitted in arg_families AND, for the array builders'
# result, as reserved result codes (anyarray = ELEM[], anyelement = ELEM).
POLYMORPHIC_FAMILIES = %w[anyarray anyelement].to_set

# `none` is the non-strict discipline (array_append/prepend/cat): the resolver does NOT
# short-circuit a NULL argument — the kernel handles NULL itself (array-functions.md §4).
RESERVED_RESULTS = %w[promoted anyarray anyelement].to_set
# "concat" is the `||` array concatenation operator's kind (array-functions.md §8): a binary infix
# operator with its own precedence, polymorphic over anyarray/anyelement like the array functions.
# "containment" is the kind of the array containment/overlap operators `@>`/`<@`/`&&`
# (array-functions.md §10): binary infix, `(anyarray, anyarray) → boolean`, sharing `||`'s precedence.
KNOWN_KINDS      = %w[comparison null_test arithmetic logical function concat containment].to_set
NULL_BEHAVIORS   = %w[propagates detects null_safe kleene none].to_set
RESOLUTIONS      = %w[promote none].to_set
VOLATILITIES     = %w[immutable stable volatile].to_set
REQUIRED_FIELDS  = %w[name kind arity arg_families arg_resolution result null errors].freeze

# Aggregate functions (kind = "aggregate") use a distinct field set and validation
# branch — they are not operators (functions.md). `result` accepts a scalar id or one of
# these reserved widening ids; `null` is the single "aggregate" skip-NULL discipline.
RESERVED_AGG_RESULTS = %w[sum_widen same_as_input].to_set
AGG_ARGS             = %w[star expr].to_set
AGG_NULL_BEHAVIORS   = %w[aggregate].to_set
AGG_REQUIRED_FIELDS  = %w[name kind surface arg result null errors].freeze

# Set-returning functions (kind = "set_returning") use a distinct field set and validation
# branch — they expand args into a row set, fitting neither operator nor aggregate (functions.md
# §10). `result` accepts a scalar id or a reserved set id; `null` is the single "empty_on_null"
# discipline; the uniqueness key is (name, arity). An SRF arg_family may also be a polymorphic
# pseudo-family (anyarray/anyelement — unnest, array-functions.md §2), interpreted by the
# hand-written resolver exactly as for the array functions. The reserved set results:
#   set_of_promoted — a row set of one column at the promoted integer type of the args (generate_series).
#   set_of_element  — a row set of one column at the ELEM bound from the anyarray arg (unnest).
RESERVED_SRF_RESULTS = %w[set_of_promoted set_of_element].to_set
SRF_NULL_BEHAVIORS   = %w[empty_on_null].to_set
SRF_REQUIRED_FIELDS  = %w[name kind surface arity arg_families arg_resolution result column null errors].freeze

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
  fail!("catalog.toml: schema_version must be 2") unless catalog["schema_version"] == 2

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
  param_pos = {}   # [name, param_name] => position — cross-overload name→slot consistency

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
    # a forgotten spelling). Keyword-form comparisons have no punctuation symbol — exempt
    # them, like null tests: the NULL-safe comparisons (IS [NOT] DISTINCT FROM,
    # null = "null_safe") and the keyword pattern operator LIKE (name "like").
    keyword_comparison = op["null"] == "null_safe" || op["name"] == "like"
    if kind == "comparison" && !keyword_comparison && !op.key?("symbol")
      fail!("operator #{id}: comparison must have a `symbol`")
    end

    # (3) arg_families reference real families (or "any", or a polymorphic pseudo-family)
    args.each do |fam|
      next if fam == "any" || POLYMORPHIC_FAMILIES.include?(fam)
      fail!("operator #{id}: arg family #{fam.inspect} is not a family in scalars.toml") unless families.include?(fam)
    end

    # (4) result is a scalar id, a reserved id, or a concrete array result `<scalar>[]`
    # (array_positions → "i32[]"; the resolver reads it as Array(scalar) — array-functions.md §8).
    result = op["result"]
    array_result = result.is_a?(String) && result.end_with?("[]") && scalar_ids.include?(result[0..-3])
    unless scalar_ids.include?(result) || RESERVED_RESULTS.include?(result) || array_result
      fail!("operator #{id}: result #{result.inspect} is neither a scalar id, a reserved id (#{RESERVED_RESULTS.to_a.join('|')}), nor a concrete array `<scalar>[]`")
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

    # (13) optional named/default arguments (scalar functions — functions.md §11).
    if op.key?("arg_names")
      names = op["arg_names"]
      fail!("operator #{id}: arg_names must be an array") unless names.is_a?(Array)
      fail!("operator #{id}: arg_names length #{names.length} != arity #{op['arity']}") unless names.length == op["arity"]
      names.each do |nm|
        fail!("operator #{id}: arg_names entry #{nm.inspect} must be a non-empty string") unless nm.is_a?(String) && !nm.empty?
      end
      fail!("operator #{id}: duplicate parameter name in arg_names #{names.inspect}") unless names.uniq.length == names.length
      # cross-overload consistency: a parameter name maps to one position for this function.
      names.each_with_index do |pn, i|
        key = [op["name"], pn]
        if param_pos.key?(key) && param_pos[key] != i
          fail!("operator #{id}: parameter #{pn.inspect} maps to position #{param_pos[key]} and #{i} across overloads of #{op['name']}")
        end
        param_pos[key] = i
      end
    end
    if op.key?("arg_defaults")
      defs = op["arg_defaults"]
      fail!("operator #{id}: arg_defaults must be an array") unless defs.is_a?(Array)
      fail!("operator #{id}: arg_defaults length #{defs.length} > arity #{op['arity']}") if defs.length > op["arity"]
      defs.each do |d|
        fail!("operator #{id}: arg_defaults entry #{d.inspect} must be an integer-literal string") unless d.is_a?(String) && d.match?(/\A-?\d+\z/)
      end
    end

    # (14) optional volatility class (functions.md §12); absent ⇒ immutable.
    if op.key?("volatility")
      fail!("operator #{id}: volatility #{op['volatility'].inspect} not in (#{VOLATILITIES.to_a.join('|')})") unless VOLATILITIES.include?(op["volatility"])
    end

    # (15) optional VARIADIC flag (array-functions.md §12); absent ⇒ false. A boolean; true is
    # valid only on a scalar function (kind = "function") with a non-empty arg_families (the last
    # entry is the variadic element family) and no arg_defaults (defaults + variadic are not
    # modeled — PG allows a default on the variadic param, but jed has no such built-in).
    if op.key?("variadic")
      v = op["variadic"]
      fail!("operator #{id}: variadic #{v.inspect} must be a boolean") unless [true, false].include?(v)
      if v
        fail!("operator #{id}: variadic is valid only on a scalar function (kind = \"function\")") unless kind == "function"
        fail!("operator #{id}: variadic function needs a non-empty arg_families") if args.empty?
        fail!("operator #{id}: variadic + arg_defaults is not supported") if op.key?("arg_defaults")
      end
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

  # (11) aggregates (kind = "aggregate") — a separate array + field set. Aggregates are
  # not operators, so they skip the symbol/precedence/arg_resolution/arity checks above.
  aggregates = catalog["aggregate"] || []
  agg_sigs = [] # [name, arg_families] — unique per overload, like operators
  aggregates.each do |ag|
    id = ag["name"] || "(unnamed)"

    AGG_REQUIRED_FIELDS.each do |f|
      fail!("aggregate #{id}: missing field `#{f}`") unless ag.key?(f)
    end
    fail!("aggregate #{id}: kind must be \"aggregate\"") unless ag["kind"] == "aggregate"
    fail!("aggregate #{id}: surface must be a non-empty string") unless ag["surface"].is_a?(String) && !ag["surface"].empty?

    arg = ag["arg"]
    fail!("aggregate #{id}: arg #{arg.inspect} not in (#{AGG_ARGS.to_a.join('|')})") unless AGG_ARGS.include?(arg)
    fams = ag["arg_families"]
    if arg == "expr"
      fail!("aggregate #{id}: arg=expr needs a non-empty arg_families") unless fams.is_a?(Array) && !fams.empty?
      fams.each do |fam|
        next if fam == "any"
        fail!("aggregate #{id}: arg family #{fam.inspect} is not a family in scalars.toml") unless families.include?(fam)
      end
    elsif fams && !fams.empty?
      fail!("aggregate #{id}: arg=star takes no arg_families")
    end

    result = ag["result"]
    unless scalar_ids.include?(result) || RESERVED_AGG_RESULTS.include?(result)
      fail!("aggregate #{id}: result #{result.inspect} is neither a scalar id nor a reserved aggregate result (#{RESERVED_AGG_RESULTS.to_a.join('|')})")
    end

    fail!("aggregate #{id}: null #{ag['null'].inspect} must be \"aggregate\"") unless AGG_NULL_BEHAVIORS.include?(ag["null"])

    (ag["errors"] || []).each do |code|
      fail!("aggregate #{id}: error code #{code.inspect} is not in registry.toml") unless error_codes.include?(code)
    end

    agg_sigs << [ag["name"], (fams || [])]
  end
  dup_aggs = agg_sigs.tally.select { |_, n| n > 1 }.keys
  unless dup_aggs.empty?
    fail!("duplicate aggregate (name, arg_families): #{dup_aggs.map { |n, a| "#{n}#{a.inspect}" }.join(', ')}")
  end

  # (12) set-returning functions (kind = "set_returning") — a separate array + field set.
  # SRFs are neither operators nor aggregates, so they skip the operator/aggregate checks above.
  set_returning = catalog["set_returning"] || []
  srf_sigs = [] # [name, arity] — unique per overload (the 2-arg and 3-arg forms share a name)
  set_returning.each do |sf|
    id = sf["name"] || "(unnamed)"

    SRF_REQUIRED_FIELDS.each do |f|
      fail!("set_returning #{id}: missing field `#{f}`") unless sf.key?(f)
    end
    fail!("set_returning #{id}: kind must be \"set_returning\"") unless sf["kind"] == "set_returning"
    fail!("set_returning #{id}: surface must be a non-empty string") unless sf["surface"].is_a?(String) && !sf["surface"].empty?
    fail!("set_returning #{id}: column must be a non-empty string") unless sf["column"].is_a?(String) && !sf["column"].empty?

    args = sf["arg_families"]
    fail!("set_returning #{id}: arg_families must be an array") unless args.is_a?(Array)
    fail!("set_returning #{id}: arity #{sf['arity']} != arg_families length #{args.length}") unless sf["arity"] == args.length
    args.each do |fam|
      next if fam == "any"
      # A polymorphic pseudo-family (anyarray/anyelement) is interpreted by the hand-written
      # resolver, not a real family in scalars.toml (unnest, array-functions.md §2).
      next if POLYMORPHIC_FAMILIES.include?(fam)
      fail!("set_returning #{id}: arg family #{fam.inspect} is not a family in scalars.toml") unless families.include?(fam)
    end

    res = sf["arg_resolution"]
    fail!("set_returning #{id}: arg_resolution #{res.inspect} not in (#{RESOLUTIONS.to_a.join('|')})") unless RESOLUTIONS.include?(res)
    # "promote" widens an SRF's OWN args among themselves (never compares two families), so —
    # unlike a binary operator — it requires each family to have a promotion rule, not a
    # comparable pair. This is the one deliberate divergence from the operator promote check.
    if res == "promote"
      args.uniq.each do |fam|
        fail!("set_returning #{id}: no promotion rule for family #{fam.inspect} in compare.toml") unless promotion_families.include?(fam)
      end
    end

    result = sf["result"]
    unless scalar_ids.include?(result) || RESERVED_SRF_RESULTS.include?(result)
      fail!("set_returning #{id}: result #{result.inspect} is neither a scalar id nor a reserved set result (#{RESERVED_SRF_RESULTS.to_a.join('|')})")
    end

    fail!("set_returning #{id}: null #{sf['null'].inspect} must be \"empty_on_null\"") unless SRF_NULL_BEHAVIORS.include?(sf["null"])

    (sf["errors"] || []).each do |code|
      fail!("set_returning #{id}: error code #{code.inspect} is not in registry.toml") unless error_codes.include?(code)
    end

    srf_sigs << [sf["name"], sf["arity"]]
  end
  dup_srfs = srf_sigs.tally.select { |_, n| n > 1 }.keys
  unless dup_srfs.empty?
    fail!("duplicate set_returning (name, arity): #{dup_srfs.map { |n, a| "#{n}/arity-#{a}" }.join(', ')}")
  end

  puts "OK: #{operators.length} operators, #{aggregates.length} aggregates, #{set_returning.length} set-returning — catalog coherent"
end

main
