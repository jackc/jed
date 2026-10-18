# frozen_string_literal: true

require "open3"
require_relative "bundle_setup" # gems from Gemfile.lock, whatever the cwd or entry point
require "toml-rb"

# scripts/lib/pg_oracle.rb — the live-PostgreSQL oracle mechanics, extracted verbatim from
# scripts/oracle_import.rb so the oracle importer AND the RQG firehose (scripts/rqg_gen.rb) share
# ONE implementation (CLAUDE.md §5: data/logic over duplicated code). This is the entire "run a
# query against the live oracle cluster and render it as a sqllogictest `----` block" engine: the
# psql invocation, the jed→PG type rewrite, the PG-type→coltype-tag map, the `\gdesc` type pass,
# the newline-fieldsep value pass, and the canonical rowsort.
#
# Connection: honors the PGHOST/PGPORT env, which mise loads from this checkout's .dev/ files
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
#
# Containment: every probe runs inside one BEGIN…ROLLBACK, so nothing it replays persists — and
# statements PostgreSQL refuses in a transaction block (REINDEX DATABASE, VACUUM, CREATE DATABASE)
# fail harmlessly rather than running against the cluster. A .test's OWN transaction control (BEGIN,
# COMMIT, ROLLBACK — suites/transactions/*.test, every *_savepoint.test) would end that wrapper: a
# COMMIT made the prefix's tables permanent for every later probe and file, a ROLLBACK undid the
# prefix mid-probe. So it is mapped onto a savepoint inside the wrapper (`contain`); the test's own
# SAVEPOINT / RELEASE / ROLLBACK TO nest inside that savepoint unchanged. A `statement error` inside
# the test's transaction replays too, as a raise of its declared code, so the block fails as it did
# in jed and a later COMMIT rolls back (`contain_failed`).
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
              database missing?      mise run oracle:setup

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
          intended — edit the profile and re-run `mise run corpus:check` over the corpus.
        MSG
      end

      want_links = want.dig("tzdata", "backward_links")
      got_links = live["tzdata_backward_links"]
      return if want_links.nil? || got_links == want_links

      warn "WARNING: oracle tzdata backward_links = #{got_links} (profile declares #{want_links}). " \
           "expr/at_time_zone.test's override depends on this; it may now be a silent no-op."
    end
  end

  attr_accessor :applied, :tz, :prelude

  # The savepoint standing in for a .test's own transaction (see Containment in the header).
  TXN_SAVEPOINT = "jed_txn"
  TXN_BEGIN = /\A\s*(?:BEGIN|START\s+TRANSACTION)\b(?<modes>[^;]*);?\s*\z/i
  TXN_COMMIT = /\A\s*(?:COMMIT|END)(?:\s+(?:WORK|TRANSACTION))?\s*;?\s*\z/i
  TXN_ROLLBACK = /\A\s*(?:ROLLBACK|ABORT)(?:\s+(?:WORK|TRANSACTION))?\s*;?\s*\z/i
  TXN_SUBCONTROL = /\A\s*(?:SAVEPOINT|RELEASE|ROLLBACK\s+(?:(?:WORK|TRANSACTION)\s+)?TO)\b/i

  # contain maps one successful statement's transaction control onto the wrapper's savepoint, given
  # the test's transaction `state`: nil outside one, else { read_only:, failed: }. Returns
  # [sql, new state]; anything else passes through.
  #   BEGIN [READ ONLY]  -> SAVEPOINT jed_txn (read-only via SET LOCAL, undone with the savepoint)
  #   COMMIT / END       -> RELEASE it; a failed block rolls back (as PostgreSQL's COMMIT does), and
  #                         so does a read-only one, which wrote nothing and whose RELEASE would keep
  #                         transaction_read_only for the wrapper
  #   ROLLBACK / ABORT   -> ROLLBACK TO it, then RELEASE it
  #   ROLLBACK TO x      -> unchanged; it clears a failed block, returning to x
  # Outside a test transaction, COMMIT/ROLLBACK are no-ops (PostgreSQL only warns), a BEGIN inside
  # one is too, and SAVEPOINT / RELEASE / ROLLBACK TO raise 25P01 as PostgreSQL does — the wrapper
  # would otherwise let them succeed.
  def self.contain(sql, state)
    if (m = TXN_BEGIN.match(sql))
      return ["", state] if state

      read_only = m[:modes].match?(/\bREAD\s+ONLY\b/i)
      out = +"SAVEPOINT #{TXN_SAVEPOINT}"
      out << ";\nSET LOCAL transaction_read_only = on" if read_only
      [out, { read_only: read_only, failed: false }.freeze]
    elsif TXN_COMMIT.match?(sql)
      return ["", nil] unless state

      [state[:read_only] || state[:failed] ? undo_txn : "RELEASE SAVEPOINT #{TXN_SAVEPOINT}", nil]
    elsif TXN_ROLLBACK.match?(sql)
      [state ? undo_txn : "", nil]
    elsif TXN_SUBCONTROL.match?(sql)
      return [raise_sql("25P01", "#{subcontrol_word(sql)} can only be used in transaction blocks"), nil] unless state

      rollback_to = sql.strip.start_with?(/ROLLBACK/i)
      [sql, rollback_to ? { read_only: state[:read_only], failed: false }.freeze : state]
    else
      [sql, state]
    end
  end

  # contain_failed is contain for a `statement error` record: the statement jed rejected, with the
  # declared SQLSTATE. Outside a test transaction it had no effect (autocommit), so nothing replays.
  # Inside one it failed the block (transactions.md §6), whatever PostgreSQL would make of the
  # statement itself, so it replays as a raise of the declared code, which aborts the savepoint with
  # no effects; a failing COMMIT or ROLLBACK ends the transaction rolled back. A nested BEGIN is the
  # exception: jed rejects it (25001) without failing the block, and PostgreSQL only warns.
  def self.contain_failed(sql, code, state)
    return ["", nil] if state.nil?
    return ["", state] if TXN_BEGIN.match?(sql)
    return [undo_txn, nil] if TXN_COMMIT.match?(sql) || TXN_ROLLBACK.match?(sql)

    code = "P0001" unless code.match?(/\A[0-9A-Z]{5}\z/)
    [raise_sql(code, "replayed jed error"), { read_only: state[:read_only], failed: true }.freeze]
  end

  def self.raise_sql(code, message) =
    "DO $$BEGIN RAISE EXCEPTION USING ERRCODE = '#{code}', MESSAGE = '#{message}'; END$$"

  def self.subcontrol_word(sql)
    word = sql.strip.split(/\s+/, 2).first.upcase
    word == "ROLLBACK" ? "ROLLBACK TO SAVEPOINT" : word
  end

  def self.undo_txn = "ROLLBACK TO SAVEPOINT #{TXN_SAVEPOINT};\nRELEASE SAVEPOINT #{TXN_SAVEPOINT}"

  def initialize(label: "oracle", tz: "UTC")
    @label = label    # used only in the "sentinels missing" raise message
    @applied = []      # [PG-rewritten SQL, zone] of statements known to succeed (the replay prefix)
    @tz = tz           # the session zone for the CURRENT probe (`# timezone:` directive)
    @prelude = nil     # SQL run after the replay prefix, before the probe (a bound record's PREPARE)
    PgOracle.assert_profile!
  end

  # Clear the replay prefix and reset the zone — a firehose consumer calls this between cases.
  def reset(tz: "UTC")
    @applied = []
    @tz = tz
    self
  end

  # Record a successfully-applied statement into the replay prefix (PG-rewritten), with the session
  # zone it ran under: a statement's effect can depend on the zone (an offset-less timestamptz
  # literal is read in it — timezones.md §9.3), so the replay re-runs it in that same zone.
  def add_applied(sql) = @applied << [rewrite(sql), @tz]

  # Record a `statement error` record into the replay prefix with its declared SQLSTATE: it changes
  # no data, but inside the test's own transaction it fails the block (see contain_failed).
  def add_failed(sql, code) = @applied << [rewrite(sql), @tz, code]

  def rewrite(sql)
    TYPE_REWRITE.reduce(sql) { |s, (jed, pg)| s.gsub(/\b#{jed}\b/, pg) }
  end

  # The PG spelling of a query body, ready for \gdesc / the value pass (rewritten, de-`;`'d).
  def pg_sql(sql) = rewrite(sql).rstrip.chomp(";")

  # One isolated psql invocation: BEGIN, replay the successful prefix, run `body`, ROLLBACK.
  # stdout and stderr are captured SEPARATELY: psql writes errors to stderr (unbuffered) and
  # `\echo` sentinels to stdout (block-buffered), so a merged stream races them out of order.
  # The prefix may raise on purpose (a replayed `statement error` — contain_failed), so the body's
  # start is marked on stderr too (`\warn`), and only what follows it is returned.
  # Returns [stdout lines between the sentinels, the body's stderr string].
  def run(body, fieldsep: nil)
    args = PgOracle.psql.dup
    args += ["-F", fieldsep] if fieldsep
    prefix, state = replay_prefix
    script = +"BEGIN;\n"
    script << session_preamble
    script << prefix
    script << @prelude.to_s
    script << "\\echo @@@S\n\\warn @@@S\n" << contain_body(body, state) << "\n\\echo @@@E\n" << "ROLLBACK;\n"
    out, err, = Open3.capture3(*args, stdin_data: script)
    err = err.split(/^@@@S\n/, 2).last
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
  # with the typmod stripped (`numeric(10,2)` -> `numeric`, `timestamp(3) without time zone` ->
  # `timestamp without time zone`, `interval day to second(3)` -> `interval`); empty means PG could
  # not plan it.
  def describe(pg_sql)
    desc, = run("#{pg_sql} \\gdesc")
    desc.map do |l|
      n, t = l.chomp.split("|", 2)
      t = t.gsub(/\(\d+(,\d+)?\)/, "")
      t = "interval" if t.start_with?("interval ")
      [n, t]
    end
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

  # The applied prefix, each statement under the zone it originally ran in and with its transaction
  # control contained, then the probe's own zone restored for the body. Returns [text, state]: whether
  # the replayed test left its transaction open, for the body's own containment.
  def replay_prefix
    out = +""
    zone = @tz
    state = nil
    @applied.each do |sql, tz, failed_code|
      next if sql.strip.empty?

      out << "SET LOCAL TimeZone='#{tz}';\n" if tz != zone
      zone = tz
      sql, state = failed_code ? PgOracle.contain_failed(sql, failed_code, state) : PgOracle.contain(sql, state)
      out << sql.rstrip << ";\n" unless sql.empty?
    end
    out << "SET LOCAL TimeZone='#{@tz}';\n" if zone != @tz
    [out, state]
  end

  # A probe body is psql meta-command lines (`\set VERBOSITY`), then SQL; contain the SQL. A body
  # that is not transaction control passes through untouched (it may end in `\gdesc`).
  def contain_body(body, state)
    meta = body.lines.take_while { |l| l.start_with?("\\") }
    sql = body.lines.drop(meta.size).join
    contained, = PgOracle.contain(sql, state)
    return body if contained == sql

    meta.join + contained + (contained.empty? ? "" : ";")
  end

  def value_script(pg_sql)
    script = +"BEGIN;\n"
    script << session_preamble
    script << replay_prefix.first
    script << @prelude.to_s
    script << "\\echo @@@S\n" << pg_sql << ";\n" << "\\echo @@@E\n" << "ROLLBACK;\n"
    script
  end
end
