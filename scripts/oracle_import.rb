# frozen_string_literal: true

# scripts/oracle_import.rb — minimal PostgreSQL oracle-import harness (CLAUDE.md §7, TODO.md
# Phase 8). Fills a sqllogictest `.test`'s expected output from the LIVE `db` PostgreSQL
# service (.devcontainer/docker-compose.yml) — never the source checkout, so the §12
# reference-provisioning gate is sidestepped. psql-only: no `pg` gem, so no §14 dep decision.
#
# Model: skeleton-fill, NOT query-gen (that is scripts/rqg_gen.rb, the RQG firehose). The human
# authors the SQL; PG supplies the truth. For each query record we REGENERATE only the expected
# `----` block; every other byte (comments, headers, blank lines, the SQL itself) passes through
# untouched, so a re-run of an already-correct file is byte-identical iff jed == PG.
#
#   ruby scripts/oracle_import.rb --check  spec/conformance/suites/query/integers_basic.test
#   ruby scripts/oracle_import.rb          spec/conformance/suites/query/integers_basic.test  # writes
#
# The psql mechanics (the PSQL invocation, jed→PG rewrite, the \gdesc + value passes, rowsort) live
# in scripts/lib/pg_oracle.rb, shared verbatim with the RQG firehose; the divergence ledger lives in
# scripts/lib/oracle_overrides.rb. This file is now just the file-walker + the import-specific
# warnings (coltype mismatch, PG-cannot-describe, statement ok/error verification).
#
# What it CANNOT derive: `# cost:` (PG has no notion of jed's cost units) — those stay
# hand-authored. Where jed intentionally diverges from PG, an OVERRIDE SIDECAR
# (spec/conformance/oracle_overrides.toml) records jed's intended outcome + the reason; a matched
# record keeps its committed expected output and the importer stays silent. Everything else is flagged.

require_relative "lib/pg_oracle"
require_relative "lib/oracle_overrides"

class OracleImport
  def initialize(path)
    @path = path
    @lines = File.readlines(path) # newlines retained
    @oracle = PgOracle.new(label: @path) # the psql engine: replay prefix + session zone live here
    @ledger = OracleOverrides.new(path: @path)
    @warnings = []
    @next_tz = nil # a pending `# timezone:` set by a comment, consumed by the next record
  end

  def normalize(sql) = @ledger.normalize(sql)
  def overridden?(sql) = @ledger.overridden?(sql)

  # Returns the regenerated file as a String.
  def regenerate
    out = []
    i = 0
    while i < @lines.size
      line = @lines[i]
      # Consume any pending `# timezone:` directive for THIS record (per-record, like jed's harness):
      # the session zone PG decomposes a timestamptz in. Set to UTC otherwise so a directive never
      # leaks forward (timezones.md §9.4). Records that are comments/blank don't consume it.
      record_start = line =~ /^(statement|query)\s/
      if record_start
        @oracle.tz = @next_tz || "UTC"
        @next_tz = nil
      end
      case line
      when /^statement\s+ok\s*$/
        sql, i = take_sql(i + 1)
        out << line << sql.join
        verify_ok(sql)
        @oracle.add_applied(sql.join)
      when /^statement\s+error\s+(\S+)/
        declared = Regexp.last_match(1)
        sql, i = take_sql(i + 1)
        out << line << sql.join
        check_error(sql, declared)
      when /^query\s+(\S+)\s+(\S+)/
        coltypes = Regexp.last_match(1)
        sortmode = Regexp.last_match(2)
        sql, j = take_until_separator(i + 1)         # SQL up to the `----`
        old, i = take_sql(j + 1)                     # committed expected block
        out << line << sql.join << @lines[j]         # header + SQL + the `----` line
        # A documented divergence keeps jed's committed rows; otherwise PG supplies them.
        if overridden?(sql.join)
          out.concat(old)
        else
          out.concat(regenerate_query(sql.join, coltypes, sortmode, old))
        end
        # A row-returning DML query record (INSERT/UPDATE/DELETE ... RETURNING — grammar.md §32)
        # mutates state like a `statement ok`, so its effects must reach later records' replay
        # prefix; SELECTs stay un-replayed. A data-modifying WITH (writable-cte.md) is equally a
        # write. Appended unconditionally, like `statement ok`: an overridden PG-failing DML query
        # record must sit LAST (conformance.md §5).
        joined = sql.join
        is_dml_query = joined =~ /\A\s*(insert|update|delete)\b/i ||
                       (joined =~ /\A\s*with\b/i &&
                        joined =~ /\b(insert\s+into|update\b[\s\S]*\bset\b|delete\s+from)\b/i)
        @oracle.add_applied(joined) if is_dml_query
      else
        # A `# timezone: <zone>` directive sets the session zone for the next record (timezones.md
        # §9.4); record it so `run` issues a matching `SET LOCAL TimeZone`. `# load-timezone:` is a
        # no-op for the oracle (PG ships the full IANA db natively). Other comments pass through.
        if line =~ /^#\s*timezone:\s*(\S+)/
          @next_tz = Regexp.last_match(1)
        end
        out << line # comment / blank / directive — pass through verbatim
        i += 1
      end
    end
    out.join
  end

  def warnings = @warnings

  private

  # Collect a record body: lines from `start` until a blank line or EOF (records are
  # blank-line separated). Returns [lines, index_of_terminator].
  def take_sql(start)
    j = start
    j += 1 while j < @lines.size && @lines[j] !~ /^\s*$/
    [@lines[start...j], j]
  end

  # Collect query SQL: lines until the `----` separator. Returns [lines, index_of_separator].
  def take_until_separator(start)
    j = start
    j += 1 while j < @lines.size && @lines[j] !~ /^----\s*$/
    [@lines[start...j], j]
  end

  def verify_ok(sql)
    return if overridden?(sql.join)

    _stdout, err = @oracle.run(@oracle.terminate(sql.join))
    bad = err[/^ERROR:.*/]
    @warnings << "expected `statement ok` but PG raised: #{bad.strip}\n  #{sql.join.strip}" if bad
  end

  def check_error(sql, declared)
    return if overridden?(sql.join) # documented divergence — PG's disagreement is expected

    _stdout, err = @oracle.run("\\set VERBOSITY sqlstate\n" + @oracle.terminate(sql.join))
    # The prefix is clean (all `statement ok`), so the LAST error on stderr is the target's.
    got = err.scan(/ERROR:\s+([0-9A-Za-z]{5})/).last&.first
    if got.nil?
      @warnings << "expected error #{declared} but PG did not raise one — add an override " \
                   "(true divergence): #{sql.join.strip}"
    elsif got != declared
      @warnings << "error-code divergence: declared #{declared}, PG says #{got} " \
                   "(override candidate): #{sql.join.strip}"
    end
  end

  def regenerate_query(sql, declared_coltypes, sortmode, old)
    pg_sql = @oracle.pg_sql(sql)

    # Type pass: `\gdesc` describes result columns without executing.
    types = @oracle.describe(pg_sql)
    # An empty description means PG could not even plan the query — almost always a jed extension
    # PG has no overload for (e.g. MIN/MAX over uuid or boolean). Keep the committed rows and flag.
    if types.empty?
      @warnings << "PG could not describe `#{sql.strip}` (no result columns — likely a jed " \
                   "extension PG lacks). Keeping the committed rows; add an oracle_overrides " \
                   "entry to document the divergence."
      return old
    end
    tags = types.map { |(_name, t)| @oracle.tag_for(t) }
    is_bool = types.map { |(_name, t)| t == "boolean" }
    derived = tags.join
    if derived != declared_coltypes
      @warnings << "coltype mismatch on `#{sql.strip}`: file says #{declared_coltypes}, " \
                   "oracle derives #{derived}"
    end

    # Value pass: row-major one-value-per-line; NULL explicit; bools normalized to true/false.
    body = @oracle.query_values(pg_sql, is_bool)

    # `rowsort` does not pin row order, so emit in the harness's canonical order (rows sorted,
    # NUL-joined) so an imported file is stable. `nosort` (ORDER BY) is left as-is.
    body = @oracle.canonical_rowsort(body, tags.size) if sortmode == "rowsort"
    body.map { |v| v + "\n" }
  end
end

if $PROGRAM_NAME == __FILE__
  check = ARGV.delete("--check")
  path = ARGV.shift or abort "usage: oracle_import.rb [--check] <file.test>"
  imp = OracleImport.new(path)
  regenerated = imp.regenerate
  imp.warnings.each { |w| warn "WARN: #{w}" }

  if check
    if regenerated == File.read(path)
      puts "OK: #{path} regenerated byte-identically from the PostgreSQL oracle"
    else
      puts "DIFF: #{path} differs from oracle regeneration"
      File.write("/tmp/__oracle.test", regenerated)
      system("diff", "-u", path, "/tmp/__oracle.test")
      exit 1
    end
  else
    File.write(path, regenerated)
    puts "wrote #{path}"
  end
end
