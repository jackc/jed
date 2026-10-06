#!/usr/bin/env ruby
# frozen_string_literal: true

# Validate the conformance taxonomy: the manifest (capabilities + profiles) against
# the corpus (.test files and their `# requires:` declarations). Test-time only
# (CLAUDE.md §5); run via `rake verify`. Checks, with no engine required:
#
#   1. every capability a profile lists is a defined capability
#   2. every profile `includes` names a defined profile (no cycles)
#   3. every capability a .test or .process.toml file requires is defined
#   4. no orphan capabilities (defined, never required by any corpus file)
#   5. every .test file carries exactly one `# requires:` line
#   6. every `# cost: N` directive parses as a non-negative integer, and any file using
#      one declares the `resource.cost_metering` capability (CLAUDE.md §13)
#   7. the oracle profile (oracle_profile.toml) is structurally complete — every asserted
#      cluster key present and well-typed, every session pin a non-empty string. Static only:
#      whether the LIVE oracle matches is PgOracle.assert_profile!'s job, which needs a server.
#
# Exit 0 = taxonomy is internally coherent; nonzero = the offending problem.

require "bundler/setup"
require "toml-rb"

CONF_DIR = __dir__
SUITES_DIR = File.join(CONF_DIR, "suites")
PROCESS_DIR = File.join(CONF_DIR, "process")

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

# The raw token of every `# max_sql_length: N` directive in a .test file (CLAUDE.md §13). It
# runs the next record under a (small) per-handle input-size cap so an over-long statement
# aborts with 54000; a comment the stock sqllogictest runner ignores, like `# cost:`.
def parse_max_sql_length_directives(path)
  File.readlines(path, encoding: "UTF-8")
      .filter_map { |l| l[/^#\s*max_sql_length:\s*(\S+)/i, 1] }
end

# (7) The oracle profile is the declared configuration of the live PostgreSQL oracle
# (scripts/lib/pg_oracle.rb). This check is STATIC — it proves the table is complete and
# well-formed so a typo cannot make an assertion silently vacuous. It cannot check the live
# server; PgOracle.assert_profile! does that at connect time.
CLUSTER_KEYS = { "pg_major" => Integer, "locale_provider" => String,
                 "locale" => String, "encoding" => String, "database" => String }.freeze

def check_oracle_profile
  path = File.join(CONF_DIR, "oracle_profile.toml")
  fail!("oracle_profile.toml missing") unless File.exist?(path)

  doc = TomlRB.load_file(path)
  fail!("oracle_profile.toml: schema_version must be 1") unless doc["schema_version"] == 1

  cluster = doc["cluster"] or fail!("oracle_profile.toml: missing [cluster]")
  CLUSTER_KEYS.each do |key, type|
    value = cluster[key]
    fail!("oracle_profile.toml: [cluster] missing #{key}") if value.nil?
    fail!("oracle_profile.toml: [cluster] #{key} must be #{type}, got #{value.class}") unless value.is_a?(type)
  end
  provider = cluster["locale_provider"]
  unless %w[c b i].include?(provider)
    fail!("oracle_profile.toml: [cluster] locale_provider must be c|b|i, got #{provider.inspect}")
  end

  links = cluster.dig("tzdata", "backward_links")
  unless [true, false].include?(links)
    fail!("oracle_profile.toml: [cluster.tzdata] backward_links must be a boolean")
  end

  session = doc["session"] or fail!("oracle_profile.toml: missing [session]")
  fail!("oracle_profile.toml: [session] is empty") if session.empty?
  session.each do |name, value|
    fail!("oracle_profile.toml: [session] #{name} must be a string, got #{value.class}") unless value.is_a?(String)
    fail!("oracle_profile.toml: [session] #{name} is empty") if value.strip.empty?
  end
  # TimeZone is per-record (the `# timezone:` directive), set by PgOracle from the record itself.
  if session.key?("TimeZone")
    fail!("oracle_profile.toml: [session] must not pin TimeZone — it is per-record (`# timezone:`)")
  end

  [cluster, session]
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

    scalar_limits = File.readlines(path).filter_map { |l| l[/^#\s*max_scalar_bytes:\s*(\S+)/, 1] }
    unless scalar_limits.empty?
      scalar_limits.each { |tok| fail!("#{rel}: invalid max_scalar_bytes #{tok}") unless tok.match?(/\A\d+\z/) }
      fail!("#{rel}: max_scalar_bytes requires resource.scalar_memory") unless reqs.include?("resource.scalar_memory")
    end

    query_limits = File.readlines(path).filter_map { |l| l[/^#\s*max_query_memory_bytes:\s*(\S+)/, 1] }
    unless query_limits.empty?
      query_limits.each { |tok| fail!("#{rel}: invalid max_query_memory_bytes #{tok}") unless tok.match?(/\A\d+\z/) }
      fail!("#{rel}: max_query_memory_bytes requires resource.query_memory") unless reqs.include?("resource.query_memory")
    end

    # (6) max_sql_length directives: each is a non-negative integer, and the file must require
    # the input-size capability (so cores lacking the gate skip it — conformance.md §3).
    max_sql_lengths = parse_max_sql_length_directives(path)
    unless max_sql_lengths.empty?
      max_sql_lengths.each do |tok|
        fail!("#{rel}: `# max_sql_length: #{tok}` is not a non-negative integer") unless tok =~ /\A\d+\z/
      end
      unless reqs.include?("resource.sql_length_limit")
        fail!("#{rel}: uses `# max_sql_length:` but does not require `resource.sql_length_limit`")
      end
    end
  end

  # Layer-4 host/process scenarios are not sqllogictest files, but they participate in the same
  # capability taxonomy through a top-level TOML `requires` array.
  Dir.glob(File.join(PROCESS_DIR, "*.process.toml")).sort.each do |path|
    rel = path.delete_prefix("#{CONF_DIR}/")
    reqs = TomlRB.load_file(path)["requires"] || []
    fail!("#{rel}: empty `requires` array") if reqs.empty?
    reqs.each do |capability|
      fail!("#{rel}: requires undefined capability #{capability}") unless cap_set.include?(capability)
      required_anywhere << capability
    end
  end

  # (4) no orphan capabilities
  orphans = cap_set - required_anywhere
  fail!("orphan capabilities (defined, never required by any test): #{orphans.sort.join(', ')}") unless orphans.empty?

  # (7) the oracle profile table
  oracle_cluster, oracle_session = check_oracle_profile

  puts "OK: #{capabilities.length} capabilities, #{profiles.length} profiles, " \
       "#{tests.length} test files — taxonomy coherent"
  puts "OK: oracle profile — PG #{oracle_cluster['pg_major']}, provider #{oracle_cluster['locale_provider']}, " \
       "locale #{oracle_cluster['locale']}, #{oracle_session.length} session pins"
end

main
