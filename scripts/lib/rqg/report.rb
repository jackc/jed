# frozen_string_literal: true

require "json"
require "fileutils"

module RQG
  # Per-run output: a timestamped dir under bench/results/rqg/ (the mutation/stress precedent) holding
  # run.jsonl (one event per case) and flagged/ (reduced divergence `.test`s + their .md sidecars).
  # Outside rake ci — this is a discovery tool, not a conformance gate. Time.now is fine here (a plain
  # script, not a Workflow), so the stamp is wall-clock.
  class Report
    attr_reader :dir

    def initialize(root, stamp)
      @dir = File.join(root, "bench/results/rqg", stamp)
      FileUtils.mkdir_p(File.join(@dir, "flagged"))
      @jsonl = File.join(@dir, "run.jsonl")
    end

    def flagged_path(name) = File.join(@dir, "flagged", name)

    def event(hash) = File.open(@jsonl, "a") { |f| f.puts(JSON.generate(hash)) }

    # A human-readable sidecar for a flagged divergence.
    def write_sidecar(name, kase, coltypes, jed_detail, ledger_reason)
      File.write(flagged_path("#{name}.md"), <<~MD)
        # RQG divergence — #{kase.shape} seed #{kase.seed}

        Reproduce: `rake 'rqg:replay[#{kase.seed},#{kase.shape}]'`

        ## Query
        ```sql
        #{kase.query}
        ```
        coltypes=`#{coltypes}` sortmode=`#{kase.sortmode}` shape=`#{kase.shape_key}`

        ## Setup
        ```sql
        #{kase.setup.join(";\n")};
        ```

        ## jed vs PG
        Expected output is PostgreSQL's. The jed (Rust) core disagreed:

        ```
        #{jed_detail.to_s.strip}
        ```

        ## Triage
        #{ledger_reason ? "Ledgered divergence: #{ledger_reason}" : "NOT in oracle_overrides.toml — candidate shared-core bug, or a missing override. If deliberate, add an [[override]] to spec/conformance/oracle_overrides.toml; else investigate the cores."}
      MD
    end
  end
end
