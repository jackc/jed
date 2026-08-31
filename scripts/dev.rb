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
db_socket, db_port = `#{RbConfig.ruby} #{File.join(__dir__, 'devdb.rb')} info`.chomp.split("\t")

# Point PG* at THIS checkout's cluster for everything the stack starts. Exporting PGPORT (from
# ports.env) while leaving PGHOST at the ambient value is not a half-measure, it is a wrong one:
# the pair would name a port on the wrong server, and libpq would look for a socket that cannot
# exist. Either both move or neither does. Processes outside the stack keep the ambient settings.
ENV["PGHOST"] = db_socket
ENV["PGPORT"] = db_port
puts "  postgres  #{db_socket}  port #{db_port}  (rake db:psql)"
puts "  web dev   #{ENV['WEB_DEV_URL']} (disabled by default: process-compose process start web)"
puts "  control   127.0.0.1:#{ENV['PC_PORT_NUM']}"
puts
# exec replaces the process image WITHOUT running Ruby's at_exit or flushing its buffers. When
# stdout is a pipe rather than a TTY it is block-buffered, so everything above is silently lost —
# exactly where an agent or a CI log would need it most. Flush before handing the process over.
$stdout.flush

exec("process-compose", "up", *ARGV)
