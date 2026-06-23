# frozen_string_literal: true

require_relative "lib/jed/version"

Gem::Specification.new do |spec|
  spec.name = "jed"
  spec.version = Jed::VERSION
  spec.summary = "Embedded SQL database with PostgreSQL behavior and a strict, static type system."
  spec.description = <<~DESC
    The Ruby binding for jed, an embeddable single-file SQL database. The gem wraps the safe Rust
    core (CLAUDE.md §2/§13): the engine runs at Rust speed and conforms by construction. SQL-first,
    PostgreSQL behavior, exact decimals, three-valued NULL logic. See spec/design/ruby.md.
  DESC
  spec.authors = ["Jack Christensen"]
  spec.email = ["jack@jncsoftware.com"]
  # No license / homepage asserted: the jed project declares none yet (no LICENSE file, no license
  # in any core manifest). Set these when the project picks a license, before any public publish.
  spec.required_ruby_version = ">= 3.2"

  # Pure-Ruby sources + the native-extension Rust crate. The compiled cdylib is NOT listed here:
  # in-repo it is built by `rake ruby:build` and loaded from ext/target/release; producing a
  # distributable gem that builds or bundles the cdylib on install (rb-sys / precompiled platform
  # gems) is the packaging follow-on (spec/design/ruby.md §6).
  spec.files =
    Dir["lib/**/*.rb"] +
    Dir["ext/**/*.{rs,toml}"] +
    %w[README.md]
  spec.require_paths = ["lib"]

  spec.metadata = {
    "rubygems_mfa_required" => "true",
  }
end
