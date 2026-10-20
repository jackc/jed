# frozen_string_literal: true
require 'json'
require '/home/jack/.codex/worktrees/b8e6/jed/bench/ruby-native/results/nway-frontier-followup/direct_evidence'
rows = %w[before final].map do |state|
  raw = JSON.parse(File.read("/tmp/jed-nway-frontier-smoke-#{state}.json"))
  result = NwayFrontierDirectEvidence.verify(raw, available: state == 'final')
  [raw, result]
end
raise 'smoke semantics differ' unless rows[0][0].fetch('semantic') == rows[1][0].fetch('semantic')
%i[physical_plans_sha256 file_signature].each do |key|
  raise "smoke #{key} differ" unless rows[0][1].fetch(key) == rows[1][1].fetch(key)
end
puts JSON.generate(rows.map(&:last))
