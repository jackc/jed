# frozen_string_literal: true

# scripts/oracle_import.rb — minimal PostgreSQL oracle-import harness (CLAUDE.md §7, TODO.md
# Phase 8). Fills a sqllogictest `.test`'s expected output from the LIVE `db` PostgreSQL
# service (.devcontainer/docker-compose.yml) — never the source checkout, so the §12
# reference-provisioning gate is sidestepped. psql-only: no `pg` gem, so no §14 dep decision.
#
# Model: skeleton-fill, NOT query-gen (that is the separate SQLancer item). The human authors
# the SQL; PG supplies the truth. For each query record we REGENERATE only the expected
# `----` block; every other byte (comments, headers, blank lines, the SQL itself) passes
# through untouched, so a re-run of an already-correct file is byte-identical iff jed == PG.
#
#   ruby scripts/oracle_import.rb --check  spec/conformance/suites/query/integers_basic.test
#   ruby scripts/oracle_import.rb          spec/conformance/suites/query/integers_basic.test  # writes
#
# What it CANNOT derive: `# cost:` (PG has no notion of jed's cost units) — those stay
# hand-authored. Where jed intentionally diverges from PG (the strict type system, the
# documented narrowings), an OVERRIDE SIDECAR (spec/conformance/oracle_overrides.toml) records
# jed's intended outcome + the reason; a matched record keeps its committed expected output and
# the importer stays silent instead of "correcting" jed toward PG. Everything else is flagged.

require "open3"
begin
  require "toml-rb"
rescue LoadError
  # Overrides are optional; without toml-rb the sidecar is simply empty.
end

class OracleImport
  # No -h here: honor the PGHOST env (the devcontainer points it at the shared Unix
  # socket directory, which is faster than the localhost TCP path). Socket connections
  # authenticate via the default `local all all trust` rule, so no password is needed.
  PSQL = %w[psql -U postgres -q -A -t -X -v ON_ERROR_STOP=0].freeze
  OVERRIDES = File.expand_path("../spec/conformance/oracle_overrides.toml", __dir__)

  # jed canonical type names -> the PG / SQL-standard spelling PG parses. A skeleton authored
  # in the common subset (smallint/integer/bigint) needs no rewrite; this lets us also replay
  # the existing corpus, which uses jed's canonical int16/int32/int64.
  TYPE_REWRITE = { "int16" => "smallint", "int32" => "integer", "int64" => "bigint" }.freeze

  # PG result-column type (from \gdesc) -> the conformance coltype tag (conformance.md §1).
  TAG = {
    "smallint" => "I", "integer" => "I", "bigint" => "I",
    "boolean" => "B", "numeric" => "D",
    "text" => "T", "character varying" => "T", "uuid" => "T", "bytea" => "T",
  }.freeze

  def initialize(path)
    @path = path
    @lines = File.readlines(path) # newlines retained
    @applied = []                 # PG-rewritten SQL of statements known to succeed
    @warnings = []
    @overrides = load_overrides
  end

  # Documented jed-vs-PG divergences for THIS file: a set of normalized SQL strings whose
  # committed expected output is jed's intended (divergent) answer, not PG's. Matched records
  # are left untouched and produce no warning. Reasons are kept for the `# diverges` note in
  # import mode and for human review.
  def load_overrides
    return {} unless defined?(TomlRB) && File.exist?(OVERRIDES)

    doc = TomlRB.load_file(OVERRIDES)
    (doc["override"] || []).each_with_object({}) do |o, h|
      next unless @path.end_with?(o["file"]) # `file` is a path suffix, e.g. "types/decimal.test"

      h[normalize(o["sql"])] = o["reason"]
    end
  end

  def normalize(sql) = sql.strip.gsub(/\s+/, " ").chomp(";")
  def overridden?(sql) = @overrides.key?(normalize(sql))

  # Returns the regenerated file as a String.
  def regenerate
    out = []
    i = 0
    while i < @lines.size
      line = @lines[i]
      case line
      when /^statement\s+ok\s*$/
        sql, i = take_sql(i + 1)
        out << line << sql.join
        verify_ok(sql)
        @applied << rewrite(sql.join)
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
          out.concat(regenerate_query(sql.join, coltypes, sortmode))
        end
      else
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

  def rewrite(sql)
    TYPE_REWRITE.reduce(sql) { |s, (jed, pg)| s.gsub(/\b#{jed}\b/, pg) }
  end

  # One isolated psql invocation: BEGIN, replay the successful prefix, run `body`, ROLLBACK.
  # stdout and stderr are captured SEPARATELY: psql writes errors to stderr (unbuffered) and
  # `\echo` sentinels to stdout (block-buffered), so a merged stream races them out of order.
  # Returns [stdout lines between the sentinels, full stderr string].
  def run(body, fieldsep: nil)
    args = PSQL.dup
    args += ["-F", fieldsep] if fieldsep
    script = +"BEGIN;\n"
    @applied.each { |s| script << s.rstrip << ";\n" unless s.strip.empty? }
    script << "\\echo @@@S\n" << body << "\n\\echo @@@E\n" << "ROLLBACK;\n"
    out, err, = Open3.capture3(*args, stdin_data: script)
    s = out.lines.index { |l| l.chomp == "@@@S" }
    e = out.lines.index { |l| l.chomp == "@@@E" }
    raise "psql sentinels missing for #{@path}:\n#{out}#{err}" unless s && e

    [out.lines[(s + 1)...e], err]
  end

  # The body must terminate the SQL statement itself; `run` only appends `\echo`/ROLLBACK
  # afterward, so an unterminated body would swallow `ROLLBACK` into a syntax error.
  def terminate(sql) = rewrite(sql.join).strip.chomp(";") + ";"

  def verify_ok(sql)
    return if overridden?(sql.join)

    _stdout, err = run(terminate(sql))
    bad = err[/^ERROR:.*/]
    @warnings << "expected `statement ok` but PG raised: #{bad.strip}\n  #{sql.join.strip}" if bad
  end

  def check_error(sql, declared)
    return if overridden?(sql.join) # documented divergence — PG's disagreement is expected

    _stdout, err = run("\\set VERBOSITY sqlstate\n" + terminate(sql))
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

  def regenerate_query(sql, declared_coltypes, sortmode)
    pg_sql = rewrite(sql).rstrip.chomp(";")

    # Type pass: `\gdesc` describes result columns without executing (default `|` fieldsep).
    # \gdesc reports the type WITH its typmod (`numeric(10,2)`); the tag is by base type.
    desc, = run("#{pg_sql} \\gdesc")
    types = desc.map { |l| n, t = l.chomp.split("|", 2); [n, t.sub(/\(.*\)\z/, "")] }
    tags = types.map { |(_name, t)| TAG[t] || raise("no coltype tag for PG type #{t.inspect}") }
    is_bool = types.map { |(_name, t)| t == "boolean" }
    derived = tags.join
    if derived != declared_coltypes
      @warnings << "coltype mismatch on `#{sql.strip}`: file says #{declared_coltypes}, " \
                   "oracle derives #{derived}"
    end

    # Value pass: fieldsep=newline gives row-major one-value-per-line; NULL is explicit.
    args_null = ["-P", "null=NULL"]
    out, = Open3.capture2e(*(PSQL + args_null + ["-F", "\n"]),
                           stdin_data: value_script(pg_sql))
    s = out.lines.index { |l| l.chomp == "@@@S" }
    e = out.lines.index { |l| l.chomp == "@@@E" }
    vals = out.lines[(s + 1)...e].map(&:chomp)

    ncol = tags.size
    body = vals.each_with_index.map do |v, k|
      is_bool[k % ncol] ? { "t" => "true", "f" => "false" }.fetch(v, v) : v
    end

    # `rowsort` does not pin row order, so the oracle's row order is not the contract — emit in
    # the harness's canonical order (rows sorted, NUL-joined) so an imported file is stable and
    # the cores' order-insensitive comparison still passes. `nosort` (ORDER BY) is left as-is.
    body = canonical_rowsort(body, ncol) if sortmode == "rowsort"
    body.map { |v| v + "\n" }
  end

  # Mirror the harness's rowsort: group the flat values into rows of `ncol`, sort rows by their
  # NUL-joined string (spec/conformance + impl/*/conformance applySort), then re-flatten.
  def canonical_rowsort(flat, ncol)
    ncol = 1 if ncol < 1
    flat.each_slice(ncol).sort_by { |row| row.join("\x00") }.flatten
  end

  def value_script(pg_sql)
    script = +"BEGIN;\n"
    @applied.each { |s| script << s.rstrip << ";\n" unless s.strip.empty? }
    script << "\\echo @@@S\n" << pg_sql << ";\n" << "\\echo @@@E\n" << "ROLLBACK;\n"
    script
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
