#!/usr/bin/env ruby
# frozen_string_literal: true

# vendor_collations.rb — distribute the committed `.coll` artifacts (the collation tables the
# build-time pipeline produced, spec/design/collation.md §9) into each core that needs an in-tree
# copy to embed. In the reference-only model every core EMBEDS the identical `.coll` bytes
# (cross-core byte-identity, §9/§10) and reads them at startup; the database file only references a
# collation by name.
#
# Why copies at all: Rust embeds the spec files directly (`include_bytes!` accepts `../`), but Go's
# `//go:embed` and the no-build-step TS core cannot reach outside their own package tree, so they
# need a synced copy. This script is that sync (and `rake codegen`'s collation step); `--check`
# is the CI drift gate (wired into `rake verify`). The source of truth stays spec/collation/fixtures.
#
# Usage:
#   ruby scripts/vendor_collations.rb            # copy spec → each core's embed dir
#   ruby scripts/vendor_collations.rb --check    # verify copies match the source (exit 1 on drift)

require "fileutils"
require "digest"

ROOT = File.expand_path("..", __dir__)
SRC  = File.join(ROOT, "spec/collation/fixtures")

# The dev fixture set vendored today (collation.md §14, slice 2a). The real version-pinned DUCET +
# curated tailorings and the embedder-chosen footprint tiers (§13) replace this in a later slice.
COLLATIONS = ["dev-root.coll", "dev-nordic.coll"].freeze

# Cores that need an in-tree copy. Rust is absent on purpose — it `include_bytes!`es spec/ directly,
# so it is always in sync with the source.
TARGETS = [
  "impl/go/collationdata",
  "impl/ts/src/collationdata",
].freeze

check = ARGV.include?("--check")
drift = []

TARGETS.each do |rel|
  dir = File.join(ROOT, rel)
  COLLATIONS.each do |name|
    src = File.join(SRC, name)
    dst = File.join(dir, name)
    abort "vendor_collations: missing source #{src}" unless File.exist?(src)
    if check
      unless File.exist?(dst) && Digest::SHA256.file(dst) == Digest::SHA256.file(src)
        drift << File.join(rel, name)
      end
    else
      FileUtils.mkdir_p(dir)
      FileUtils.cp(src, dst)
    end
  end
end

if check
  unless drift.empty?
    warn "vendored collations out of sync with spec/collation/fixtures:"
    drift.each { |f| warn "  #{f}" }
    warn "run: ruby scripts/vendor_collations.rb"
    exit 1
  end
  puts "vendored collations in sync (#{TARGETS.size} cores × #{COLLATIONS.size} files)"
else
  puts "vendored #{COLLATIONS.size} collation(s) into #{TARGETS.size} core(s)"
end
