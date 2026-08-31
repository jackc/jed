# frozen_string_literal: true

# scripts/lib/pg_bin.rb — locate the PostgreSQL SERVER binaries (initdb, postgres, pg_ctl).
#
# The client tools (psql, pg_isready) are on PATH on every platform we care about. The server
# tools usually are NOT, and for different reasons on each:
#
#   * Debian/Ubuntu (pgdg)  — postgresql-common puts version-multiplexing wrappers in /usr/bin for
#                             the CLIENT tools only; initdb/postgres/pg_ctl live in
#                             /usr/lib/postgresql/<major>/bin and are reached via pg_ctlcluster.
#   * macOS (Homebrew)      — postgresql@<major> is KEG-ONLY, so nothing is linked into the prefix
#                             bin; the tools sit in <brew prefix>/opt/postgresql@<major>/bin.
#
# So a portable `initdb` needs an explicit search rather than a bare exec. Order: an explicit
# PGBIN override, then PATH (a plain source build or a linked formula), then the two known layouts
# for the major version the oracle profile declares. Failing all of that we say what we looked for
# and how to install it, because "initdb: not found" is a uniquely unhelpful error.

require "rbconfig"

module PgBin
  module_function

  TOOLS = %w[initdb postgres pg_ctl].freeze

  # The directory holding the server binaries for `major`, or nil.
  def dir(major)
    explicit = ENV["PGBIN"]
    return explicit if explicit && !explicit.empty? && complete?(explicit)

    from_path = which("initdb")
    return File.dirname(from_path) if from_path && complete?(File.dirname(from_path))

    candidates(major).find { |d| complete?(d) }
  end

  # The path to one tool, aborting with an actionable message if the toolchain is missing.
  def tool(name, major)
    d = dir(major) or abort(missing_message(major))
    File.join(d, name)
  end

  def complete?(d) = TOOLS.all? { |t| File.executable?(File.join(d, t)) }

  def candidates(major)
    if RbConfig::CONFIG["host_os"] =~ /darwin/
      # `brew --prefix` is authoritative but slow to shell out to on every call; the two standard
      # prefixes (Apple silicon, Intel) cover any default install, and PGBIN covers the rest.
      %W[/opt/homebrew/opt/postgresql@#{major}/bin /usr/local/opt/postgresql@#{major}/bin]
    else
      %W[/usr/lib/postgresql/#{major}/bin /usr/pgsql-#{major}/bin]
    end
  end

  def which(name)
    ENV.fetch("PATH", "").split(File::PATH_SEPARATOR).each do |d|
      candidate = File.join(d, name)
      return candidate if File.executable?(candidate) && !File.directory?(candidate)
    end
    nil
  end

  def missing_message(major)
    install =
      if RbConfig::CONFIG["host_os"] =~ /darwin/
        "brew install postgresql@#{major}"
      else
        "apt-get install postgresql-#{major}"
      end
    <<~MSG
      PostgreSQL #{major} server binaries not found (need: #{TOOLS.join(', ')}).
      Looked in: $PGBIN, $PATH, #{candidates(major).join(', ')}
      Install with: #{install}
      Or point PGBIN at the directory holding them.
    MSG
  end
end
