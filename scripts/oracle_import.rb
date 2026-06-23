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
  # the existing corpus, which uses jed's canonical i16/i32/i64.
  # f64/f32 -> the SQL-standard spellings PG's ConstTypename grammar accepts in a
  # `typename 'literal'` const (double precision / real — float.md §2; float8/float4 are not in
  # that production). Only these two appear in the corpus; no other rewrite is needed.
  TYPE_REWRITE = {
    "i16" => "smallint", "i32" => "integer", "i64" => "bigint",
    "f64" => "double precision", "f32" => "real",
  }.freeze

  # PG result-column type (from \gdesc) -> the conformance coltype tag (conformance.md §1).
  TAG = {
    "smallint" => "I", "integer" => "I", "bigint" => "I",
    "boolean" => "B", "numeric" => "D",
    "double precision" => "R", "real" => "R", # the float types' tolerant render tag (float.md §9)
    "text" => "T", "character varying" => "T", "uuid" => "T", "bytea" => "T",
    "timestamp without time zone" => "T", "timestamp with time zone" => "T",
    "interval" => "T", "date" => "T",
    # Range types render as text (range_out). PG reports the canonical PG name; jed's i32range/
    # i64range are aliases of int4range/int8range, so both spellings tag T (spec/design/ranges.md §5).
    "int4range" => "T", "int8range" => "T", "numrange" => "T",
    "tsrange" => "T", "tstzrange" => "T", "daterange" => "T",
    # json/jsonb render as text (json_out / jsonb_out canonical form — spec/design/json.md §6.2).
    "json" => "T", "jsonb" => "T",
  }.freeze

  def initialize(path)
    @path = path
    @lines = File.readlines(path) # newlines retained
    @applied = []                 # PG-rewritten SQL of statements known to succeed
    @warnings = []
    @overrides = load_overrides
    @tz = "UTC"        # the session zone for the CURRENT record (the `# timezone:` directive)
    @next_tz = nil     # a pending `# timezone:` set by a comment, consumed by the next record
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
      # Consume any pending `# timezone:` directive for THIS record (per-record, like jed's harness):
      # the session zone PG decomposes a timestamptz in. Set to UTC otherwise so a directive never
      # leaks forward (timezones.md §9.4). Records that are comments/blank don't consume it.
      record_start = line =~ /^(statement|query)\s/
      if record_start
        @tz = @next_tz || "UTC"
        @next_tz = nil
      end
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
          out.concat(regenerate_query(sql.join, coltypes, sortmode, old))
        end
        # A row-returning DML query record (INSERT/UPDATE/DELETE ... RETURNING —
        # grammar.md §32) mutates state like a `statement ok`, so its effects must reach
        # later records' replay prefix; SELECTs stay un-replayed (side-effect-free).
        # A data-modifying WITH (writable-cte.md) is equally a write — it leads with WITH but
        # contains an INSERT/UPDATE/DELETE — so it must replay too. Appended unconditionally, like
        # `statement ok`: an overridden PG-failing DML query record must sit LAST (conformance.md §5).
        joined = sql.join
        is_dml_query = joined =~ /\A\s*(insert|update|delete)\b/i ||
                       (joined =~ /\A\s*with\b/i &&
                        joined =~ /\b(insert\s+into|update\b[\s\S]*\bset\b|delete\s+from)\b/i)
        @applied << rewrite(joined) if is_dml_query
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
    # Pin the session zone to the record's `# timezone:` (default UTC). jed renders a timestamptz in
    # UTC regardless of the slot (the rendering follow-on, timezones.md §9.5), so a record OUTPUTTING
    # a timestamptz must be authored under UTC (or rendered via AT TIME ZONE 'UTC') to match; the
    # zone-DEPENDENT computation of date_trunc/EXTRACT/casts runs in the same zone PG uses. SET LOCAL
    # is scoped to this rolled-back transaction.
    script << "SET LOCAL TimeZone='#{@tz}';\n"
    @applied.each { |s| script << s.rstrip << ";\n" unless s.strip.empty? }
    script << "\\echo @@@S\n" << body << "\n\\echo @@@E\n" << "ROLLBACK;\n"
    out, err, = Open3.capture3(*args, stdin_data: script)
    s = out.lines.index { |l| l.chomp == "@@@S" }
    e = out.lines.index { |l| l.chomp == "@@@E" }
    # A missing @@@S means psql / the connection itself failed. A missing @@@E with @@@S
    # present is the BODY's doing — an unterminated string or `/*` comment swallows the
    # trailing `\echo` (psql accumulates it as literal text and flushes the open buffer at
    # EOF, so PG still reports the body's error on stderr — grammar.md §33). Return the
    # truncated window; the error-probing caller only needs stderr.
    raise "psql sentinels missing for #{@path}:\n#{out}#{err}" unless s

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

  def regenerate_query(sql, declared_coltypes, sortmode, old)
    pg_sql = rewrite(sql).rstrip.chomp(";")

    # Type pass: `\gdesc` describes result columns without executing (default `|` fieldsep).
    # \gdesc reports the type WITH its typmod (`numeric(10,2)`); the tag is by base type.
    desc, = run("#{pg_sql} \\gdesc")
    types = desc.map { |l| n, t = l.chomp.split("|", 2); [n, t.sub(/\(.*\)\z/, "")] }
    # An empty description means PG could not even plan the query — almost always a jed extension
    # PG has no overload for (e.g. MIN/MAX over uuid or boolean: jed defines the ordering, PG ships
    # no such aggregate). Keep the committed rows and flag it, rather than crashing on ncol == 0.
    if types.empty?
      @warnings << "PG could not describe `#{sql.strip}` (no result columns — likely a jed " \
                   "extension PG lacks). Keeping the committed rows; add an oracle_overrides " \
                   "entry to document the divergence."
      return old
    end
    tags = types.map do |(_name, t)|
      # An array result column (PG describes it as `<elem>[]`) renders as a printable string, the
      # `T` tag — like composite/uuid/bytea (spec/design/array.md §7).
      TAG[t] ||
        (t.end_with?("[]") || composite_type?(t) ? "T" : raise("no coltype tag for PG type #{t.inspect}"))
    end
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

  # Whether a PG result-column type is a COMPOSITE (row) type — it renders as text via PG's
  # composite output, which jed's `record_out` matches byte-for-byte (spec/design/composite.md §8),
  # so it takes the `T` tag. `\gdesc` reports the registered composite type's NAME (e.g. `addr`) or
  # the literal `record` for an anonymous `ROW(...)`; neither is in `TAG`. A registered composite is
  # detected by `typtype = 'c'` in the replayed session (the `CREATE TYPE`s are in the applied
  # prefix). Memoized — the lookup is one extra psql round-trip per distinct unknown type.
  def composite_type?(type_name)
    return true if type_name == "record"

    @composite_cache ||= {}
    @composite_cache.fetch(type_name) do
      lit = type_name.gsub("'", "''")
      out, = run("SELECT typtype FROM pg_type WHERE typname = '#{lit}';")
      @composite_cache[type_name] = out.map(&:chomp).include?("c")
    end
  end

  # Mirror the harness's rowsort: group the flat values into rows of `ncol`, sort rows by their
  # NUL-joined string (spec/conformance + impl/*/conformance applySort), then re-flatten.
  def canonical_rowsort(flat, ncol)
    ncol = 1 if ncol < 1
    flat.each_slice(ncol).sort_by { |row| row.join("\x00") }.flatten
  end

  def value_script(pg_sql)
    script = +"BEGIN;\n"
    # Pin the session zone to the record's `# timezone:` (default UTC). jed renders a timestamptz in
    # UTC regardless of the slot (the rendering follow-on, timezones.md §9.5), so a record OUTPUTTING
    # a timestamptz must be authored under UTC (or rendered via AT TIME ZONE 'UTC') to match; the
    # zone-DEPENDENT computation of date_trunc/EXTRACT/casts runs in the same zone PG uses. SET LOCAL
    # is scoped to this rolled-back transaction.
    script << "SET LOCAL TimeZone='#{@tz}';\n"
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
