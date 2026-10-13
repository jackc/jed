# frozen_string_literal: true

# scripts/sim.rb — the deterministic-simulation driver (spec/design/simulation-testing.md §10–§11).
#
# Generates traces (scripts/sim_gen.rb), runs them on each core's simulation runner, and fails on
# any oracle violation or any cross-core transcript disagreement. A failing trace can be shrunk to a
# minimal one for spec/conformance/sim/regress/.
#
#   ruby scripts/sim.rb sweep [--profile P|all] [--seeds 1..200] [--steps N] [--cores go,rust,ts]
#   ruby scripts/sim.rb regress [--cores go,rust,ts]          # the committed regression corpus
#   ruby scripts/sim.rb replay TRACE.sim [--cores ...]         # rerun one trace, print transcripts
#   ruby scripts/sim.rb shrink TRACE.sim --core go [-o OUT]    # ddmin over steps
#
# Results land in bench/results/sim/<stamp>/: every trace, each core's transcript, and a crash
# image for a failed recovery. The mise tasks sim:sweep / sim:regress / sim:replay wrap this.

require "etc"
require "fileutils"
require "json"
require "open3"
require "optparse"
require "time"
require_relative "sim_gen"

module Sim
  REPO = File.expand_path("..", __dir__)
  REGRESS_DIR = File.join(REPO, "spec/conformance/sim/regress")
  ALL_CORES = %w[go rust ts].freeze

  # Each core's runner: how to build it once and how to invoke it. Runners read JED_SIM_IN (a .sim
  # file, a directory, or a list file) and write transcripts + a results JSONL under JED_SIM_OUT.
  RUNNERS = {
    "go" => {
      build: [%w[go test -c -o bin/sim.test .], File.join(REPO, "impl/go")],
      run: [["./bin/sim.test", "-test.run", "^TestSimRunner$", "-test.count=1"], File.join(REPO, "impl/go")]
    },
    "rust" => {
      build: [%w[cargo build --release --quiet --features sim --bin sim], File.join(REPO, "impl/rust")],
      run: [["target/release/sim"], File.join(REPO, "impl/rust")]
    },
    "ts" => {
      build: nil,
      run: [["node", "src/bin/sim.ts"], File.join(REPO, "impl/ts")]
    }
  }.freeze

  module_function

  def build(cores)
    cores.each do |core|
      cmd, dir = RUNNERS.fetch(core)[:build]
      next unless cmd

      _out, err, st = Open3.capture3(*cmd, chdir: dir)
      abort "sim: building the #{core} runner failed:\n#{err}" unless st.success?
    end
  end

  # Run every trace in `list` (a file of paths) on `core`, split across `jobs` processes. Returns
  # { trace name => result hash }.
  def run_core(core, paths, out, jobs: 1, images: false)
    cmd, dir = RUNNERS.fetch(core)[:run]
    chunks = paths.each_slice([(paths.size.to_f / jobs).ceil, 1].max).to_a
    threads = chunks.each_with_index.map do |chunk, i|
      Thread.new do
        list = File.join(out, "#{core}.list.#{i}")
        File.write(list, chunk.join("\n") + "\n")
        env = { "JED_SIM_IN" => list, "JED_SIM_OUT" => out,
                "JED_SIM_RESULTS" => File.join(out, "#{core}.results.#{i}.jsonl") }
        env["JED_SIM_IMAGES"] = "1" if images
        _o, e, _st = Open3.capture3(env, *cmd, chdir: dir)
        [i, e]
      end
    end
    threads.each(&:join)
    results = {}
    Dir[File.join(out, "#{core}.results.*.jsonl")].each do |f|
      File.readlines(f, chomp: true).reject(&:empty?).each do |line|
        r = JSON.parse(line)
        results[r["trace"]] = r
      end
    end
    results
  end

  # A trace with a per-core fault (simulation-testing.md §6.3) is checked by every core's oracles but
  # not compared across cores: whether a read or a file growth happens at all is host policy.
  def per_core?(path)
    File.foreach(path).any? { |line| line.start_with?("fault read_error", "fault enospc") }
  end

  def transcript(out, name, core)
    path = File.join(out, "#{name}.#{core}.jsonl")
    File.exist?(path) ? File.read(path) : nil
  end

  # Run `paths` on `cores` and report. Returns the list of failure strings. With `images`, every
  # recovery's image is also decoded by the independent checker (check_images).
  def run_and_compare(paths, cores, out, jobs:, images: false)
    results = cores.to_h { |core| [core, run_core(core, paths, out, jobs: jobs, images: images)] }
    failures = []
    paths.each do |path|
      name = File.basename(path, ".sim")
      cores.each do |core|
        r = results[core][name]
        if r.nil?
          failures << "#{name} [#{core}]: the runner produced no result (crashed?)"
        elsif r["status"] != "pass"
          failures << "#{name} [#{core}]: #{r['violation']}"
        end
      end
      transcripts = cores.to_h { |core| [core, transcript(out, name, core)] }.compact
      next if transcripts.values.uniq.size <= 1
      next if per_core?(path)

      a, b = transcripts.keys.first(2).map { |c| [c, transcripts[c].lines] }
      diff = a[1].each_with_index.find { |line, i| line != b[1][i] }
      at = diff ? "first difference at transcript line #{diff[1] + 1}:\n      #{a[0]}: #{diff[0].strip}\n      #{b[0]}: #{b[1][diff[1]]&.strip}" : "lengths differ"
      failures << "#{name}: CROSS-CORE TRANSCRIPT DISAGREEMENT (#{transcripts.keys.join(', ')}) — #{at}"
    end
    failures.concat(check_images(paths, cores.first, out)) if images
    failures
  end

  # The independent fourth voice (simulation-testing.md §12, S6): spec/fileformat/verify.rb decodes
  # every image a recovery opened and must select the same generation — version, live pages, page
  # count, and free-list size — that the core's transcript reports. Equal transcripts mean equal
  # images across cores, so one core's images suffice.
  def check_images(paths, core, out)
    expected = {}
    paths.each do |path|
      name = File.basename(path, ".sim")
      text = transcript(out, name, core) or next
      text.each_line do |line|
        j = JSON.parse(line)
        next unless j["op"] == "crash"

        img = File.join(out, "#{name}.#{core}.step#{j['i']}.img")
        expected[img] = j if File.exist?(img)
      end
    end
    return [] if expected.empty?

    failures = []
    expected.keys.each_slice(200) do |batch|
      stdout, _err, _st = Open3.capture3("ruby", File.join(REPO, "spec/fileformat/verify.rb"), "--check-image", *batch)
      stdout.each_line do |line|
        r = JSON.parse(line)
        want = expected[r["file"]]
        label = "#{File.basename(r['file'], '.img')}"
        if r["error"]
          failures << "#{label}: the independent checker rejects the image #{core} recovered: #{r['error']}"
          next
        end
        got = [r["version"], r["live"], r["pages"], r["free"]]
        have = [want["recovered"], want["live"], want["pages"], want["free"]]
        next if got == have

        failures << "#{label}: the independent checker selects version/live/pages/free #{got.join('/')}, " \
                    "#{core} recovered #{have.join('/')}"
      end
    end
    puts "sim: the independent checker decoded #{expected.size} crash image(s)"
    failures
  end

  def stamp_dir(kind)
    dir = File.join(REPO, "bench/results/sim", "#{Time.now.strftime('%Y%m%d-%H%M%S')}-#{kind}")
    FileUtils.mkdir_p(dir)
    dir
  end

  def parse_seeds(text)
    lo, hi = text.split("..").map(&:to_i)
    (lo..(hi || lo)).to_a
  end

  def sweep(opts)
    cores = opts[:cores]
    build(cores)
    out = opts[:out] || stamp_dir("sweep")
    profiles = opts[:profile] == "all" ? SimGen.profile_names : [opts[:profile]]
    paths = []
    profiles.each do |profile|
      opts[:seeds].each do |seed|
        path = File.join(out, "#{profile}-#{seed}.sim")
        File.write(path, SimGen.generate(profile, seed, steps: opts[:steps]))
        paths << path
      end
    end
    puts "sim: #{paths.size} traces (#{profiles.join(', ')}) on #{cores.join(', ')} → #{out}"
    failures = run_and_compare(paths, cores, out, jobs: opts[:jobs], images: opts[:images])
    report(failures, paths.size, out)
  end

  def regress(opts)
    paths = Dir[File.join(REGRESS_DIR, "*.sim")].sort
    if paths.empty?
      puts "sim: no regression traces in #{REGRESS_DIR}"
      return
    end
    build(opts[:cores])
    out = opts[:out] || stamp_dir("regress")
    puts "sim: #{paths.size} regression traces on #{opts[:cores].join(', ')}"
    report(run_and_compare(paths, opts[:cores], out, jobs: opts[:jobs], images: true), paths.size, out)
  end

  def replay(opts, trace)
    build(opts[:cores])
    out = opts[:out] || stamp_dir("replay")
    path = File.join(out, File.basename(trace))
    FileUtils.cp(trace, path) unless File.expand_path(trace) == File.expand_path(path)
    failures = run_and_compare([path], opts[:cores], out, jobs: 1)
    opts[:cores].each do |core|
      puts "--- #{core} transcript: #{File.join(out, "#{File.basename(path, '.sim')}.#{core}.jsonl")}"
    end
    report(failures, 1, out)
  end

  def report(failures, total, out)
    if failures.empty?
      puts "sim OK: #{total} trace(s) passed every oracle#{' and agree across cores' if total.positive?}"
      return
    end
    puts "\nsim FAILED (#{failures.size}):"
    failures.first(40).each { |f| puts "  - #{f}" }
    puts "  … #{failures.size - 40} more" if failures.size > 40
    puts "artifacts: #{out}"
    exit 1
  end

  # --- shrinking (simulation-testing.md §10) -----------------------------------------------------

  # The class of a violation: its message with the step/line location and any numbers removed, so a
  # shrunk trace that fails "the same way" at a different step still matches.
  def violation_class(msg)
    msg.to_s.sub(/\Astep \d+ \(line \d+\): /, "").gsub(/\d+/, "N").gsub(/[0-9a-f]{16}/, "H")[0, 80]
  end

  def run_one(core, header, steps, out, tag)
    path = File.join(out, "shrink-#{tag}.sim")
    File.write(path, (header + steps).join("\n") + "\n")
    Dir[File.join(out, "#{core}.results.*.jsonl")].each { |f| File.delete(f) }
    run_core(core, [path], out)[File.basename(path, ".sim")]
  end

  def shrink(opts, trace)
    core = opts[:core] or abort "sim shrink: --core is required"
    build([core])
    out = opts[:out_dir] || stamp_dir("shrink")
    lines = File.readlines(trace, chomp: true).reject { |l| l.strip.empty? }
    header = lines.take_while { |l| l.start_with?("#") }
    steps = lines.drop(header.size)
    base = run_one(core, header, steps, out, "base")
    abort "sim shrink: the trace passes on #{core}; nothing to shrink" if base.nil? || base["status"] == "pass"
    target = violation_class(base["violation"])
    puts "sim shrink: #{steps.size} steps, target #{target.inspect}"
    tries = 0
    fails = lambda do |candidate|
      tries += 1
      r = run_one(core, header, candidate, out, "try")
      r && r["status"] == "fail" && violation_class(r["violation"]) == target
    end
    steps = ddmin(steps, fails)
    result = (header + steps).join("\n") + "\n"
    puts "sim shrink: #{steps.size} steps after #{tries} runs"
    if opts[:output]
      File.write(opts[:output], result)
      puts "wrote #{opts[:output]}"
    else
      print result
    end
  end

  # Zeller & Hildebrandt's ddmin over a list: the smallest subsequence for which `fails` holds.
  def ddmin(items, fails)
    n = 2
    while items.size >= 2
      chunk = (items.size.to_f / n).ceil
      subsets = items.each_slice(chunk).to_a
      reduced = false
      subsets.each_with_index do |_, i|
        complement = subsets.each_with_index.reject { |_, j| j == i }.flat_map(&:first)
        next unless fails.call(complement)

        items = complement
        n = [n - 1, 2].max
        reduced = true
        break
      end
      next if reduced
      break if n >= items.size

      n = [n * 2, items.size].min
    end
    items
  end
end

if $PROGRAM_NAME == __FILE__
  command = ARGV.shift
  opts = { profile: "mixed", seeds: (1..50).to_a, steps: nil, cores: Sim::ALL_CORES.dup,
           jobs: [Etc.nprocessors, 8].min }
  parser = OptionParser.new do |o|
    o.banner = "usage: ruby scripts/sim.rb sweep|regress|replay TRACE|shrink TRACE [options]"
    o.on("--profile NAME", "a profile, or `all` (#{SimGen.profile_names.join(', ')})") { |v| opts[:profile] = v }
    o.on("--seeds RANGE", "e.g. 1..200") { |v| opts[:seeds] = Sim.parse_seeds(v) }
    o.on("--steps N", Integer) { |v| opts[:steps] = v }
    o.on("--cores LIST", "comma-separated subset of go,rust,ts") { |v| opts[:cores] = v.split(",") }
    o.on("--core NAME", "the core to shrink against") { |v| opts[:core] = v }
    o.on("--jobs N", Integer, "runner processes per core") { |v| opts[:jobs] = v }
    o.on("--out DIR", "results directory") { |v| opts[:out] = v }
    o.on("-o FILE", "shrink: write the minimal trace here") { |v| opts[:output] = v }
    o.on("--check-images", "sweep: decode every crash image with spec/fileformat/verify.rb too") { opts[:images] = true }
  end
  parser.parse!
  bad = opts[:cores] - Sim::ALL_CORES
  abort "sim: unknown core(s) #{bad.join(', ')}" unless bad.empty?
  case command
  when "sweep" then Sim.sweep(opts)
  when "regress" then Sim.regress(opts)
  when "replay" then Sim.replay(opts, ARGV.shift || abort(parser.banner))
  when "shrink" then Sim.shrink(opts, ARGV.shift || abort(parser.banner))
  else abort parser.banner
  end
end
