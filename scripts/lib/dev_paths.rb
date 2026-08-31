# frozen_string_literal: true

# scripts/lib/dev_paths.rb — the filesystem layout of a development checkout's instance-local
# state, in ONE place because several tools must agree on it exactly:
#
#   scripts/devenv.rb  writes PGHOST into .dev/derived.env
#   scripts/devdb.rb   starts the server on that socket and probes it
#   scripts/dev.rb     reports it
#
# If these three ever disagreed the failure would be a connection to a socket that does not exist,
# which is precisely the bug this module exists to make impossible.

require "digest"
require "rbconfig"

module DevPaths
  module_function

  ROOT = File.expand_path("../..", __dir__)
  DEV_DIR = File.join(ROOT, ".dev")
  # Two dotenv files, both loaded by mise (mise.toml [env]) — see scripts/devenv.rb for why they
  # are separate: port-tamer owns the first and accepts nothing but NAME=<port> in it.
  PORTS_ENV = File.join(DEV_DIR, "ports.env")     # port-tamer's state: the port assignments
  DERIVED_ENV = File.join(DEV_DIR, "derived.env") # ours: values computed from them (PGHOST)
  PGDATA = File.join(DEV_DIR, "postgres", "data")

  # A Unix socket path is capped by sockaddr_un.sun_path — 104 bytes on macOS, 108 on Linux — and
  # PostgreSQL appends "/.s.PGSQL.<port>" to the directory. A checkout nested under a long home
  # directory blows that limit, and the resulting error points at nothing useful.
  SUN_PATH_MAX = RbConfig::CONFIG["host_os"] =~ /darwin/ ? 104 : 108

  # The directory PostgreSQL puts its Unix socket in: inside .dev when it fits, otherwise a short
  # deterministic /tmp path so the same checkout always resolves to the same directory.
  def socket_dir(port)
    preferred = File.join(DEV_DIR, "postgres", "run")
    return preferred if preferred.bytesize + "/.s.PGSQL.#{port}".bytesize < SUN_PATH_MAX

    File.join("/tmp", "jed-pg-#{Digest::SHA256.hexdigest(ROOT)[0, 12]}")
  end
end
