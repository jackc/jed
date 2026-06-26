# frozen_string_literal: true

# scripts/rqg_gen.rb — the RQG-vs-PG firehose (CLAUDE.md §7; .scratch/testing-ideas.md §1 item 1).
#
# Generates random SQL over jed's SUPPORTED SUBSET (type-aware, by construction — scripts/lib/rqg/),
# fills each query's expected output from the LIVE PostgreSQL oracle (scripts/lib/pg_oracle.rb), and
# runs the resulting self-contained candidate `.test`s through the Rust conformance harness — which
# reuses CI's EXACT renderer + comparator. A harness PASS ⇔ jed == PG ⇔ a valid corpus entry (emit
# it, curated/capped/deduped); a FAIL ⇔ a divergence: classify against the ledger
# (scripts/lib/oracle_overrides.rb — known → log), else reduce (scripts/reduce.rb) + flag a candidate
# shared-core bug. This catches the bug class the 3-core differential is blind to: one all cores share.
#
#   ruby scripts/rqg_gen.rb 7                       # one case, seed 7, every shape (check, no emit)
#   ruby scripts/rqg_gen.rb --sweep 200 --emit      # seeds 1..200, emit agreements to suites/rqg/
#   ruby scripts/rqg_gen.rb --from 1 --to 50 --shapes select_where
#
# OUT of rake ci (slow, needs live PG — like mutation/stress/bench). The PRODUCT that flows INTO
# rake ci is the emitted suites/rqg/*.test, which then runs on all three cores.

require "open3"
require "fileutils"
require "time"
require "rbconfig"
require_relative "lib/pg_oracle"
require_relative "lib/oracle_overrides"
require_relative "lib/rqg/spec_data"
require_relative "lib/rqg/shapes"
require_relative "lib/rqg/corpus"
require_relative "lib/rqg/report"

module RQG
  REPO = File.expand_path("..", __dir__)
  TMP_DIR = File.join(REPO, "spec/conformance/suites/_rqg_tmp")
  CORPUS_DIR = File.join(REPO, "spec/conformance/suites/rqg")

  # Driver: generate -> oracle-fill -> batch-harness -> emit/flag.
  class Firehose
    def initialize(opts)
      @opts = opts
      @ledger = OracleOverrides.new
      stamp = Time.now.strftime("%Y%m%d-%H%M%S")
      @report = Report.new(REPO, stamp)
      @stats = Hash.new(0)
    end

    def run
      SpecData.validate!
      shapes = @opts[:shapes]
      seeds = @opts[:seeds]
      warn "RQG: #{shapes.join(', ')} × seeds #{seeds.first}..#{seeds.last} " \
           "(#{seeds.size * shapes.size} cases)#{@opts[:emit] ? ' [emit]' : ''}"

      candidates = build_candidates(shapes, seeds)
      warn "RQG: #{candidates.size} candidates built (#{@stats[:skipped]} generator-misses skipped); " \
           "running the Rust conformance harness…"
      statuses = run_harness
      classify(candidates, statuses)

      flush_corpus
      summarize
      @stats[:flagged]
    end

    private

    # Generate each (shape, seed), fill expected from PG, write a self-contained candidate into
    # _rqg_tmp. Returns [{kase:, coltypes:, expected:, rel:}]. A case PG itself rejects (generator
    # strayed out of subset) or cannot describe is skipped (NOT a divergence — we only flag jed-fails).
    def build_candidates(shapes, seeds)
      FileUtils.rm_rf(TMP_DIR)
      FileUtils.mkdir_p(TMP_DIR)
      out = []
      shapes.each do |shape|
        gen = Shapes::REGISTRY[shape] or abort "rqg: unknown shape #{shape.inspect}"
        seeds.each do |seed|
          @stats[:generated] += 1
          kase = gen.call(seed)
          filled = oracle_fill(kase)
          if filled.nil?
            @stats[:skipped] += 1
            next
          end
          rel = "_rqg_tmp/#{shape}_#{seed}.test"
          File.write(File.join(REPO, "spec/conformance/suites", rel),
                     RQG.candidate_file(kase, filled[:coltypes], filled[:expected]))
          out << { kase: kase, coltypes: filled[:coltypes], expected: filled[:expected], rel: rel }
        end
      end
      out
    end

    # Run the case's setup + query against PG; return {coltypes:, expected:} or nil to skip.
    def oracle_fill(kase)
      oracle = PgOracle.new(label: "#{kase.shape}/#{kase.seed}")
      kase.setup.each { |sql| oracle.add_applied(sql) }
      pg_sql = oracle.pg_sql(kase.query)
      _out, err = oracle.run(oracle.terminate(kase.query))
      return nil if err =~ /^ERROR:/ # PG rejected setup or query — a generator miss, not a divergence

      types = oracle.describe(pg_sql)
      return nil if types.empty? # PG cannot plan it (likely a jed extension PG lacks)

      tags = begin
        types.map { |(_n, t)| oracle.tag_for(t) }
      rescue RuntimeError
        return nil # an unrenderable PG type — skip rather than crash
      end
      is_bool = types.map { |(_n, t)| t == "boolean" }
      vals = oracle.query_values(pg_sql, is_bool)
      vals = oracle.canonical_rowsort(vals, tags.size) if kase.sortmode == "rowsort"
      { coltypes: tags.join, expected: vals }
    end

    # Build + run the Rust conformance harness once over the whole suites tree; return {rel => [status,
    # detail]} for our _rqg_tmp candidates only. Release build per the depth_limit stack-size note.
    def run_harness
      ok = system(*%w[cargo build --release --quiet --bin conformance], chdir: File.join(REPO, "impl/rust"))
      abort "rqg: rust harness build failed" unless ok
      bin = File.join(REPO, "impl/rust/target/release/conformance")
      out, = Open3.capture2e(bin, chdir: REPO)
      parse_harness(out)
    end

    def parse_harness(out)
      lines = out.lines
      result = {}
      lines.each_with_index do |line, i|
        m = line.match(%r{\A(PASS|FAIL|SKIP)\s+(_rqg_tmp/\S+?\.test)})
        next unless m

        status, rel = m[1], m[2]
        detail = [line]
        lines[(i + 1)..].each do |l|
          break if l =~ /\A(PASS|FAIL|SKIP)\s/ || l =~ /\A\d+ passed,/

          detail << l
        end
        result[rel] = [status, detail.join]
      end
      result
    end

    def classify(candidates, statuses)
      @corpus_by_shape = {}
      candidates.each do |c|
        status, detail = statuses[c[:rel]] || ["MISSING", "candidate not run by harness"]
        @report.event(seed: c[:kase].seed, shape: c[:kase].shape, status: status,
                      coltypes: c[:coltypes], sortmode: c[:kase].sortmode,
                      shape_key: c[:kase].shape_key, query: c[:kase].query)
        case status
        when "PASS" then on_agree(c)
        when "FAIL" then on_diverge(c, detail)
        when "SKIP" then @stats[:skipped_cap] += 1
        else @stats[:harness_missing] += 1
        end
      end
    end

    def on_agree(c)
      @stats[:agree] += 1
      return unless @opts[:emit]

      corpus = (@corpus_by_shape[c[:kase].shape] ||= open_corpus(c[:kase].shape))
      case corpus.maybe_emit(c[:kase], c[:coltypes], c[:expected])
      when :added then @stats[:emitted] += 1
      when :dup then @stats[:emit_dup] += 1
      when :full then @stats[:emit_full] += 1
      end
    end

    def on_diverge(c, detail)
      @stats[:diverge] += 1
      kase = c[:kase]
      reason = @ledger.overridden?(kase.query) ? @ledger.reason(kase.query) : nil
      if reason
        @stats[:diverge_ledgered] += 1
        @report.event(seed: kase.seed, shape: kase.shape, status: "LEDGERED", reason: reason)
        return
      end
      @stats[:flagged] += 1
      name = "#{kase.shape}_#{kase.seed}"
      flagged = @report.flagged_path("#{name}.test")
      FileUtils.cp(File.join(REPO, "spec/conformance/suites", c[:rel]), flagged)
      reduce(flagged) unless @opts[:no_reduce]
      @report.write_sidecar(name, kase, c[:coltypes], detail, nil)
      warn "RQG: FLAGGED divergence #{name} (seed #{kase.seed}) — #{@report.flagged_path("#{name}.md")}"
    end

    def reduce(flagged)
      reduced = flagged.sub(/\.test\z/, ".min.test")
      system(RbConfig.ruby, File.join(REPO, "scripts/reduce.rb"),
             flagged, "--core", "rust", "-o", reduced)
    rescue StandardError => e
      warn "RQG: reduce failed (keeping unreduced): #{e.message}"
    end

    def open_corpus(shape)
      Corpus.new(File.join(CORPUS_DIR, "#{shape}.test"), shape, total_cap: @opts[:cap])
    end

    def flush_corpus
      (@corpus_by_shape || {}).each_value(&:flush)
      FileUtils.rm_rf(TMP_DIR) unless @opts[:keep]
    end

    def summarize
      warn format(
        "RQG done: generated=%d skipped=%d agree=%d diverge=%d (ledgered=%d flagged=%d) emitted=%d (dup=%d full=%d)",
        @stats[:generated], @stats[:skipped], @stats[:agree], @stats[:diverge],
        @stats[:diverge_ledgered], @stats[:flagged], @stats[:emitted], @stats[:emit_dup], @stats[:emit_full]
      )
      warn "RQG: report dir #{@report.dir}"
    end
  end
end

def parse_opts(argv)
  opts = { shapes: nil, seeds: nil, emit: false, no_reduce: false, cap: 30, keep: false }
  rest = []
  while (a = argv.shift)
    case a
    when "--sweep" then opts[:seeds] = (1..Integer(argv.shift))
    when "--from" then @from = Integer(argv.shift)
    when "--to" then @to = Integer(argv.shift)
    when "--shapes" then opts[:shapes] = argv.shift.split(",")
    when "--emit" then opts[:emit] = true
    when "--no-reduce" then opts[:no_reduce] = true
    when "--keep" then opts[:keep] = true
    when "--cap" then opts[:cap] = Integer(argv.shift)
    else rest << a
    end
  end
  opts[:seeds] ||= (@from..@to) if defined?(@from) && defined?(@to) && @from && @to
  opts[:seeds] ||= (Integer(rest.first)..Integer(rest.first)) unless rest.empty?
  opts[:seeds] ||= (1..1)
  opts[:shapes] ||= RQG::Shapes::REGISTRY.keys
  opts
end

if $PROGRAM_NAME == __FILE__
  opts = parse_opts(ARGV)
  flagged = RQG::Firehose.new(opts).run
  exit(flagged.zero? ? 0 : 1)
end
