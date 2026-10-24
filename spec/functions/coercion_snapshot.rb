# frozen_string_literal: true

# The argument-coercion capture check (spec/design/arg-coercion.md §7). Under cost ranking, adding or
# removing a catalog row can change which row an existing call selects, and stored view bodies,
# CHECKs, defaults and index expressions are re-resolved from their text on load. So the selection
# the pass makes for every numeric argument tuple is recorded in coercion_snapshot.toml, and a change
# to it must be part of the change that causes it.
#
#   ruby spec/functions/coercion_snapshot.rb            # fail if the snapshot is stale
#   ruby spec/functions/coercion_snapshot.rb --write    # rewrite the snapshot
#
# This is an independent reference implementation of arg-coercion.md §4 over the built-in catalog
# (host functions are per-environment, so they are not part of it), reading the same compare.toml
# cost table the cores' codegen reads. It covers the kind = "function" rows the generic path resolves:
# not the VARIADIC rows, the named-argument rows (make_interval and friends, functions.md §11), or the
# range constructors (range-functions.md), which have resolvers of their own. For each name and arity, every argument
# position that some row types numerically ranges over the numeric sources below; a position no row
# types numerically takes each family the rows have there. A tuple that matches a row exactly is
# not a coercion and is skipped, as is a tuple no row admits. Every other tuple records the
# selected row's signature, or 42725 for a tie. Unknown-typed arguments are not enumerated: they
# never coerce.

require "bundler/setup"
require "toml-rb"
require "set"

SPEC_DIR = File.expand_path("..", __dir__)
SNAPSHOT = File.join(__dir__, "coercion_snapshot.toml")

NUMERIC_FAMILIES = %w[integer decimal float].to_set
# The numeric argument sources: each scalar type, plus the two literal sources of the cost table.
NUMERIC_SOURCES = %w[i16 i32 i64 decimal f32 f64 integer_literal decimal_literal].freeze
# The default type of a literal source (an integer constant is untyped and defaults to i64,
# types.md §6); its family is the family a row slot is matched against.
LITERAL_DEFAULT = { "integer_literal" => "i64", "decimal_literal" => "decimal" }.freeze

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

def load_data
  catalog = TomlRB.load_file(File.join(SPEC_DIR, "functions", "catalog.toml"))
  scalars = TomlRB.load_file(File.join(SPEC_DIR, "types", "scalars.toml"))
  compare = TomlRB.load_file(File.join(SPEC_DIR, "types", "compare.toml"))
  ranges = TomlRB.load_file(File.join(SPEC_DIR, "types", "ranges.toml"))
  family_of = (scalars["type"] || []).to_h { |t| [t["id"], t["family"]] }
  range_ids = (ranges["range"] || []).to_set { |r| r["id"] }
  rows = (catalog["operator"] || []).select do |o|
    o["kind"] == "function" && !o["variadic"] && !o["arg_names"] && !range_ids.include?(o["result"]) &&
      o["arg_families"].all? { |f| family_of.value?(f) || f == "any" }
  end
  [rows, family_of, compare["arg_coercion"] || []]
end

# The cheapest coercion of `src` into a type `accept` admits (arg-coercion.md §3), or nil: a literal
# also takes its default type's rows; the first row in table order wins a tie. An integer literal
# narrows only when its value fits, which this check cannot know, so it is assumed to fit.
def cheapest(src, coercions)
  own = LITERAL_DEFAULT.fetch(src, src)
  coercions
    .select { |r| (r["from"] == own || r["from"] == src) && yield(r["to"]) }
    .min_by.with_index { |r, i| [r["cost"], i] }
end

# The score of one row against a source tuple, or nil when a position is not admitted.
def score(row, srcs, family_of, coercions)
  row["arg_families"].each_with_index.sum do |slot, i|
    src = srcs[i]
    own_family = family_of[LITERAL_DEFAULT.fetch(src, src)] || src
    next 0 if slot == "any" || own_family == slot

    best = cheapest(src, coercions) { |to| family_of[to] == slot }
    return nil unless best

    best["cost"]
  end
end

def signature(row) = "#{row['name']}(#{row['arg_families'].join(', ')})"

def resolutions
  rows, family_of, coercions = load_data
  out = {}
  rows.group_by { |r| [r["name"], r["arg_families"].length] }.sort.each do |(name, arity), group|
    next if arity.zero?

    choices = (0...arity).map do |i|
      fams = group.map { |r| r["arg_families"][i] }.uniq
      fams.any? { |f| NUMERIC_FAMILIES.include?(f) } ? NUMERIC_SOURCES : fams.reject { |f| f == "any" }.sort
    end
    next if choices.any?(&:empty?)

    choices.first.product(*choices.drop(1)).each do |srcs|
      exact = group.any? do |r|
        r["arg_families"].each_with_index.all? do |slot, i|
          slot == "any" || (family_of[LITERAL_DEFAULT.fetch(srcs[i], srcs[i])] || srcs[i]) == slot
        end
      end
      next if exact

      scored = group.filter_map { |r| (s = score(r, srcs, family_of, coercions)) && [s, r] }
      next if scored.empty?

      min = scored.map(&:first).min
      best = scored.select { |s, _| s == min }.map(&:last)
      out["#{name}(#{srcs.join(', ')})"] = best.length == 1 ? signature(best.first) : "42725"
    end
  end
  out
end

def render(res)
  lines = [
    "# @generated by spec/functions/coercion_snapshot.rb — DO NOT EDIT. The argument-coercion capture",
    "# check (../design/arg-coercion.md §7): the built-in row the argument-coercion pass selects for every",
    "# numeric argument tuple that matches no row exactly (or 42725 for a tie). A catalog or cost-table",
    "# change that moves an entry must show the move here; regenerate with --write and review the diff.",
    "",
    "[selects]",
  ]
  res.each { |call, sel| lines << "#{call.inspect} = #{sel.inspect}" }
  "#{lines.join("\n")}\n"
end

def main
  content = render(resolutions)
  if ARGV.include?("--write")
    File.write(SNAPSHOT, content)
    puts "wrote #{SNAPSHOT}"
    return
  end
  current = File.exist?(SNAPSHOT) ? File.read(SNAPSHOT) : ""
  return puts("OK: argument-coercion snapshot current (#{content.lines.count - 6} coerced calls)") if current == content

  old = current.lines.grep(/ = /).to_set
  new = content.lines.grep(/ = /).to_set
  (old - new).sort.each { |l| warn "- #{l}" }
  (new - old).sort.each { |l| warn "+ #{l}" }
  fail!("coercion_snapshot.toml is stale: a catalog or cost-table change moved the calls above " \
        "(arg-coercion.md §7) — review them, then run 'ruby spec/functions/coercion_snapshot.rb --write'")
end

main
