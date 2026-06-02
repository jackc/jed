#!/usr/bin/env ruby
# frozen_string_literal: true

# Validate the conformance taxonomy: the manifest (capabilities + profiles) against
# the corpus (.test files and their `# requires:` declarations). Test-time only
# (CLAUDE.md §5); run via `rake verify`. Checks, with no engine required:
#
#   1. every capability a profile lists is a defined capability
#   2. every profile `includes` names a defined profile (no cycles)
#   3. every capability a .test file `requires:` is a defined capability
#   4. no orphan capabilities (defined but never required by any test)
#   5. every .test file carries exactly one `# requires:` line
#   6. every `# cost: N` directive parses as a non-negative integer, and any file using
#      one declares the `resource.cost_metering` capability (CLAUDE.md §13)
#
# Exit 0 = taxonomy is internally coherent; nonzero = the offending problem.

require "bundler/setup"
require "toml-rb"

CONF_DIR = __dir__
SUITES_DIR = File.join(CONF_DIR, "suites")

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

# Parse the single `# requires: a, b, c` header line from a .test file.
# Returns an array of capability ids (possibly empty if the line is absent —
# which the caller treats as an error).
def parse_requires(path)
  lines = File.readlines(path, encoding: "UTF-8")
  req = lines.find { |l| l =~ /^#\s*requires:/i }
  return nil unless req

  req.sub(/^#\s*requires:/i, "").split(",").map(&:strip).reject(&:empty?)
end

# The raw token of every `# cost: N` directive in a .test file (CLAUDE.md §13). A
# cost directive asserts the deterministic accrued cost of the next query/statement-ok
# record; it is a comment the stock sqllogictest runner ignores, like `# requires:`.
def parse_cost_directives(path)
  File.readlines(path, encoding: "UTF-8")
      .filter_map { |l| l[/^#\s*cost:\s*(\S+)/i, 1] }
end

def main
  manifest = TomlRB.load_file(File.join(CONF_DIR, "manifest.toml"))
  capabilities = (manifest["capability"] || []).map { |c| c["id"] }
  cap_set = capabilities.to_set
  fail!("duplicate capability ids") unless capabilities.uniq.length == capabilities.length

  profiles = manifest["profile"] || []
  profile_ids = profiles.map { |p| p["id"] }.to_set

  # (1) profile capabilities are defined; (2) includes are defined
  profiles.each do |p|
    (p["capabilities"] || []).each do |c|
      fail!("profile #{p['id']} lists undefined capability #{c}") unless cap_set.include?(c)
    end
    inc = p["includes"]
    fail!("profile #{p['id']} includes undefined profile #{inc}") if inc && !profile_ids.include?(inc)
  end

  # (2) no include cycles — walk each profile's include chain
  profiles.each do |p|
    seen = []
    cur = p
    while cur
      fail!("profile include cycle at #{cur['id']}") if seen.include?(cur["id"])
      seen << cur["id"]
      inc = cur["includes"]
      cur = inc && profiles.find { |q| q["id"] == inc }
    end
  end

  # (3)+(5) every .test requires-line references defined capabilities
  tests = Dir.glob(File.join(SUITES_DIR, "**", "*.test")).sort
  fail!("no .test files found under #{SUITES_DIR}") if tests.empty?
  required_anywhere = Set.new
  tests.each do |path|
    rel = path.delete_prefix("#{CONF_DIR}/")
    reqs = parse_requires(path)
    fail!("#{rel}: missing `# requires:` header line") if reqs.nil?
    fail!("#{rel}: empty `# requires:` line") if reqs.empty?
    reqs.each do |c|
      fail!("#{rel}: requires undefined capability #{c}") unless cap_set.include?(c)
      required_anywhere << c
    end

    # (6) cost directives: each is a non-negative integer, and the file must require
    # the cost-metering capability (so non-cost-aware cores skip it — conformance.md §3).
    costs = parse_cost_directives(path)
    unless costs.empty?
      costs.each do |tok|
        fail!("#{rel}: `# cost: #{tok}` is not a non-negative integer") unless tok =~ /\A\d+\z/
      end
      unless reqs.include?("resource.cost_metering")
        fail!("#{rel}: uses `# cost:` but does not require `resource.cost_metering`")
      end
    end
  end

  # (4) no orphan capabilities
  orphans = cap_set - required_anywhere
  fail!("orphan capabilities (defined, never required by any test): #{orphans.sort.join(', ')}") unless orphans.empty?

  puts "OK: #{capabilities.length} capabilities, #{profiles.length} profiles, " \
       "#{tests.length} test files — taxonomy coherent"
end

main
