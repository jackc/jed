# frozen_string_literal: true

require "set"
require "fileutils"
require_relative "case"

module RQG
  # The curated, capped, deduped per-shape corpus file (spec/conformance/suites/rqg/<shape>.test).
  # Emit happens ONLY on jed==PG agreement, deduped by structural shape_key and capped, so the file
  # grows with novel shapes and saturates (re-running rqg:emit is then a no-op). The file's single
  # `# requires:` header is the UNION of its blocks' caps (the harness gates the whole file at once).
  class Corpus
    SPLIT = /^(?=# --- seed )/.freeze

    def initialize(path, shape, total_cap: 30)
      @path = path
      @shape = shape
      @cap = total_cap
      load
    end

    attr_reader :path

    def count = @blocks.size
    def full? = @blocks.size >= @cap

    # Returns :added | :dup | :full.
    def maybe_emit(kase, coltypes, expected)
      key = kase.shape_key
      return :dup if @keys.include?(key)
      return :full if full?

      @blocks << RQG.case_block(kase, coltypes, expected)
      @keys << key
      @caps |= kase.caps
      :added
    end

    def flush
      FileUtils.mkdir_p(File.dirname(@path))
      File.write(@path, render)
    end

    private

    def load
      @blocks = []
      @keys = Set.new
      @caps = Set.new
      return unless File.exist?(@path)

      chunks = File.read(@path).split(SPLIT)
      header = chunks.shift.to_s
      @caps = parse_requires(header)
      chunks.each do |block|
        @blocks << block.rstrip
        @keys << block_key(block)
      end
    end

    def parse_requires(header)
      line = header[/^#\s*requires:\s*(.+)$/, 1]
      (line ? line.split(",").map(&:strip) : []).to_set
    end

    # The shape_key of a stored block: skeleton of the line following its `query` header (queries are
    # single-line).
    def block_key(block)
      lines = block.lines.map(&:chomp)
      qi = lines.index { |l| l.start_with?("query ") }
      qi ? RQG.skeleton(lines[qi + 1]) : block
    end

    def render
      [*file_header, "# requires: #{@caps.to_a.sort.join(', ')}", "", @blocks.join("\n\n")].join("\n") + "\n"
    end

    def file_header
      ["# RQG #{@shape} corpus — GENERATED + curated by scripts/rqg_gen.rb (rake rqg:emit).",
       "# One self-contained {schema, data, query} block per distinct query shape; expected rows are",
       "# the live PostgreSQL oracle's (jed agreed by construction). Runs on all three cores in rake",
       "# ci. Do NOT hand-edit — regenerate. See .scratch/testing-ideas.md §1 (the RQG firehose)."]
    end
  end
end
