#!/usr/bin/env ruby
# frozen_string_literal: true

# scripts/dev.rb — the launcher behind `mise run dev`.
#
# Deliberately thin, and in this exact order:
#
#   1. ensure this checkout has a port allocation (and that nothing else is using it);
#   2. export it into the environment;
#   3. exec process-compose.
#
# The ordering is the point. mise loads the environment once, before a task runs, so a task that
# CHANGES .dev/ports.env cannot have the new values reach anything through mise's own env — the
# stale-value trap. Doing the ensure and the export inside one process, immediately before exec,
# means process-compose (and therefore every process it starts, since it passes its environment
# down) sees exactly the allocation that was just validated. No per-process `environment:` blocks
# in process-compose.yaml, and one source of truth.
#
# PC_PORT_NUM is not arbitrary: it is the variable process-compose itself reads for its control
# port, so every `process-compose ...` command in this checkout targets THIS checkout's instance
# with no flags.

require "fileutils"
# dev.rb itself needs no gems, but its children do. Requiring the bootstrap here fails fast with
# ONE actionable message on an un-bootstrapped checkout, rather than letting a child fail and
# reporting it second-hand.
require_relative "lib/bundle_setup"

ROOT = File.expand_path("..", __dir__)
PORTS_ENV = File.join(ROOT, ".dev", "ports.env")

system(RbConfig.ruby, File.join(__dir__, "devports.rb"), "ensure", out: File::NULL) or
  abort("dev: port allocation failed")

File.readlines(PORTS_ENV).each do |line|
  next if line.start_with?("#") || !line.include?("=")

  name, value = line.chomp.split("=", 2)
  ENV[name] = value
end

unless system("command -v process-compose > /dev/null 2>&1")
  abort <<~MSG
    dev: process-compose not found. It is a project tool, pinned in mise.toml:
      mise install
  MSG
end

# Ask devdb for the endpoint rather than reading PGHOST: in a devcontainer the ambient PGHOST
# points at the `db` compose service, which is not the cluster this stack supervises.
info = `#{RbConfig.ruby} #{File.join(__dir__, 'devdb.rb')} info`
# Check the child actually succeeded. Without this a failing devdb (missing gems, missing
# toolchain) yields empty output, and the mismatch check below then reports "the allocation
# disagrees with the cluster layout" — pointing at the wrong thing entirely, with the real error
# scrolled off above.
unless $?.success?
  abort "dev: could not read the cluster layout from devdb.rb (see the error above). " \
        "If this checkout has not been bootstrapped yet, run `mise run dev:init`."
end

db_socket, db_port = info.chomp.split("\t")

# PGHOST and PGPORT come from the allocation above (devports writes them as a pair, from the same
# DevPaths source devdb starts the server on), so the loop has already set them. Asserting it here
# rather than re-assigning keeps ONE source of truth and turns a drift between the two scripts into
# a loud failure instead of a connection to a socket that does not exist.
if ENV["PGHOST"] != db_socket || ENV["PGPORT"] != db_port
  abort <<~MSG
    dev: the allocation disagrees with the cluster layout.
      .dev/ports.env: PGHOST=#{ENV['PGHOST'].inspect} PGPORT=#{ENV['PGPORT'].inspect}
      devdb.rb info:  PGHOST=#{db_socket.inspect} PGPORT=#{db_port.inspect}
    Run `rake dev:ports:ensure` to regenerate the allocation.
  MSG
end
puts "  postgres  #{db_socket}  port #{db_port}  (rake db:psql)"
puts "  web dev   #{ENV['WEB_DEV_URL']} (disabled by default: process-compose process start web)"
puts "  control   127.0.0.1:#{ENV['PC_PORT_NUM']}"
puts
# exec replaces the process image WITHOUT running Ruby's at_exit or flushing its buffers. When
# stdout is a pipe rather than a TTY it is block-buffered, so everything above is silently lost —
# exactly where an agent or a CI log would need it most. Flush before handing the process over.
$stdout.flush

exec("process-compose", "up", *ARGV)
