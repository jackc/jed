#!/usr/bin/env ruby
# frozen_string_literal: true

# scripts/devdb.rb — this checkout's own PostgreSQL cluster.
#
#   rake db:init      idempotent initdb into .dev/postgres/data
#   rake db:psql      psql against this cluster
#   rake db:reset     destroy the cluster and re-init (destructive; requires CONFIRM=yes)
#
#   devdb.rb serve    exec the foreground server   (what process-compose supervises)
#   devdb.rb ready    exec pg_isready against it   (process-compose's readiness probe)
#   devdb.rb info     print "<socket dir>\t<port>"  (so the launcher reports the REAL endpoint)
#
# `serve` and `ready` exist so process-compose.yaml contains no resolved paths: the binary
# location and the socket directory are computed HERE, once, and both the supervisor and the probe
# agree by construction. exec (not spawn) keeps process-compose supervising the real server.
#
# WHY a cluster per checkout rather than databases inside one server: destructive testing and
# resets stay local, two checkouts can run at once, and the layout matches the devcontainer model
# it replaces — one instance, one server, its own data. The cluster is instance-local state, so it
# lives under the gitignored .dev/ alongside the port allocation.
#
# The server runs in the FOREGROUND under process-compose (process-compose.yaml), not as a
# pg_ctl daemon: one supervisor owns start/stop/restart/logs for everything, and a readiness probe
# gates dependents instead of a sleep. `db:init` and `db:reset` are the one-shot half.

require "fileutils"
require_relative "lib/dev_paths"
require_relative "lib/pg_bin"
require_relative "lib/pg_oracle"

DEV_DIR = DevPaths::DEV_DIR
PGDATA = DevPaths::PGDATA
PORTS_ENV = DevPaths::PORTS_ENV

CLUSTER = PgOracle.profile.fetch("cluster")
MAJOR = CLUSTER.fetch("pg_major")

# The port comes from this checkout's allocation (port-tamer.toml, scripts/devenv.rb). No default:
# guessing would silently collide with another checkout, which is the exact failure the allocation
# prevents.
def pgport
  unless File.exist?(PORTS_ENV)
    abort "devdb: no port allocation — run `rake dev:ports:ensure` first (#{PORTS_ENV} is missing)."
  end

  File.read(PORTS_ENV)[/^PGPORT=(\d+)$/, 1] ||
    abort("devdb: PGPORT missing from #{PORTS_ENV} — re-run `rake dev:ports:ensure`.")
end


def initialized? = File.exist?(File.join(PGDATA, "PG_VERSION"))

def init
  if initialized?
    puts "  cluster exists: #{PGDATA}"
  else
    FileUtils.mkdir_p(File.dirname(PGDATA))
    initdb = PgBin.tool("initdb", MAJOR)
    # Match the oracle profile's locale so the cluster's template databases order text the way jed
    # does; `rake oracle:setup` then creates jed_oracle from template0 with the same settings.
    args = [initdb, "-D", PGDATA, "--encoding=#{CLUSTER.fetch('encoding')}", "-U", "postgres"]
    args += locale_args
    puts "  initdb #{PGDATA}"
    system(*args, out: File::NULL) or abort("devdb: initdb failed")
  end

  FileUtils.mkdir_p(DevPaths.socket_dir(pgport))
  puts "  data:   #{PGDATA}"
  puts "  socket: #{DevPaths.socket_dir(pgport)}"
  puts "  port:   #{pgport}"
end

def locale_args
  case CLUSTER.fetch("locale_provider")
  when "b" then ["--locale-provider=builtin", "--builtin-locale=#{CLUSTER.fetch('locale')}"]
  when "i" then ["--locale-provider=icu", "--icu-locale=#{CLUSTER.fetch('locale')}", "--locale=C.UTF-8"]
  else ["--locale=#{CLUSTER.fetch('locale')}"]
  end
end

# Listens on BOTH the Unix socket (jed's documented oracle path — faster, and what PGHOST points
# at) and loopback TCP (reachable by GUI tools and anything that cannot use a socket).
def server_argv
  [PgBin.tool("postgres", MAJOR), "-D", PGDATA, "-p", pgport, "-k", DevPaths.socket_dir(pgport), "-h", "127.0.0.1"]
end

case ARGV[0]
when "init" then init
when "serve"
  abort "devdb: cluster not initialized — run `rake db:init`" unless initialized?

  FileUtils.mkdir_p(DevPaths.socket_dir(pgport)) # a /tmp fallback dir can vanish between boots
  exec(*server_argv)
when "ready"
  exec("pg_isready", "-h", DevPaths.socket_dir(pgport), "-p", pgport, "-U", "postgres")
when "info"
  # The launcher must not report the ambient PGHOST: in a devcontainer that points at the `db`
  # compose service, which is NOT the cluster this stack runs. One definition, printed on request.
  puts "#{DevPaths.socket_dir(pgport)}\t#{pgport}"
when "psql"
  abort "devdb: cluster not initialized — run `rake db:init`" unless initialized?
  exec("psql", "-h", DevPaths.socket_dir(pgport), "-p", pgport, "-U", "postgres",
       "-d", CLUSTER.fetch("database"), *ARGV[1..])
when "reset"
  unless ENV["CONFIRM"] == "yes"
    abort "devdb: `db:reset` DESTROYS #{PGDATA}. Re-run with CONFIRM=yes if that is what you want."
  end
  puts "  removing #{PGDATA}"
  FileUtils.rm_rf(File.join(DEV_DIR, "postgres"))
  init
else
  abort("usage: devdb.rb init|serve|ready|info|psql|reset")
end
