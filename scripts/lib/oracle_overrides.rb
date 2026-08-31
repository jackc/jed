# frozen_string_literal: true

require_relative "bundle_setup"
require "toml-rb"

# scripts/lib/oracle_overrides.rb — the jed-vs-PG divergence ledger
# (spec/conformance/oracle_overrides.toml), shared by the oracle importer and the RQG firehose.
#
# A divergence is a normalized SQL string whose committed expected output is jed's intended
# (divergent) answer, not PG's. The importer leaves a matched record untouched; the firehose
# classifies a matched divergence as DELIBERATE (log + continue) rather than a candidate bug.
# Both tools normalize identically so the same ledger entry matches in both.
#
# Loading is STRICT. This used to rescue a missing toml-rb and carry on with an empty ledger, on
# the grounds that overrides are optional. They are not: an empty ledger silently reclassifies
# every deliberate jed-vs-PG divergence as unledgered, so the importer nags for overrides that
# already exist and the RQG firehose reports known divergences as candidate bugs. A missing gem
# should stop the tool, not quietly change what it concludes.
class OracleOverrides
  PATH = File.expand_path("../../spec/conformance/oracle_overrides.toml", __dir__)

  # `file` is a path suffix (e.g. "types/decimal.test"); pass the file a record came from to scope
  # the ledger to its overrides. A firehose case has no committed file, so pass nil to match every
  # override whose `file` suffix the (generated) path would end with — callers filter by file when
  # they have one and consult the full set otherwise.
  def initialize(path: nil)
    @path = path
    @overrides = load_overrides
  end

  def normalize(sql) = sql.strip.gsub(/\s+/, " ").chomp(";")
  def overridden?(sql) = @overrides.key?(normalize(sql))
  def reason(sql) = @overrides[normalize(sql)]

  private

  def load_overrides
    return {} unless File.exist?(PATH)

    doc = TomlRB.load_file(PATH)
    (doc["override"] || []).each_with_object({}) do |o, h|
      next if @path && !@path.end_with?(o["file"])

      h[normalize(o["sql"])] = o["reason"]
    end
  end
end
