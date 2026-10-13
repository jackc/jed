# frozen_string_literal: true

# scripts/sim_gen.rb — the deterministic-simulation trace generator (spec/design/simulation-testing.md).
#
# Emits a `.sim` trace: a seeded, multi-session history of SQL, commits, faults, and crashes that
# every core's simulation runner executes against a fault-injecting simulated disk. The trace holds
# no expected results — the runners' oracles (generation history, durability floor, integrity check)
# decide pass/fail, and the driver (scripts/sim.rb) compares transcripts across cores.
#
# The generator is the ONE author of traces (CLAUDE.md §5): the per-core runners only execute them.
# It keeps a model of the schema, the open sessions, and the writer gate, so it emits mostly valid
# statements. After a faulted commit it cannot know whether the commit landed, so every table the
# transaction changed becomes "maybe": its next use re-establishes it idempotently
# (CREATE TABLE IF NOT EXISTS, ADD COLUMN IF NOT EXISTS, CREATE INDEX IF NOT EXISTS).
#
#   ruby scripts/sim_gen.rb --profile mixed --seed 7               # print one trace
#   ruby scripts/sim_gen.rb --profile mixed --seed 7 --steps 2000  # a longer one
#
# Determinism: the generator's only randomness is its splitmix64 stream seeded from --seed, so a
# (profile, seed, steps) triple always yields the same trace.

require "optparse"
require_relative "lib/bundle_setup"
require "toml-rb"

module SimGen
  REPO = File.expand_path("..", __dir__)
  PROFILE_DIR = File.join(REPO, "spec/conformance/sim/profiles")
  MASK64 = 0xFFFF_FFFF_FFFF_FFFF

  # The built-in defaults; spec/conformance/sim/profiles/mixed.toml states the same values.
  DEFAULTS = {
    "page_sizes" => [256, 512, 1024],
    "cache_bytes" => [0, 2048, 8192],
    "steps" => 300,
    "max_tables" => 3,
    "max_rows" => 300,
    "batch" => [1, 40],
    "max_readers" => 3,
    "weights" => {
      "open_write" => 10, "open_read" => 6, "close_read" => 5, "read_query" => 8,
      "insert" => 14, "update" => 9, "delete" => 5, "upsert" => 4,
      "create_table" => 2, "drop_table" => 1, "create_index" => 2, "drop_index" => 1,
      "add_column" => 1, "analyze" => 1, "sequence" => 1, "invalid" => 1,
      "commit" => 9, "rollback" => 2, "check" => 2, "reopen" => 1, "clock" => 1,
      "crash_power" => 1, "crash_process" => 1
    },
    "values" => {
      "short" => 10, "long_compressible" => 3, "long_incompressible" => 3, "null" => 2, "uuid" => 1
    },
    "faults" => {
      "commit_fault" => 150, "chain" => 150, "power_loss" => 5, "process_crash" => 3,
      "io_error_write" => 1, "io_error_sync" => 1,
      "survive" => [100, 900], "tear" => [0, 400], "issue" => [200, 900], "io_p" => [50, 500]
    }
  }.freeze

  # Load a profile by name and merge it over the defaults (one level deep for the weight tables).
  def self.profile(name)
    path = File.join(PROFILE_DIR, "#{name}.toml")
    raise ArgumentError, "unknown simulation profile #{name.inspect} (#{path})" unless File.exist?(path)

    over = TomlRB.load_file(path)
    merged = DEFAULTS.transform_values { |v| v.is_a?(Hash) ? v.dup : v }
    over.each do |k, v|
      merged[k] = v.is_a?(Hash) && merged[k].is_a?(Hash) ? merged[k].merge(v) : v
    end
    merged["name"] = name
    merged
  end

  def self.profile_names
    Dir[File.join(PROFILE_DIR, "*.toml")].map { |p| File.basename(p, ".toml") }.sort
  end

  # splitmix64 — the same stream the entropy seam pins (spec/encoding/prng.toml).
  class Rng
    def initialize(seed)
      @state = seed & MASK64
    end

    def next_u64
      @state = (@state + 0x9E37_79B9_7F4A_7C15) & MASK64
      z = @state
      z = ((z ^ (z >> 30)) * 0xBF58_476D_1CE4_E5B9) & MASK64
      z = ((z ^ (z >> 27)) * 0x94D0_49BB_1331_11EB) & MASK64
      z ^ (z >> 31)
    end

    # A uniform integer in [lo, hi] (inclusive).
    def range(lo, hi)
      lo + (next_u64 % (hi - lo + 1))
    end

    def pick(list) = list[next_u64 % list.length]

    def chance(permille) = (next_u64 % 1000) < permille

    # Draw a key from a { key => weight } hash; zero-weight keys are never drawn.
    def weighted(weights)
      live = weights.select { |_, w| w.positive? }
      total = live.values.sum
      return nil if total.zero?

      r = next_u64 % total
      live.each do |k, w|
        return k if r < w

        r -= w
      end
    end
  end

  # One table in the model. cols are [name, type, role]; role :key marks the immutable i64 column
  # derived from id (safe under a UNIQUE index). orig is how many cols CREATE TABLE declared.
  Table = Struct.new(:name, :cols, :orig, :next_id, :indexes, :maybe, keyword_init: true) do
    def deep_dup
      Table.new(name: name, cols: cols.map(&:dup), orig: orig, next_id: next_id,
                indexes: indexes.transform_values(&:dup), maybe: maybe)
    end

    def same?(other)
      cols == other.cols && indexes == other.indexes && next_id == other.next_id && maybe == other.maybe
    end
  end

  class Generator
    def initialize(profile, seed, steps: nil)
      @p = profile
      @seed = seed
      @rng = Rng.new(seed)
      @steps = steps || @p["steps"]
      @lines = []
      @committed = {}       # table name => Table (the last known committed schema)
      @working = nil        # the open write transaction's schema, or nil
      @aborted = false      # the open write transaction hit an error (commands now 25P02)
      @writer = nil         # the open write session id, or nil
      @readers = {}         # reader session id => table names visible at its open
      @sequences = []       # sequence names ever created
      @next = Hash.new(0)   # name counters
    end

    def generate
      page_size = @rng.pick(@p["page_sizes"])
      cache = @rng.pick(@p["cache_bytes"])
      header = ["# format: simulation", "# profile: #{@p['name']}", "# seed: #{@seed}",
                "# page_size: #{page_size}"]
      header << "# cache_bytes: #{cache}" if cache.positive?
      header << "# clock: 1700000000000000"
      bootstrap
      step_once while @lines.length < @steps
      finish
      (header + @lines).join("\n") + "\n"
    end

    private

    def emit(line) = @lines << line

    def fresh(prefix)
      @next[prefix] += 1
      "#{prefix}#{@next[prefix]}"
    end

    # The first transaction creates one table so most draws have something to act on.
    def bootstrap
      open_write
      create_table
      insert
      commit(fault: false)
    end

    def finish
      commit(fault: false) if @writer
      @readers.keys.each { |sid| emit("close #{sid}") }
      @readers.clear
      emit("reopen")
      emit("check")
    end

    def step_once
      action = @rng.weighted(@p["weights"])
      case action
      when "open_write" then open_write unless @writer
      when "open_read" then open_read
      when "close_read" then close_read
      when "read_query" then read_query
      when "commit" then commit if @writer
      when "rollback" then rollback if @writer
      when "check" then emit("check")
      when "reopen" then reopen
      when "clock" then emit("clock +#{@rng.range(1, 5_000_000)}")
      when "crash_power" then crash_power
      when "crash_process" then crash_process
      else
        writer_statement(action)
      end
    end

    # --- sessions -------------------------------------------------------------------------------

    def open_write
      @writer = fresh("w")
      @working = @committed.transform_values(&:deep_dup)
      @aborted = false
      emit("open #{@writer} write")
    end

    def open_read
      return if @readers.length >= @p["max_readers"]

      sid = fresh("r")
      @readers[sid] = @committed.keys.sort
      emit("open #{sid} read")
    end

    def close_read
      return if @readers.empty?

      sid = @rng.pick(@readers.keys.sort)
      @readers.delete(sid)
      emit("close #{sid}")
    end

    # After a crash, the first commit is the one whose barrier stabilizes the recovered generation
    # (validated-cow.md §3); `chain` is the chance that commit is faulted too.
    def commit(fault: true)
      rate = @after_crash ? @p.dig("faults", "chain") : @p.dig("faults", "commit_fault")
      faulted = fault && @rng.chance(rate)
      @after_crash = faulted
      emit(fault_line) if faulted
      emit("commit #{@writer}")
      if faulted
        merge_uncertain
        kill_sessions
      else
        @committed = @working unless @aborted
        @writer = nil
        @working = nil
      end
    end

    def rollback
      emit("rollback #{@writer}")
      @writer = nil
      @working = nil
    end

    def reopen
      # The runner rolls back an open writer and closes readers before the clean close.
      @writer = nil
      @working = nil
      @readers.clear
      emit("reopen")
    end

    def crash_power
      emit("crash power survive=#{draw('survive')} tear=#{draw('tear')}")
      kill_sessions
    end

    def crash_process
      emit("crash process")
      kill_sessions
    end

    # Every crash ends every session of the crashed Database (simulation-testing.md §5).
    def kill_sessions
      @after_crash = true
      @writer = nil
      @working = nil
      @readers.clear
    end

    # A permille fault parameter. A quarter of the draws take the range's extreme — every write lands
    # (survive/issue 1000) or none is torn (tear 0) — so complete in-flight generations, which a
    # random subset rarely produces, are recovered often enough to exercise that path.
    def draw(key)
      lo, hi = @p.dig("faults", key)
      if @rng.chance(250)
        return key == "tear" ? 0 : 1000 if %w[survive issue tear].include?(key)
      end
      @rng.range(lo, hi)
    end

    def fault_line
      kind = @rng.weighted(@p["faults"].slice("power_loss", "process_crash", "io_error_write", "io_error_sync"))
      case kind
      when "power_loss" then "fault power_loss survive=#{draw('survive')} tear=#{draw('tear')}"
      when "process_crash" then "fault process_crash issue=#{draw('issue')}"
      when "io_error_write" then "fault io_error op=write p=#{draw('io_p')}"
      else "fault io_error op=sync nth=#{@rng.range(1, 2)}"
      end
    end

    # After a faulted commit the outcome is unknown: anything the transaction changed may or may not
    # have landed. Keep the union of both schemas, every difference marked maybe.
    def merge_uncertain
      return if @aborted || @working.nil?

      merged = {}
      (@committed.keys | @working.keys).sort.each do |name|
        c = @committed[name]
        w = @working[name]
        if c && w && c.same?(w)
          merged[name] = c
          next
        end
        base = (w || c).deep_dup
        if c && w
          c.cols.each { |col| base.cols << col.dup unless base.cols.any? { |b| b[0] == col[0] } }
          c.indexes.each { |n, d| base.indexes[n] ||= d.dup }
          base.next_id = [c.next_id, w.next_id].max
        end
        base.maybe = true
        merged[name] = base
      end
      @committed = merged
    end

    # --- statements in the write session --------------------------------------------------------

    def writer_statement(action)
      return unless @writer
      return if @aborted # only commit/rollback make sense in a failed block

      case action
      when "insert" then insert
      when "update" then update
      when "delete" then delete
      when "upsert" then upsert
      when "create_table" then create_table
      when "drop_table" then drop_table
      when "create_index" then create_index
      when "drop_index" then drop_index
      when "add_column" then add_column
      when "analyze" then analyze
      when "sequence" then sequence
      when "invalid" then invalid
      end
    end

    def on(sql) = emit("on #{@writer} #{sql}")

    # A table the working transaction can use, re-established first if its existence is uncertain.
    def pick_table
      return nil if @working.empty?

      t = @working[@rng.pick(@working.keys.sort)]
      ensure_table(t)
      t
    end

    def ensure_table(t)
      return unless t.maybe

      on("CREATE TABLE IF NOT EXISTS #{t.name} (#{column_defs(t.cols.first(t.orig))})")
      t.cols.drop(t.orig).each do |name, type, _|
        on("ALTER TABLE #{t.name} ADD COLUMN IF NOT EXISTS #{name} #{type}")
      end
      t.indexes.sort.each { |n, (col, unique)| on(index_sql(n, t.name, col, unique)) }
      t.maybe = false
    end

    def column_defs(cols)
      cols.map { |name, type, _| name == "id" ? "id i64 PRIMARY KEY" : "#{name} #{type}" }.join(", ")
    end

    def index_sql(name, table, col, unique)
      "CREATE #{unique ? 'UNIQUE ' : ''}INDEX IF NOT EXISTS #{name} ON #{table} (#{col})"
    end

    def create_table
      return if @working.length >= @p["max_tables"]

      name = fresh("t")
      cols = [["id", "i64", :key]]
      n = @rng.range(1, 4)
      n.times do |i|
        type = @rng.pick(%w[i32 i64 text text bytea])
        cols << ["c#{i + 1}", type, type == "i64" ? :key : :plain]
      end
      t = Table.new(name: name, cols: cols, orig: cols.length, next_id: 1, indexes: {}, maybe: false)
      @working[name] = t
      on("CREATE TABLE #{name} (#{column_defs(cols)})")
    end

    def drop_table
      return if @working.length <= 1

      t = @working[@rng.pick(@working.keys.sort)]
      @working.delete(t.name)
      on("DROP TABLE IF EXISTS #{t.name}")
    end

    def create_index
      t = pick_table or return
      cols = t.cols.drop(1).select { |_, type, _| %w[i32 i64].include?(type) }
      return if cols.empty?

      col, type, role = @rng.pick(cols)
      unique = role == :key && type == "i64" && @rng.chance(500)
      name = fresh("ix")
      t.indexes[name] = [col, unique]
      on(index_sql(name, t.name, col, unique).sub(" IF NOT EXISTS", ""))
    end

    def drop_index
      t = pick_table or return
      return if t.indexes.empty?

      name = @rng.pick(t.indexes.keys.sort)
      t.indexes.delete(name)
      on("DROP INDEX #{name}")
    end

    def add_column
      t = pick_table or return
      return if t.cols.length >= 7

      type = @rng.pick(%w[i32 text])
      name = "c#{t.cols.length}x#{@next['col'] += 1}"
      t.cols << [name, type, :plain]
      on("ALTER TABLE #{t.name} ADD COLUMN #{name} #{type}")
    end

    def analyze
      t = pick_table or return
      on("ANALYZE #{t.name}")
    end

    # A sequence may have been lost with a rolled-back or faulted transaction, so every use
    # re-establishes it first; nextval is transactional (sequences.md), so its value needs no model.
    def sequence
      name = @sequences.empty? || @rng.chance(300) ? fresh("s") : @rng.pick(@sequences.sort)
      @sequences << name unless @sequences.include?(name)
      on("CREATE SEQUENCE IF NOT EXISTS #{name}")
      on("SELECT nextval('#{name}')")
    end

    def invalid
      on(@rng.pick(["INSERT INTO no_such_table VALUES (1)", "SELECT 1 / 0",
                    "INSERT INTO #{@working.keys.min || 'no_such_table'} (id) VALUES (NULL)"]))
      @aborted = true
    end

    def insert
      t = pick_table or return
      return if t.next_id > @p["max_rows"]

      lo = t.next_id
      hi = lo + @rng.range(*@p["batch"]) - 1
      t.next_id = hi + 1
      cols = t.cols.map(&:first).join(", ")
      exprs = t.cols.map { |name, type, role| value_expr(name, type, role, "g") }.join(", ")
      on("INSERT INTO #{t.name} (#{cols}) SELECT #{exprs} FROM generate_series(#{lo}, #{hi}) g")
    end

    def upsert
      t = pick_table or return
      return if t.next_id > @p["max_rows"]

      lo = @rng.range(1, t.next_id)
      hi = lo + @rng.range(*@p["batch"]) - 1
      t.next_id = [t.next_id, hi + 1].max
      cols = t.cols.map(&:first)
      exprs = t.cols.map { |name, type, role| value_expr(name, type, role, "g") }.join(", ")
      sets = t.cols.drop(1).reject { |_, _, role| role == :key }.map { |name, _, _| "#{name} = excluded.#{name}" }
      action = sets.empty? ? "DO NOTHING" : "DO UPDATE SET #{sets.join(', ')}"
      on("INSERT INTO #{t.name} (#{cols.join(', ')}) SELECT #{exprs} FROM generate_series(#{lo}, #{hi}) g " \
         "ON CONFLICT (id) #{action}")
    end

    def update
      t = pick_table or return
      targets = t.cols.drop(1).reject { |_, _, role| role == :key }
      return if targets.empty? || t.next_id <= 1

      chosen = targets.select { @rng.chance(600) }
      chosen = [@rng.pick(targets)] if chosen.empty?
      sets = chosen.map { |name, type, role| "#{name} = #{update_expr(name, type, role)}" }.join(", ")
      on("UPDATE #{t.name} SET #{sets} WHERE #{range_predicate(t)}")
    end

    def delete
      t = pick_table or return
      return if t.next_id <= 1

      on("DELETE FROM #{t.name} WHERE #{range_predicate(t)}")
    end

    def range_predicate(t)
      if @rng.chance(300)
        m = @rng.range(2, 7)
        "id % #{m} = #{@rng.range(0, m - 1)}"
      else
        lo = @rng.range(1, t.next_id - 1)
        "id BETWEEN #{lo} AND #{lo + @rng.range(0, 30)}"
      end
    end

    # --- value expressions ----------------------------------------------------------------------

    def value_expr(name, type, role, x)
      return x if name == "id"

      case type
      when "i32" then "(#{x} * #{@rng.range(1, 97)} + #{@rng.range(0, 50)}) % 1000"
      when "i64" then role == :key ? "#{x} * 7" : "#{x} * #{@rng.range(2, 1_000_003)}"
      when "text" then text_expr(x)
      when "bytea" then bytea_expr(x)
      end
    end

    def update_expr(name, type, _role)
      case type
      when "i32" then @rng.chance(500) ? "(#{name} + 1) % 1000" : "(id * #{@rng.range(1, 97)}) % 1000"
      when "i64" then "id * #{@rng.range(2, 1_000_003)}"
      when "text" then @rng.chance(300) ? "#{name} || '#{'x' * @rng.range(1, 40)}'" : text_expr("id")
      when "bytea" then bytea_expr("id")
      end
    end

    def text_expr(x)
      case @rng.weighted(@p["values"])
      when "short" then "md5(#{x}::text)"
      when "long_compressible" then "repeat(md5(#{x}::text), #{@rng.range(4, 60)})"
      when "long_incompressible"
        salt = @rng.range(0, 999)
        (1..@rng.range(4, 24)).map { |i| "md5(#{x}::text || '#{salt}.#{i}')" }.join(" || ")
      when "uuid" then "uuidv4()::text"
      else "NULL"
      end
    end

    def bytea_expr(x)
      case @rng.weighted(@p["values"].slice("short", "long_compressible", "null"))
      when "short" then "decode(md5(#{x}::text), 'hex')"
      when "long_compressible" then "decode(repeat(md5(#{x}::text), #{@rng.range(4, 40)}), 'hex')"
      else "NULL"
      end
    end

    # --- reads ----------------------------------------------------------------------------------

    def read_query
      if @writer && !@aborted && (@readers.empty? || @rng.chance(300))
        t = pick_table or return
        on(read_sql(t.name))
        return
      end
      return if @readers.empty?

      sid = @rng.pick(@readers.keys.sort)
      names = @readers[sid]
      return if names.empty?

      emit("on #{sid} #{read_sql(@rng.pick(names))}")
    end

    def read_sql(table)
      case @rng.range(0, 3)
      when 0 then "SELECT count(*), sum(id) FROM #{table}"
      when 1 then "SELECT * FROM #{table} WHERE id = #{@rng.range(1, 300)}"
      when 2 then "SELECT * FROM #{table} WHERE id BETWEEN #{(lo = @rng.range(1, 300))} AND #{lo + 20} ORDER BY id"
      else "SELECT * FROM #{table} ORDER BY id LIMIT #{@rng.range(1, 10)}"
      end
    end
  end

  def self.generate(profile_name, seed, steps: nil)
    Generator.new(profile(profile_name), seed, steps: steps).generate
  end
end

if $PROGRAM_NAME == __FILE__
  opts = { profile: "mixed", seed: 1, steps: nil }
  OptionParser.new do |o|
    o.banner = "usage: ruby scripts/sim_gen.rb [--profile NAME] [--seed N] [--steps N]"
    o.on("--profile NAME", "workload profile (#{SimGen.profile_names.join(', ')})") { |v| opts[:profile] = v }
    o.on("--seed N", Integer) { |v| opts[:seed] = v }
    o.on("--steps N", Integer) { |v| opts[:steps] = v }
  end.parse!
  print SimGen.generate(opts[:profile], opts[:seed], steps: opts[:steps])
end
