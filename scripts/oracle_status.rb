#!/usr/bin/env ruby
# frozen_string_literal: true

# scripts/oracle_status.rb — print the DECLARED oracle profile (spec/conformance/oracle_profile.toml)
# beside the LIVE server's actual values, and assert they agree.
#
# The profile is what the conformance corpus's expected output is calibrated to (CLAUDE.md §7):
# a differently-configured PostgreSQL answers differently — in VALUES, not just formatting — so an
# undeclared oracle silently corrupts an import. `PgOracle.assert_profile!` enforces the cluster
# facts at connect; this script is the human-readable view of the same contract, plus the session
# pins (which are applied, not asserted, so they cannot mismatch — they are shown for review).
#
#   mise run oracle:status # print declared vs actual, exit nonzero on mismatch

require_relative "lib/pg_oracle"

profile = PgOracle.profile
cluster = profile.fetch("cluster")
session = profile.fetch("session", {})

# Constructing the oracle runs assert_profile! — a mismatch aborts here with the actionable
# message, which is exactly the behaviour the importer and the firehose get.
PgOracle.new(label: "status")

live = PgOracle.live_cluster

puts
puts "  Oracle profile — spec/conformance/oracle_profile.toml"
puts
puts format("  %-22s %-22s %s", "FIELD", "DECLARED", "LIVE")
%w[pg_major locale_provider locale encoding database].each do |key|
  puts format("  %-22s %-22s %s", key, cluster[key], live.fetch(key, "?"))
end
puts format("  %-22s %-22s %s", "tzdata.backward_links",
            cluster.dig("tzdata", "backward_links"), live["tzdata_backward_links"])
puts
puts "  Session pins (SET LOCAL in every probe's preamble; TimeZone is per-record):"
session.each { |name, value| puts format("    %-28s %s", name, value) }
puts
puts "  OK: the live oracle matches the declared profile."
