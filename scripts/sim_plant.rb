# frozen_string_literal: true

# scripts/sim_plant.rb — does the simulator still catch real storage bugs? (simulation-testing.md
# §14 S1, the exit criterion.)
#
# Each plant removes one protection the validated-COW protocol relies on from the Go core, sweeps the
# profile built to reach it, and restores the source. A plant no trace catches means the simulator
# has lost the ability to see that class of bug — a generator, fault model, or oracle regression.
# Like mutation testing (spec/design/mutation-testing.md) this is an analysis tool, outside `ci`:
#
#   ruby scripts/sim_plant.rb [SEEDS]        # e.g. 1..60 (the default)
#   ONLY=skip-stabilization ruby scripts/sim_plant.rb
#
# The source edits are exact string substitutions; a plant whose pattern no longer matches fails
# loudly so the list is kept in step with the code.

require_relative "sim"

module SimPlant
  REPO = File.expand_path("..", __dir__)
  HELPER = File.join(REPO, "impl/go/sim_plant_helper.go")
  HELPER_SOURCE = <<~GO
    package jed

    import "sort"

    // plantFree is sim_plant.rb's free-manifest-pages plant: it lets a commit's own manifest pages
    // onto the free list. Never committed.
    func plantFree(free, ids []uint32) []uint32 {
    	out := append(append([]uint32(nil), free...), ids...)
    	sort.Slice(out, func(a, b int) bool { return out[a] < out[b] })
    	return out
    }
  GO

  Plant = Struct.new(:name, :protection, :profile, :edits, keyword_init: true)

  PLANTS = [
    Plant.new(
      name: "skip-stabilization",
      protection: "a writer syncs an adopted, unacknowledged generation before its first write (validated-cow.md §3)",
      profile: "stabilization",
      edits: [["impl/go/validated_commit.go",
               "\tif p.recoverySync {\n\t\tif err := p.sync(); err != nil {",
               "\tif false && p.recoverySync {\n\t\tif err := p.sync(); err != nil {"]]
    ),
    Plant.new(
      name: "free-manifest-pages",
      protection: "the committed free list excludes the commit's own manifest pages (validated-cow.md §2)",
      profile: "big-commits",
      edits: [["impl/go/validated_commit.go",
               "meta: meta, free: free, pageCount: pc",
               "meta: meta, free: plantFree(free, ids), pageCount: pc"]]
    ),
    Plant.new(
      name: "skip-digest-check",
      protection: "recovery accepts a generation only if every dirty page matches its manifest digest (validated-cow.md §1)",
      profile: "all",
      edits: [["impl/go/validated_commit.go", " || commitCRC64(0, block) != binary.BigEndian.Uint64(e[4:])", ""]]
    ),
    # The reclaim and reuse gates are redundant defenses — disabling only one is masked by the other
    # (the free list a commit cannot reuse just grows) — so the plant removes both.
    Plant.new(
      name: "ignore-watermark",
      protection: "pages a pinned reader still needs are neither reclaimed nor reused (transactions.md §8)",
      profile: "reuse-under-pins",
      edits: [["impl/go/shared.go",
               "canReclaim := oldest == snap.txid\n\tcanReuse := oldest >= c.storage.freeGenTxid",
               "canReclaim := oldest <= snap.txid\n\tcanReuse := oldest >= 0"]]
    ),
    # Bounded growth (simulation-testing.md §8.3): a reader that never releases its pin holds
    # reclamation back for good, so the file grows without bound after every reader has closed.
    Plant.new(
      name: "leak-reader-pin",
      protection: "a closed reader releases its pin, so reclamation resumes (transactions.md §8)",
      profile: "growth",
      edits: [["impl/go/shared.go",
               "\tif c.live[v]--; c.live[v] <= 0 {\n\t\tdelete(c.live, v)\n",
               "\tif c.live[v]--; false && c.live[v] <= 0 {\n\t\tdelete(c.live, v)\n"]]
    ),
    Plant.new(
      name: "never-reuse",
      protection: "a commit reuses the free list before it grows the file (storage.md §6)",
      profile: "growth",
      edits: [["impl/go/shared.go",
               "canReuse := oldest >= c.storage.freeGenTxid",
               "canReuse := false && oldest >= c.storage.freeGenTxid"]]
    ),
    # File attachments (S4): an attachment's commit is gated by the same reader watermark as main's.
    Plant.new(
      name: "attachment-ignores-readers",
      protection: "an attachment's pages a pinned reader still needs are neither reclaimed nor reused (attached-databases.md §5)",
      profile: "attachments",
      edits: [["impl/go/execute.go",
               "canReclaim := db.core == nil || !db.core.hasLiveReaders()",
               "canReclaim := true || db.core == nil || !db.core.hasLiveReaders()"]]
    ),
    # Corruption (§6.3 `corrupt`): a page whose checksum no longer matches must never be decoded.
    Plant.new(
      name: "skip-page-checksum",
      protection: "a body page whose CRC does not match is XX001, never decoded (storage.md §6)",
      profile: "corruption",
      edits: [["impl/go/format.go",
               "\tif pageCRC(block) != binary.BigEndian.Uint32(block[12:16]) {\n\t\treturn page{}, newError(DataCorrupted, \"page checksum mismatch",
               "\tif false && pageCRC(block) != binary.BigEndian.Uint32(block[12:16]) {\n\t\treturn page{}, newError(DataCorrupted, \"page checksum mismatch"]]
    ),
  ].freeze

  module_function

  def run(seeds, only)
    Sim.build(["go"])
    rows = PLANTS.select { |p| only.nil? || p.name == only }.map { |plant| [plant, sweep_with(plant, seeds)] }
    puts
    rows.each do |plant, (failed, total)|
      puts format("  %-20s %-17s %4d / %-4d traces caught it", plant.name, plant.profile, failed, total)
    end
    missed = rows.select { |_, (failed, _)| failed.zero? }
    abort "\nsim:plant FAILED — no trace caught: #{missed.map { |p, _| p.name }.join(', ')}" unless missed.empty?
    puts "\nsim:plant OK: every planted bug was caught"
  end

  # Apply the plant, sweep its profile on Go, restore the source. Returns [failing traces, total].
  def sweep_with(plant, seeds)
    originals = plant.edits.map { |file, _, _| [file, File.read(File.join(REPO, file))] }.to_h
    plant.edits.each do |file, from, to|
      path = File.join(REPO, file)
      text = File.read(path)
      abort "sim:plant: #{plant.name}: pattern not found in #{file} — update scripts/sim_plant.rb" unless text.include?(from)
      File.write(path, text.sub(from, to))
    end
    File.write(HELPER, HELPER_SOURCE)
    puts "#{plant.name}: removes the guarantee that #{plant.protection}"
    Sim.build(["go"])
    out = Sim.stamp_dir("plant-#{plant.name}")
    profiles = plant.profile == "all" ? SimGen.profile_names : [plant.profile]
    paths = profiles.flat_map do |profile|
      seeds.map do |seed|
        path = File.join(out, "#{profile}-#{seed}.sim")
        File.write(path, SimGen.generate(profile, seed))
        path
      end
    end
    results = Sim.run_core("go", paths, out, jobs: [Etc.nprocessors, 8].min)
    failed = results.values.count { |r| r["status"] != "pass" }
    first = results.values.find { |r| r["status"] != "pass" }
    puts "  first: #{first['trace']}: #{first['violation'][0, 160]}" if first
    [failed, paths.size]
  ensure
    originals&.each { |file, text| File.write(File.join(REPO, file), text) }
    File.delete(HELPER) if File.exist?(HELPER)
    Sim.build(["go"])
  end
end

if $PROGRAM_NAME == __FILE__
  seeds = Sim.parse_seeds(ARGV.shift || "1..60")
  SimPlant.run(seeds, ENV["ONLY"])
end
