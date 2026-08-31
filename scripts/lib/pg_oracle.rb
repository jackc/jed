# frozen_string_literal: true

require "open3"
require "toml-rb"

# scripts/lib/pg_oracle.rb — the live-PostgreSQL oracle mechanics, extracted verbatim from
# scripts/oracle_import.rb so the oracle importer AND the RQG firehose (scripts/rqg_gen.rb) share
# ONE implementation (CLAUDE.md §5: data/logic over duplicated code). This is the entire "run a
# query against the live oracle cluster and render it as a sqllogictest `----` block" engine: the
# psql invocation, the jed→PG type rewrite, the PG-type→coltype-tag map, the `\gdesc` type pass,
# the newline-fieldsep value pass, and the canonical rowsort.
#
# Connection: honors the PGHOST/PGPORT env, which mise loads from this checkout's .dev/ports.env
# and which point at the cluster process-compose supervises (a Unix socket — faster than localhost
# TCP; `local all all trust` auth ⇒ no password). NEVER pass -h, and never set one of PGHOST /
# PGPORT without the other.
#
# Profile: the oracle's configuration is DATA, not whatever the nearest server defaults to — see
# spec/conformance/oracle_profile.toml. Cluster facts (PG major, locale provider/locale, encoding,
# database) are ASSERTED once per process and abort on mismatch; session settings (DateStyle,
# IntervalStyle, extra_float_digits, ...) are APPLIED as `SET LOCAL` in every probe's preamble.
# Both matter for VALUES, not just formatting: an `IntervalStyle = iso_8601` server rewrites every
# interval expectation, and a cluster initdb'd under a non-US locale parses ambiguous date literals
# as DMY. Without the pin the corpus is silently calibrated to one machine's defaults.
#
# State: a replay PREFIX (`applied` — the statements known to succeed, replayed before each probed
# body so the body sees the right table state) and a session `tz`. A self-contained consumer (the
# firehose) builds the prefix per case and resets between cases; the importer accumulates it down a
# file. Mechanical only — no warnings, no override logic, no file I/O (those stay in the callers).
class PgOracle
  # No -h here: honor the PGHOST env. Socket connections authenticate via `local all all trust`.
  # The DATABASE, however, comes from the profile rather than the PGDATABASE env: the corpus is
  # calibrated to a specific database's locale, so the harness names it instead of inheriting
  # whatever the shell points at (`PgOracle.psql`). Bare `psql` for humans still follows the env.
  PSQL_BASE = %w[psql -U postgres -q -A -t -X -v ON_ERROR_STOP=0].freeze

  # jed canonical type names -> the PG / SQL-standard spelling PG parses. smallint/integer/bigint
  # need no rewrite; f64/f32 -> double precision/real (the spellings PG's ConstTypename grammar
  # accepts in a `typename 'literal'` const — float8/float4 are not in that production).
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
    # Range types render as text (range_out). jed's i32range/i64range alias int4range/int8range, so
    # both spellings tag T (spec/design/ranges.md §5).
    "int4range" => "T", "int8range" => "T", "numrange" => "T",
    "tsrange" => "T", "tstzrange" => "T", "daterange" => "T",
    # json/jsonb render as text (json_out / jsonb_out canonical form — spec/design/json.md §6.2).
    "json" => "T", "jsonb" => "T",
    # jsonpath renders as its canonical normalized text (spec/design/jsonpath.md §2, P1a).
    "jsonpath" => "T",
  }.freeze

  # The declared oracle profile (see the header). Same path idiom as the overrides ledger.
  PROFILE_PATH = File.expand_path("../../spec/conformance/oracle_profile.toml", __dir__)

  # The cluster facts the profile asserts, in one round-trip. For the libc provider the locale
  # identifier is datcollate; for builtin/icu it is datlocale.
  PROFILE_QUERY = <<~SQL
    SELECT current_setting('server_version_num')::int / 10000,
           d.datlocprovider,
           CASE d.datlocprovider WHEN 'c' THEN d.datcollate ELSE coalesce(d.datlocale, '') END,
           d.datcollate, d.datctype,
           pg_encoding_to_char(d.encoding),
           current_database(),
           (SELECT count(*) FROM pg_timezone_names WHERE name LIKE 'US/%') > 0
      FROM pg_database d WHERE d.datname = current_database();
  SQL

  class << self
    # The parsed profile table. Read once per process.
    def profile = @profile ||= TomlRB.load_file(PROFILE_PATH)

    # The psql invocation, pinned to the profile's database.
    def psql = @psql ||= PSQL_BASE + ["-d", profile.fetch("cluster").fetch("database")]

    # The live server's cluster facts, as one round-trip, read once per process. Returns the
    # same keys the profile's [cluster] declares, plus the raw datcollate/datctype and the
    # tzdata flag. Aborts if the oracle is unreachable — a confusing downstream parse failure
    # is worse than saying "the server is not there".
    def live_cluster
      @live_cluster ||= begin
        out, err, status = Open3.capture3(*psql, "-c", PROFILE_QUERY)
        unless status.success?
          want = profile.fetch("cluster")
          # Two different failures reach here and they have different fixes, so name both rather
          # than guessing: the checkout's cluster is not running (the common one — the oracle is a
          # supervised service now, not an always-on container), or it is running without the
          # database the profile declares.
          abort <<~MSG
            oracle unreachable — spec/conformance/oracle_profile.toml expects database
            #{want['database']} on PG #{want['pg_major']} at PGHOST=#{ENV.fetch('PGHOST', '(unset)')} PGPORT=#{ENV.fetch('PGPORT', '(unset)')}

              server not running?    mise run dev      (starts this checkout's cluster)
              database missing?      rake oracle:setup

            #{err.strip}
          MSG
        end

        major, provider, locale, collate, ctype, encoding, database, us_zones =
          out.lines.map(&:chomp).reject(&:empty?).first.to_s.split("|", 8)
        { "pg_major" => major.to_i, "locale_provider" => provider, "locale" => locale,
          "datcollate" => collate, "datctype" => ctype, "encoding" => encoding,
          "database" => database, "tzdata_backward_links" => us_zones == "t" }
      end
    end

    # Assert the live oracle matches the declared [cluster] facts. Runs ONCE per process (the
    # firehose builds a PgOracle per case, so this must not be per-instance). A mismatch aborts:
    # filling corpus expectations from an undeclared oracle is the failure the profile prevents.
    # A tzdata difference only WARNS — see the profile's [cluster.tzdata].
    def assert_profile!
      return if @profile_checked

      @profile_checked = true # set first: a failure aborts, so this only guards the success path
      want = profile.fetch("cluster")
      live = live_cluster

      bad = %w[pg_major locale_provider locale encoding database]
            .reject { |key| live[key] == want[key] }
            .map { |key| [key, want[key], live[key]] }
      # A split collate/ctype is a configuration the profile does not describe.
      if live["locale_provider"] == "c" && live["datcollate"] != live["datctype"]
        bad << ["datcollate/datctype", "equal", "#{live['datcollate']}/#{live['datctype']}"]
      end

      unless bad.empty?
        abort <<~MSG
          oracle profile mismatch (spec/conformance/oracle_profile.toml):
          #{bad.map { |f, w, g| format('  %-19s expected %-14s got %s', f, w.to_s.inspect, g.to_s.inspect) }.join("\n")}
          The corpus's expected output is calibrated to the declared profile; importing against a
          different one silently produces different answers. Fix the server, or — if this change is
          intended — edit the profile and re-run `rake corpus:check` over the corpus.
        MSG
      end

      want_links = want.dig("tzdata", "backward_links")
      got_links = live["tzdata_backward_links"]
      return if want_links.nil? || got_links == want_links

      warn "WARNING: oracle tzdata backward_links = #{got_links} (profile declares #{want_links}). " \
           "expr/at_time_zone.test's override depends on this; it may now be a silent no-op."
    end
  end

  attr_accessor :applied, :tz

  def initialize(label: "oracle", tz: "UTC")
    @label = label    # used only in the "sentinels missing" raise message
    @applied = []      # PG-rewritten SQL of statements known to succeed (the replay prefix)
    @tz = tz           # the session zone for the CURRENT probe (`# timezone:` directive)
    PgOracle.assert_profile!
  end

  # Clear the replay prefix and reset the zone — a firehose consumer calls this between cases.
  def reset(tz: "UTC")
    @applied = []
    @tz = tz
    self
  end

  # Record a successfully-applied statement into the replay prefix (PG-rewritten).
  def add_applied(sql) = @applied << rewrite(sql)

  def rewrite(sql)
    TYPE_REWRITE.reduce(sql) { |s, (jed, pg)| s.gsub(/\b#{jed}\b/, pg) }
  end

  # The PG spelling of a query body, ready for \gdesc / the value pass (rewritten, de-`;`'d).
  def pg_sql(sql) = rewrite(sql).rstrip.chomp(";")

  # One isolated psql invocation: BEGIN, replay the successful prefix, run `body`, ROLLBACK.
  # stdout and stderr are captured SEPARATELY: psql writes errors to stderr (unbuffered) and
  # `\echo` sentinels to stdout (block-buffered), so a merged stream races them out of order.
  # Returns [stdout lines between the sentinels, full stderr string].
  def run(body, fieldsep: nil)
    args = PgOracle.psql.dup
    args += ["-F", fieldsep] if fieldsep
    script = +"BEGIN;\n"
    script << session_preamble
    @applied.each { |s| script << s.rstrip << ";\n" unless s.strip.empty? }
    script << "\\echo @@@S\n" << body << "\n\\echo @@@E\n" << "ROLLBACK;\n"
    out, err, = Open3.capture3(*args, stdin_data: script)
    s = out.lines.index { |l| l.chomp == "@@@S" }
    e = out.lines.index { |l| l.chomp == "@@@E" }
    # A missing @@@S means psql / the connection itself failed. A missing @@@E with @@@S present is
    # the BODY's doing (an unterminated string/`/*` swallows the trailing `\echo`); return the
    # truncated window — the error-probing caller only needs stderr.
    raise "psql sentinels missing for #{@label}:\n#{out}#{err}" unless s

    [out.lines[(s + 1)...e], err]
  end

  # The body must terminate the SQL statement itself; `run` only appends `\echo`/ROLLBACK after.
  def terminate(sql) = rewrite(sql).strip.chomp(";") + ";"

  # `\gdesc` describes a query's result columns WITHOUT executing. Returns [[name, base_type], ...]
  # with the typmod stripped (`numeric(10,2)` -> `numeric`); empty means PG could not plan it.
  def describe(pg_sql)
    desc, = run("#{pg_sql} \\gdesc")
    desc.map { |l| n, t = l.chomp.split("|", 2); [n, t.sub(/\(.*\)\z/, "")] }
  end

  # PG base type -> coltype tag. Arrays (`<elem>[]`) and composites render as a printable string
  # (the `T` tag — spec/design/array.md §7, composite.md §8). Raises on a genuinely unknown type
  # (the importer wants the loud failure; the firehose rescues it and skips the case).
  def tag_for(type)
    TAG[type] || (type.end_with?("[]") || composite_type?(type) ? "T" : raise("no coltype tag for PG type #{type.inspect}"))
  end

  # Whether a PG result-column type is a COMPOSITE (row) type — detected by `typtype = 'c'` in the
  # replayed session (the CREATE TYPEs are in the applied prefix), or the literal `record` for an
  # anonymous ROW(...). Memoized: one extra psql round-trip per distinct unknown type.
  def composite_type?(type_name)
    return true if type_name == "record"

    @composite_cache ||= {}
    @composite_cache.fetch(type_name) do
      lit = type_name.gsub("'", "''")
      out, = run("SELECT typtype FROM pg_type WHERE typname = '#{lit}';")
      @composite_cache[type_name] = out.map(&:chomp).include?("c")
    end
  end

  # The value pass: fieldsep=newline gives row-major one-value-per-line; NULL is explicit; PG's
  # boolean `t`/`f` are normalized to `true`/`false` (conformance.md §1). Returns the flat values.
  def query_values(pg_sql, is_bool)
    out, = Open3.capture2e(*(PgOracle.psql + ["-P", "null=NULL", "-F", "\n"]),
                           stdin_data: value_script(pg_sql))
    s = out.lines.index { |l| l.chomp == "@@@S" }
    e = out.lines.index { |l| l.chomp == "@@@E" }
    vals = out.lines[(s + 1)...e].map(&:chomp)
    ncol = is_bool.size
    vals.each_with_index.map do |v, k|
      is_bool[k % ncol] ? { "t" => "true", "f" => "false" }.fetch(v, v) : v
    end
  end

  # Mirror the harness's rowsort: group the flat values into rows of `ncol`, sort rows by their
  # NUL-joined string (impl/*/conformance applySort), then re-flatten.
  def canonical_rowsort(flat, ncol)
    ncol = 1 if ncol < 1
    flat.each_slice(ncol).sort_by { |row| row.join("\x00") }.flatten
  end

  # The `SET LOCAL` preamble every probe shares — the ONE place session state is established
  # (this file's reason for existing: one implementation, not two). It carries the record's zone
  # (`# timezone:`, default UTC) plus every declared [session] pin from the profile, so a probe
  # never inherits a server default that could differ from the calibrated one. SET LOCAL is scoped
  # to the probe's rolled-back transaction (timezones.md §9.4), so nothing leaks between probes.
  def session_preamble
    out = +"SET LOCAL TimeZone='#{@tz}';\n"
    PgOracle.profile.fetch("session", {}).each do |name, value|
      out << "SET LOCAL #{name} = '#{value.to_s.gsub("'", "''")}';\n"
    end
    out
  end

  private

  def value_script(pg_sql)
    script = +"BEGIN;\n"
    script << session_preamble
    @applied.each { |s| script << s.rstrip << ";\n" unless s.strip.empty? }
    script << "\\echo @@@S\n" << pg_sql << ";\n" << "\\echo @@@E\n" << "ROLLBACK;\n"
    script
  end
end
