# frozen_string_literal: true

# scripts/lib/bundle_setup.rb — load the gems pinned in Gemfile.lock, from any working directory
# and from a script invoked directly rather than through Rake.
#
# The Rakefile has always done this. The standalone scripts did not, and got away with it only on
# a machine where an earlier `bundle install` happened to leave the gems on Ruby's default load
# path. On a fresh checkout that is not true, and the failure is a bare
#
#   cannot load such file -- toml-rb (LoadError)
#
# which names the gem rather than the missing bootstrap step. Pinning BUNDLE_GEMFILE explicitly
# also makes this independent of the working directory, which matters because process-compose and
# the launcher invoke these scripts from different places.

require_relative "dev_paths"

ENV["BUNDLE_GEMFILE"] ||= File.join(DevPaths::ROOT, "Gemfile")

begin
  require "bundler/setup"
rescue LoadError, StandardError => e
  abort <<~MSG
    Ruby dependencies are not installed — #{e.class}: #{e.message.lines.first&.strip}

    Bootstrap this checkout first:
      mise run dev:init
  MSG
end
