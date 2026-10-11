# frozen_string_literal: true

# scripts/lib/query_memory_peaks.rb — the cross-core query-memory peak comparison shared by
# `mise run conformance:query_memory` and `mise run conformance:all` (spec/design/memory.md §9).
#
# Each core's runner, given JED_CONFORMANCE_QUERY_MEMORY_PEAKS, writes one line per record:
# file<TAB>record-ordinal<TAB>peak<TAB>main-storage-bytes. A task walks every core in each MODE and
# fails unless the cores' files agree.

require "set"
require_relative "tasks"

module QueryMemoryPeaks
  module_function

  # Huge but ACTIVE: every query shape exercises the reserve/release sites.
  DEFAULT_BUDGET = "1099511627776" # 1 TiB

  # mode => [extra runner args, extra env]. The third pass forces every spill-capable structure on
  # disk to spill (work_mem 256), so the spill transitions' releases are cross-checked too
  # (memory.md §6.6).
  MODES = {
    "memory" => [[], {}],
    "disk" => [["disk"], {}],
    "disk-spill" => [["disk"], { "JED_CONFORMANCE_WORK_MEM" => "256" }],
  }.freeze

  SUITES = File.join(Tasks::ROOT, "spec/conformance/suites/")

  # Corpus files that open a pre-built `# fixture:` image. The Go harness opens the image as a file and
  # the Rust and TS harnesses load it in memory, and the two backings measure committed storage
  # differently (memory.md §8.2 vs §8.7), so only their peaks are compared.
  FIXTURE_FILES = Dir[File.join(SUITES, "**/*.test")].filter_map do |path|
    path.delete_prefix(SUITES) if File.read(path).match?(/^# fixture:/)
  end.to_set.freeze

  # Compare the per-record peaks the runners wrote for one storage mode (`paths` is core => file).
  # A record a core did not run (a capability skip) is compared only among the cores that ran it.
  def mismatches(mode, paths)
    tables = paths.transform_values do |path|
      next {} unless File.exist?(path)

      File.readlines(path, chomp: true).to_h do |line|
        file, ordinal, peak, storage = line.split("\t")
        measured = FIXTURE_FILES.include?(file) ? "peak=#{peak}" : "peak=#{peak} storage=#{storage}"
        [[file, ordinal.to_i], measured]
      end
    end
    tables.values.flat_map(&:keys).uniq.sort.filter_map do |key|
      seen = tables.filter_map { |core, table| [core, table[key]] if table.key?(key) }.to_h
      next if seen.values.uniq.size <= 1

      "#{mode} #{key[0]} record #{key[1]}: " + seen.map { |core, peak| "#{core}: #{peak}" }.join("; ")
    end
  end
end
