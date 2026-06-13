# frozen_string_literal: true

# scripts/reduce.rb — automatic test-case reducer for sqllogictest `.test` files
# (CLAUDE.md §7/§10; spec/design/conformance.md §8).
#
# A metamorphic sweep (scripts/norec_gen.rb) or a hand-written test can FAIL with a big, noisy
# file — dozens of records, most of them irrelevant to the actual bug. This shrinks it to a
# MINIMAL `.test` that still fails IDENTICALLY, ready to commit as a regression entry. It is the
# "reduce a discovered failure" half of the SQLancer loop: the generator discovers, the reducer
# distills, the committed `.test` is the durable artifact.
#
# Algorithm: delta-debugging (Zeller & Hildebrandt's ddmin) over the file's RECORDS (the
# blank-line-separated statement/query blocks; the `# requires:` header is fixed and always kept).
# A subset is accepted iff the chosen core's harness still reports the **byte-identical failure
# signature** — the `FAIL <file>: …` block (the message + SQL + expected/actual). That strict
# oracle is what makes record-removal safe even though each record carries hard-coded expected
# rows: dropping the CREATE makes the failing query error (different message → rejected), dropping
# the INSERT changes its `actual:` (different block → rejected), so prerequisites are retained
# automatically and only records that don't affect the failure are stripped. The result is the
# minimal {CREATE, INSERT, failing query} (plus any state-changing record the failure depends on).
#
# It does NOT minimize within a record (shrink the INSERT's rows or the failing SQL): that would
# change the expected/actual and thus the signature. Record-granularity reduction is the safe,
# generic win; finer trimming is left to the author. The reduced file keeps the original
# `# requires:` line verbatim (over-requiring is harmless — the file still runs); trim it by hand
# if you want it minimal too.
#
# Usage:
#   ruby scripts/reduce.rb FAILING.test                 # reduce against the rust core; print to stdout
#   ruby scripts/reduce.rb FAILING.test --core go       # reduce against the go (or ts) core
#   ruby scripts/reduce.rb FAILING.test -o MIN.test     # write the minimal file instead of stdout
#   ruby scripts/reduce.rb FAILING.test --keep          # leave the temp candidate file for inspection
#
# Typical flow — discover with the generator, then reduce:
#   ruby scripts/norec_gen.rb 7 --keep                  # writes suites/metamorphic/_norec_*_seed7.test
#   ruby scripts/reduce.rb spec/conformance/suites/metamorphic/_norec_tlp_seed7.test -o regression.test

require "open3"
require "fileutils"

REPO = File.expand_path("..", __dir__)

# The candidate is written INTO the suites tree so the (unmodified) harness picks it up on its
# normal walk; we read back only this file's PASS/FAIL/SKIP line. Always removed on exit.
TMP_REL = "_reduce_tmp/candidate.test"
TMP_DIR = File.join(REPO, "spec/conformance/suites/_reduce_tmp")
TMP_PATH = File.join(TMP_DIR, "candidate.test")

# Build the chosen core's harness once and return how to invoke it. rust/go run a prebuilt binary
# (the harness is ~40 ms, so ddmin's dozens of oracle calls stay fast); ts runs via npm. The
# harness locates spec/conformance/suites itself (the rust binary via CARGO_MANIFEST_DIR, go/ts by
# walking up from the run dir), so the candidate at TMP_REL is found regardless of core.
def prepare_core(name)
  case name
  when "rust"
    ok = system(*%w[cargo build --quiet --bin conformance], chdir: File.join(REPO, "impl/rust"))
    abort "reduce: rust harness build failed" unless ok
    { cmd: [File.join(REPO, "impl/rust/target/debug/conformance")], dir: REPO }
  when "go"
    bin = File.join(REPO, "impl/go", ".reduce_conformance")
    ok = system(*%W[go build -o #{bin} ./cmd/conformance], chdir: File.join(REPO, "impl/go"))
    abort "reduce: go harness build failed" unless ok
    at_exit { FileUtils.rm_f(bin) }
    { cmd: [bin], dir: File.join(REPO, "impl/go") }
  when "ts"
    { cmd: %w[npm run --silent conformance], dir: File.join(REPO, "impl/ts") }
  else
    abort "reduce: unknown core #{name.inspect} (have: rust, go, ts)"
  end
end

# Run the harness and return the candidate file's status block: the trailing array is
# [status, detail] where status is "PASS"/"FAIL"/"SKIP"/nil and detail is the full multi-line
# `FAIL …` text (the failure signature) for a FAIL, else the single status line.
def harness_status(core)
  out, = Open3.capture2e(*core[:cmd], chdir: core[:dir])
  lines = out.lines
  start = lines.index { |l| l =~ /\A(PASS|FAIL|SKIP)\s+#{Regexp.escape(TMP_REL)}(\s|:|\z)/ }
  return [nil, out] unless start # candidate not run at all — surface the whole output

  status = lines[start][/\A(PASS|FAIL|SKIP)/, 1]
  block = [lines[start]]
  # A FAIL's detail (SQL/expected/actual) spills onto following lines until the next file's
  # status line or the trailing "N passed, …" summary.
  lines[(start + 1)..].each do |l|
    break if l =~ /\A(PASS|FAIL|SKIP)\s/ || l =~ /\A\d+ passed,/
    block << l
  end
  [status, block.join]
end

# Parse a .test into [header_paragraphs, record_paragraphs]. Records are the blank-line-separated
# blocks; the header is every paragraph up to and including the one carrying `# requires:` (always
# retained, so the reduced file still gates + runs). A paragraph is a run of non-blank lines.
def parse(text)
  paras = text.split(/\n[ \t]*\n+/).map(&:rstrip).reject(&:empty?)
  hidx = paras.index { |p| p.lines.any? { |l| l.strip.start_with?("#") && l.sub(/\A\s*#\s*/, "").start_with?("requires:") } }
  abort "reduce: no `# requires:` header found in #{ARGV[0]}" unless hidx

  [paras[0..hidx], paras[(hidx + 1)..] || []]
end

# Reassemble header + chosen records into a .test (blank-line-separated paragraphs).
def assemble(header, records)
  (header + records).join("\n\n") + "\n"
end

# ddmin (Zeller & Hildebrandt): minimize the index list `c` while `test.(subset)` stays true.
# Standard two-phase scheme — try each 1/n subset, then each complement, then refine n.
def ddmin(c, &test)
  n = 2
  while c.size >= 2
    size = (c.size.to_f / n).ceil
    subsets = c.each_slice(size).to_a
    if (hit = subsets.find { |s| test.call(s) })
      c = hit
      n = 2
      next
    end
    comp = subsets.map { |s| c - s }.reject(&:empty?).find { |cand| test.call(cand) }
    if comp
      c = comp
      n = [n - 1, 2].max
      next
    end
    break if n >= c.size

    n = [c.size, n * 2].min
  end
  c
end

# --- argument parsing -------------------------------------------------------------------------
args = ARGV.dup
keep = !args.delete("--keep").nil?
core_name = (i = args.index("--core")) ? args.delete_at(i + 1).tap { args.delete_at(i) } : "rust"
out_file = (i = args.index("-o")) ? args.delete_at(i + 1).tap { args.delete_at(i) } : nil
path = args.find { |a| !a.start_with?("-") }
abort "usage: ruby scripts/reduce.rb FAILING.test [--core rust|go|ts] [-o MIN.test] [--keep]" unless path
abort "reduce: no such file: #{path}" unless File.file?(path)

text = File.read(path)
header, records = parse(text)
abort "reduce: file has no reducible records (only a header)" if records.empty?

core = prepare_core(core_name)
FileUtils.mkdir_p(TMP_DIR)
calls = 0
begin
  # Baseline: the full file must FAIL on this core, giving us the signature to preserve.
  File.write(TMP_PATH, assemble(header, records))
  status, signature = harness_status(core)
  calls += 1
  case status
  when "PASS"
    abort "reduce: #{path} PASSES on the #{core_name} core — nothing to reduce (wrong --core?)."
  when "SKIP"
    abort "reduce: #{path} is SKIPPED on the #{core_name} core (it requires an undeclared capability)."
  when nil
    abort "reduce: candidate did not run on the #{core_name} core. Harness output:\n#{signature}"
  end
  warn "reduce: baseline FAIL on #{core_name} (#{records.size} records). Signature:"
  warn signature.lines.map { |l| "  | #{l}" }.join
  warn "reduce: minimizing…"

  memo = {}
  test = lambda do |idxs|
    memo.fetch(idxs) do
      File.write(TMP_PATH, assemble(header, idxs.map { |i| records[i] }))
      st, sig = harness_status(core)
      calls += 1
      memo[idxs] = (st == "FAIL" && sig == signature)
    end
  end

  min_idxs = ddmin((0...records.size).to_a, &test)
  reduced = assemble(header, min_idxs.map { |i| records[i] })

  warn "reduce: #{records.size} records → #{min_idxs.size} (#{calls} harness runs); failure signature preserved."
  if out_file
    File.write(out_file, reduced)
    warn "reduce: wrote #{out_file}"
  else
    puts reduced
  end
ensure
  unless keep
    FileUtils.rm_f(TMP_PATH)
    Dir.rmdir(TMP_DIR) if Dir.exist?(TMP_DIR) && Dir.empty?(TMP_DIR)
  end
end
