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
    "work_mem" => [0],
    "temp_buffers" => [0],
    "max_query_memory_bytes" => [0],
    "max_storage_bytes" => [0],
    "synchronous" => ["on"],
    "encrypted" => [0],
    "conversion" => 0,   # S13: the permille chance a rekey decrypts its database instead
    "gist" => 0,         # S13: i32range columns with GiST indexes, for the GiST child pointer
    "replay_after_commit" => 0, # S13: the permille chance an encrypted trace replays right after a commit
    "steps" => 300,
    "max_tables" => 3,
    "max_rows" => 300,
    "batch" => [1, 40],
    "max_readers" => 3,
    "processes" => "single",
    "max_procs" => 3,
    "attachments" => 0,
    "attached_writes" => 0,
    "weights" => {
      "open_write" => 10, "open_read" => 6, "close_read" => 5, "read_query" => 8,
      "insert" => 14, "update" => 9, "delete" => 5, "upsert" => 4, "update_narrow" => 0,
      "create_table" => 2, "drop_table" => 1, "create_index" => 2, "drop_index" => 1,
      "add_column" => 1, "analyze" => 1, "reindex" => 1, "sequence" => 1, "invalid" => 1,
      "commit" => 9, "rollback" => 2, "check" => 2, "reopen" => 1, "clock" => 1,
      "crash_power" => 1, "crash_process" => 1, "compact" => 0, "backup" => 0,
      "spawn" => 0, "exit" => 0, "kill" => 0, "disconnect" => 0,
      "attach" => 0, "corrupt" => 0, "temp_table" => 0, "temp_insert" => 0, "temp_read" => 0,
      "change_key" => 0, "rekey" => 0, "replay" => 0, "restore" => 0
    },
    "values" => {
      "short" => 10, "long_compressible" => 3, "long_incompressible" => 3, "null" => 2, "uuid" => 1,
      "long_repeated" => 0, "long_chain" => 0
    },
    "faults" => {
      "commit_fault" => 150, "chain" => 150, "power_loss" => 5, "process_crash" => 3,
      "io_error_write" => 1, "io_error_sync" => 1, "read_error" => 0, "enospc" => 0, "disconnect" => 0,
      "compact_fault" => 300, "backup_fault" => 0, "nth_max" => 1, "attach_fault" => 0, "spill_fault" => 0,
      "key_fault" => 0,
      "survive" => [100, 900], "tear" => [0, 400], "issue" => [200, 900], "io_p" => [50, 500],
      "flips" => [1, 3], "reseal" => 0, "frames_max" => 8
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

  # The pointer kinds a `replay` step targets (simulation-testing.md §14 S13; encrypted.md §2): the page
  # each names. A kind with no candidate page when the step runs is skipped.
  REPLAY_KINDS = %w[catalog catalog_next free free_next manifest root child gist_child
                    overflow overflow_lz4 overflow_next].freeze

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
      @service = @p["processes"] == "service" # an owner and page clients over one file (slice S8)
      @procs = [0]          # the running processes
      @pid_of = {}          # session id => its process
      # File attachments (slice S4): name => :yes (attached, its file durable) or :maybe (a create
      # that crashed: the file may or may not exist). Tables in an attachment are keyed `aN.tM`.
      @attached = {}
      @txdb = nil           # the durable database the open write transaction writes, once it writes one
      @temps = {}           # the write session's temp tables: name => next id
      @knobs = {}           # the handle settings drawn for the header
      @backups = 0          # backup steps emitted: a restore needs one (the runner skips it otherwise)
    end

    def generate
      page_size = @rng.pick(@p["page_sizes"])
      cache = @rng.pick(@p["cache_bytes"])
      header = ["# format: simulation", "# profile: #{@p['name']}", "# seed: #{@seed}",
                "# page_size: #{page_size}"]
      header << "# cache_bytes: #{cache}" if cache.positive?
      # Handle settings (slice S3). A single-valued list draws nothing, so profiles that leave them at
      # the default generate exactly the traces they did before the settings existed.
      %w[work_mem temp_buffers max_query_memory_bytes max_storage_bytes].each do |knob|
        value = knob_value(@p[knob])
        header << "# #{knob}: #{value}" if value.positive?
        @knobs[knob] = value
        # Under a storage or memory limit a commit can fail for reasons the model cannot predict
        # (54P06, or a 54P05 that aborted the block), so every commit is as uncertain as a faulted one.
        @limited ||= value.positive? && !%w[work_mem temp_buffers].include?(knob)
      end
      # The durability mode (validated-cow.md §4). Drawn like the handle settings above, so a profile
      # that leaves it at "on" generates exactly the traces it did before the mode existed.
      synchronous = knob_value(@p["synchronous"])
      header << "# synchronous: #{synchronous}" if synchronous != "on"
      # Encryption at rest (slice S12): main is encrypted under host key 0, its key entropy seeded by the
      # trace's seed, so the cores' images stay byte-identical.
      @encrypted = knob_value(@p["encrypted"]) == 1
      header << "# encrypted: 1" if @encrypted
      header << "# processes: service" if @service
      header << "# clock: 1700000000000000"
      bootstrap
      step_once while @lines.length < @steps
      finish
      (header + @lines).join("\n") + "\n"
    end

    private

    def emit(line) = @lines << line

    def knob_value(choices) = choices.length == 1 ? choices.first : @rng.pick(choices)

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
      when "compact" then compact
      when "change_key" then key_step("change_key") if @encrypted
      when "rekey" then key_step("rekey") if @encrypted
      when "replay" then replay if @encrypted
      when "restore" then restore
      when "backup" then backup
      when "spawn" then spawn if @service
      when "exit" then exit_process if @service
      when "kill" then kill_process_step if @service
      when "disconnect" then emit("disconnect p#{@rng.pick(@procs)}") if @service
      when "attach" then attach
      when "corrupt" then corrupt
      when "temp_table", "temp_insert", "temp_read" then temp_statement(action)
      else
        writer_statement(action)
      end
    end

    # --- sessions -------------------------------------------------------------------------------

    def open_write
      @writer = fresh("w")
      @working = @committed.transform_values(&:deep_dup)
      @aborted = false
      @txdb = nil
      @temps = {}
      emit("open #{@writer} write#{in_process(@writer)}")
    end

    def open_read
      return if @readers.length >= @p["max_readers"]

      sid = fresh("r")
      @readers[sid] = @committed.keys.sort
      emit("open #{sid} read#{in_process(sid)}")
    end

    # In a service trace a session opens in a random running process (` p<N>`); otherwise in p0, with
    # no suffix and no draw.
    def in_process(sid)
      return "" unless @service

      pid = @rng.pick(@procs)
      @pid_of[sid] = pid
      " p#{pid}"
    end

    # --- processes (slice S8) ---------------------------------------------------------------------

    # A process spawns as the owner when none runs, otherwise as a page client of the owner.
    def spawn
      free = (0...@p["max_procs"]).reject { |pid| @procs.include?(pid) }
      return if free.empty?

      pid = @rng.pick(free)
      @procs = (@procs + [pid]).sort
      emit("spawn p#{pid}")
    end

    # A clean exit rolls back the process's writer and closes its readers. The last process stays.
    def exit_process
      return if @procs.length < 2

      pid = @rng.pick(@procs)
      emit("exit p#{pid}")
      end_process(pid)
    end

    def kill_process_step
      pid = @rng.pick(@procs)
      emit("kill p#{pid}")
      @after_crash = true
      end_process(pid)
    end

    # end_process forgets pid's sessions. When no process is left, the runner reopens p0.
    def end_process(pid)
      @readers.delete_if { |sid, _| @pid_of[sid] == pid }
      if @writer && @pid_of[@writer] == pid
        @writer = nil
        @working = nil
      end
      @procs.delete(pid)
      @procs = [0] if @procs.empty?
    end

    # A fault on a step kills the owner, the only process that touches the file; a power loss kills
    # them all (S8). The model does not track which process owns, so it forgets only the faulted
    # writer; the runner skips any later step that names a session of the dead owner. A disconnect
    # kills no process. In a single-process trace every fault ends every session.
    def fault_kills(line, pid)
      if line.start_with?("fault disconnect")
        return
      elsif !@service || line.start_with?("fault power_loss") || @procs.length < 2
        kill_sessions
      else
        @after_crash = true
        if @writer && @pid_of[@writer] == pid
          @writer = nil
          @working = nil
        end
      end
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
      line = fault_line if faulted
      emit(line) if faulted
      emit("commit #{@writer}")
      if faulted
        merge_uncertain
        fault_kills(line, @pid_of[@writer])
        @writer = nil
        @working = nil
      elsif @limited
        merge_uncertain
        @writer = nil
        @working = nil
      else
        @committed = @working unless @aborted
        @writer = nil
        @working = nil
        # Right after a commit the newest manifest is that commit's, so a big commit's overflow pages
        # and its many listed pages are there to replay (S13).
        # After a drop or a delete, half of them target the free list it just grew.
        rate = @p["replay_after_commit"]
        if @encrypted && rate.positive? && @rng.chance(rate)
          replay(@freed && @rng.chance(500) ? @rng.pick(%w[free free_next]) : nil)
        end
      end
      @freed = false
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
      @procs = [0]
      emit("reopen")
    end

    # Host compaction (api.md §2.6). An open session makes it fail 55006, which is worth seeing too. A
    # fault may land at any of its barriers: the temp file's sync, then the directory's.
    def compact
      faulted = @rng.chance(@p.dig("faults", "compact_fault"))
      if faulted
        kind = @rng.chance(500) ? "power_loss survive=#{draw('survive')} tear=#{draw('tear')}" : "process_crash issue=#{draw('issue')}"
        line = "fault #{kind} nth=#{@rng.range(1, 3)}"
        emit(line)
      end
      dbs = @attached.select { |_, v| v == :yes }.keys.sort
      db = dbs.empty? || @rng.chance(500) ? nil : @rng.pick(dbs)
      emit(db ? "compact #{db}" : "compact")
      fault_kills_owner(line) if faulted
    end

    # A fault on a host step that runs in the lowest running process (compaction, a key step) kills the
    # owner of whichever database it touched, or with one process or a power loss every process. The
    # model cannot name that owner, so it forgets sessions only when every process dies: a writer in a
    # process that died is then skipped by the runner, which costs only the steps that name it.
    def fault_kills_owner(line)
      if !@service || line.start_with?("fault power_loss") || @procs.length < 2
        kill_sessions
      else
        @after_crash = true
      end
    end

    # A credential change or a rekey (slice S12, encryption.md §5) to host key 1–3 of main or, half the
    # time when one is attached, a file attachment (S13). A rekey may instead decrypt the database
    # (`plain`, the profile's `conversion` chance), and a rekey of a plaintext database encrypts it. A
    # fault may land at any of the change's barriers (between its two key-block writes) or the rekey's
    # (as compaction's); recovery must then open with exactly one of the old and new keys. A rekey fails
    # 55006 with a session open, and a credential change of a plaintext database 28P03, which the model
    # records like any outcome.
    def key_step(kind)
      faulted = @rng.chance(@p.dig("faults", "key_fault"))
      if faulted
        k = @rng.chance(500) ? "power_loss survive=#{draw('survive')} tear=#{draw('tear')}" : "process_crash issue=#{draw('issue')}"
        line = "fault #{k} nth=#{@rng.range(1, 3)}"
        emit(line)
      end
      dbs = @attached.select { |_, v| v == :yes }.keys.sort
      db = dbs.empty? || @rng.chance(500) ? nil : @rng.pick(dbs)
      target = kind == "rekey" && @p["conversion"].positive? && @rng.chance(@p["conversion"]) ? "plain" : @rng.range(1, 3)
      emit(["#{kind} #{target}", db].compact.join(" "))
      fault_kills_owner(line) if faulted
    end

    # Replace a genuine page of main with an older genuine version of it, at a page the given pointer
    # kind names (S13, encrypted.md §2). Like corrupt, the runner ends every session cleanly first and
    # puts the page back afterwards.
    def replay(kind = nil)
      return if @service

      emit("replay #{kind || @rng.pick(REPLAY_KINDS)}")
      @writer = nil
      @working = nil
      @readers.clear
    end

    # Copy one of main's earlier backups over it (S13): the j-th newest that succeeded, or the oldest.
    # Every process ends cleanly first. Main goes back to the backup's generation, so every later
    # commit diverges from the history the backup was taken in; whatever the model knew of main's
    # tables since is uncertain.
    def restore
      return if @backups.zero?

      emit("restore #{@rng.range(1, 3)}")
      @writer = nil
      @working = nil
      @readers.clear
      @procs = [0]
      @committed.each_value { |t| t.maybe = true if db_of(t.name) == "main" }
    end

    # A backup of main (page-service.md §9) in either form, from a random running process: it pins
    # like a reader and never waits for the writer. A fault may land in its file writes (the temp
    # file's sync, the directory's) or, in a service trace, end the connection its page reads use.
    def backup
      form = @rng.chance(500) ? "page_identical" : "compacted"
      @backups += 1
      pid = @service ? @rng.pick(@procs) : 0
      faulted = @rng.chance(@p.dig("faults", "backup_fault"))
      if faulted
        kind = if @service && @rng.chance(400)
                 "disconnect nth=#{@rng.range(1, @p.dig("faults", "frames_max") || 8)}"
               elsif @rng.chance(500) || (@service && @procs.length > 1)
                 # With several processes a process crash kills only the owner, which the model
                 # cannot name, so it would lose track of the running processes; a power loss kills
                 # them all.
                 "power_loss survive=#{draw('survive')} tear=#{draw('tear')} nth=#{@rng.range(1, 2)}"
               else
                 "process_crash issue=#{draw('issue')} nth=#{@rng.range(1, 2)}"
               end
        line = "fault #{kind}"
        emit(line)
      end
      # A faulted backup runs in the lowest running process (no ` p<N>`), so the step never skips and
      # its fault always fires: the model must not forget sessions a skipped step left alive.
      emit(@service && !faulted ? "backup #{form} p#{pid}" : "backup #{form}")
      return unless faulted

      # The fault kills the owner (or, on a power loss or with one process, every process), not
      # necessarily the backup's process, so a writer elsewhere may live on: forget the sessions only
      # when every process dies.
      if line.start_with?("fault disconnect")
        nil # a lost connection kills no process
      elsif !@service || line.start_with?("fault power_loss") || @procs.length < 2
        kill_sessions
      else
        @after_crash = true
      end
    end

    def crash_power
      emit("crash power survive=#{draw('survive')} tear=#{draw('tear')}")
      kill_sessions
    end

    # --- attachments, corruption, and temp tables -----------------------------------------------

    def db_of(name) = name.include?(".") ? name.split(".").first : "main"

    # A database a write may target: main, or an attachment known to be attached.
    def usable?(db) = db == "main" || @attached[db] == :yes

    # Attach a file database (creating its file if it does not exist). Attaching takes the writer gate,
    # so it waits for no writer; a fault may crash create at its temp-file or directory barrier. In a
    # service trace every running process attaches it, main's owner first (page-service.md §5.5), and a
    # fault kills that owner like any other.
    def attach
      return if @writer || @p["attachments"].zero?

      name = "a#{@rng.range(1, @p['attachments'])}"
      faulted = @attached[name] != :yes && @rng.chance(@p.dig("faults", "attach_fault"))
      if faulted
        kind = @rng.chance(500) ? "power_loss survive=#{draw('survive')} tear=#{draw('tear')}" : "process_crash issue=#{draw('issue')}"
        line = "fault #{kind} nth=#{@rng.range(1, 3)}"
        emit(line)
      end
      emit("attach #{name}")
      was = @attached[name]
      if faulted
        @service ? fault_kills(line, nil) : kill_sessions
        @attached[name] = :maybe
      else
        @attached[name] = :yes
      end
      # A crashed create may have left no file: whatever the model knew of its tables is uncertain.
      return if was == :yes

      @committed.each_value { |t| t.maybe = true if db_of(t.name) == name }
    end

    # Bit rot in the main file (simulation-testing.md §6.3): the runner ends every session cleanly,
    # flips bits, reads everything back, and repairs the bits.
    def corrupt
      return if @service

      reseal = @rng.chance(@p.dig("faults", "reseal")) ? " reseal=1" : ""
      emit("corrupt flips=#{@rng.range(*@p.dig('faults', 'flips'))}#{reseal}")
      @writer = nil
      @working = nil
      @readers.clear
    end

    # Session-local temp tables in the write session: past temp_buffers they spill to a scratch file
    # (temp-tables.md §6), whose operations a spill fault can fail.
    def temp_statement(action)
      return unless @writer && !@aborted

      case action
      when "temp_table"
        return if @temps.length >= 2

        name = fresh("tmp")
        @temps[name] = 1
        on("CREATE TEMP TABLE #{name} (id i64 PRIMARY KEY, v text)")
      when "temp_insert"
        return if @temps.empty?

        name = @rng.pick(@temps.keys.sort)
        lo = @temps[name]
        hi = lo + @rng.range(10, 120) - 1
        @temps[name] = hi + 1
        spill_on("INSERT INTO #{name} SELECT g, #{text_expr('g')} FROM generate_series(#{lo}, #{hi}) g")
      else
        return if @temps.empty?

        name = @rng.pick(@temps.keys.sort)
        # The last shape sorts every row, so a small work_mem spills its runs.
        sql = case @rng.range(0, 2)
              when 0 then "SELECT count(*), sum(length(v)) FROM #{name}"
              when 1 then "SELECT id, v FROM #{name} ORDER BY md5(v) LIMIT #{@rng.range(5, 40)}"
              else "SELECT id, md5(v) FROM #{name} ORDER BY md5(v), id"
              end
        spill_on(sql)
      end
    end

    # A statement that may spill, armed with a spill fault at the profile's rate. A fault ends the
    # process, like every fault.
    def spill_on(sql, sid = @writer, knob = "temp_buffers")
      rate = @p.dig("faults", "spill_fault")
      faulted = rate.positive? && @knobs[knob].to_i.positive? && @rng.chance(rate)
      emit("fault spill_error p=#{draw('io_p')}") if faulted
      emit("on #{sid} #{sql}")
      fault_kills("fault spill_error", @pid_of[sid]) if faulted
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
      @procs = [0]
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

    # ` nth=K` for a commit's crash fault, so it can land at a later barrier than the first (after a
    # stabilization sync, say). Drawn only when the profile asks, so other traces are unchanged.
    def nth
      max = @p.dig("faults", "nth_max")
      max > 1 ? " nth=#{@rng.range(1, max)}" : ""
    end

    def fault_line
      kinds = %w[power_loss process_crash io_error_write io_error_sync read_error enospc disconnect]
      case @rng.weighted(@p["faults"].slice(*kinds))
      when "power_loss" then "fault power_loss survive=#{draw('survive')} tear=#{draw('tear')}#{nth}"
      when "process_crash" then "fault process_crash issue=#{draw('issue')}#{nth}"
      when "io_error_write" then "fault io_error op=write p=#{draw('io_p')}"
      when "read_error" then "fault read_error p=#{draw('io_p')}"
      when "enospc" then "fault enospc"
      when "disconnect" then "fault disconnect nth=#{@rng.range(1, @p.dig("faults", "frames_max") || 8)}"
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

      # A transaction that has not chosen its database may open an attachment with a new table.
      rate = @p["attached_writes"]
      if @txdb.nil? && rate.positive? && @attached.value?(:yes) && @rng.chance(rate / 4) &&
         @working.keys.none? { |n| db_of(n) != "main" && usable?(db_of(n)) }
        return create_table
      end
      case action
      when "insert" then insert
      when "update" then update
      when "update_narrow" then update_narrow
      when "delete" then delete
      when "upsert" then upsert
      when "create_table" then create_table
      when "drop_table" then drop_table
      when "create_index" then create_index
      when "drop_index" then drop_index
      when "add_column" then add_column
      when "analyze" then analyze
      when "reindex" then reindex
      when "sequence" then sequence
      when "invalid" then invalid
      end
    end

    def on(sql) = emit("on #{@writer} #{sql}")

    # A table the working transaction can use, re-established first if its existence is uncertain.
    def pick_table(only_main: false)
      names = @working.keys.select do |n|
        db = db_of(n)
        usable?(db) && (@txdb.nil? || db == @txdb) && (!only_main || db == "main")
      end
      return nil if names.empty?

      # A transaction that has not chosen its database yet prefers the attachments' tables.
      rate = @p["attached_writes"]
      if @txdb.nil? && rate.positive? && names.any? { |n| db_of(n) != "main" } && @rng.chance(rate)
        names = names.reject { |n| db_of(n) == "main" }
      end
      t = @working[@rng.pick(names.sort)]
      @txdb = db_of(t.name)
      ensure_table(t)
      t
    end

    def ensure_table(t)
      return unless t.maybe

      on("CREATE TABLE IF NOT EXISTS #{t.name} (#{column_defs(t.cols.first(t.orig))})")
      t.cols.drop(t.orig).each do |name, type, _|
        on("ALTER TABLE #{t.name} ADD COLUMN IF NOT EXISTS #{name} #{type}")
      end
      t.indexes.sort.each { |n, (col, unique, gist)| on(index_sql(n, t.name, col, unique, gist)) }
      t.maybe = false
    end

    def column_defs(cols)
      cols.map { |name, type, _| name == "id" ? "id i64 PRIMARY KEY" : "#{name} #{type}" }.join(", ")
    end

    def index_sql(name, table, col, unique, gist = false)
      using = gist ? " USING gist" : ""
      "CREATE #{unique ? 'UNIQUE ' : ''}INDEX IF NOT EXISTS #{name} ON #{table}#{using} (#{col})"
    end

    # max_tables bounds each database's tables.
    def create_table
      dbs = @attached.select { |_, v| v == :yes }.keys.sort
      db = @txdb || (dbs.empty? || !@rng.chance(@p["attached_writes"]) ? "main" : @rng.pick(dbs))
      return if @working.keys.count { |n| db_of(n) == db } >= @p["max_tables"]

      @txdb = db
      name = db == "main" ? fresh("t") : "#{db}.#{fresh('t')}"
      cols = [["id", "i64", :key]]
      n = @rng.range(1, 4)
      n.times do |i|
        type = @rng.pick(@p["gist"].positive? ? %w[i32 i64 text text bytea i32range] : %w[i32 i64 text text bytea])
        cols << ["c#{i + 1}", type, type == "i64" ? :key : :plain]
      end
      t = Table.new(name: name, cols: cols, orig: cols.length, next_id: 1, indexes: {}, maybe: false)
      @working[name] = t
      on("CREATE TABLE #{name} (#{column_defs(cols)})")
    end

    # DROP TABLE and DROP INDEX name main's objects only (the grammar takes no database qualifier).
    def drop_table
      return if @working.length <= 1 || (@txdb && @txdb != "main")

      names = @working.keys.select { |n| db_of(n) == "main" }
      return if names.empty?

      t = @working[@rng.pick(names.sort)]
      @txdb = "main"
      @working.delete(t.name)
      @freed = true
      on("DROP TABLE IF EXISTS #{t.name}")
    end

    # A GiST index on a range column (S13), otherwise a B-tree on an integer one.
    def create_index
      t = pick_table or return
      cols = t.cols.drop(1).select { |_, type, _| %w[i32 i64 i32range].include?(type) }
      return if cols.empty?

      col, type, role = @rng.pick(cols)
      gist = type == "i32range"
      unique = role == :key && type == "i64" && @rng.chance(500)
      name = fresh("ix")
      t.indexes[name] = gist ? [col, unique, true] : [col, unique]
      on(index_sql(name, t.name, col, unique, gist).sub(" IF NOT EXISTS", ""))
    end

    def drop_index
      t = pick_table(only_main: true) or return
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

    # REINDEX TABLE rebuilds every index of the table in place (indexes.md §10): fresh index trees,
    # the old ones left as orphans for the commit's free-list reclamation.
    def reindex
      t = pick_table or return
      on("REINDEX TABLE #{t.name}")
    end

    # A sequence may have been lost with a rolled-back or faulted transaction, so every use
    # re-establishes it first; nextval is transactional (sequences.md), so its value needs no model.
    def sequence
      return if @txdb && @txdb != "main"

      @txdb = "main"
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
      t = pick_table(only_main: true) or return # ON CONFLICT is 0A000 on an attachment table
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

    # An UPDATE that assigns only a fixed-width column of a table that also has variable-width ones:
    # the rewrite carries each large value over in its stored form and keeps its overflow chain
    # (large-values.md §15). Half the time the WHERE also reads a variable-width column, which must
    # not stop the chain from being kept.
    def update_narrow
      t = pick_table or return
      return if t.next_id <= 1

      wide = t.cols.select { |_, type, _| %w[text bytea].include?(type) }
      narrow = t.cols.drop(1).select { |_, type, role| type == "i32" && role != :key }
      return if wide.empty? || narrow.empty?

      name = @rng.pick(narrow)[0]
      where = range_predicate(t)
      where = "(#{where}) AND #{@rng.pick(wide)[0]} IS NOT NULL" if @rng.chance(500)
      on("UPDATE #{t.name} SET #{name} = (#{name} + 1) % 1000 WHERE #{where}")
    end

    def delete
      t = pick_table or return
      return if t.next_id <= 1

      @freed = true
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
      when "i32range" then range_expr(x)
      end
    end

    def update_expr(name, type, _role)
      case type
      when "i32" then @rng.chance(500) ? "(#{name} + 1) % 1000" : "(id * #{@rng.range(1, 97)}) % 1000"
      when "i64" then "id * #{@rng.range(2, 1_000_003)}"
      when "text" then @rng.chance(300) ? "#{name} || '#{'x' * @rng.range(1, 40)}'" : text_expr("id")
      when "bytea" then bytea_expr("id")
      when "i32range" then range_expr("id")
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
      when "long_repeated"
        # Hex digests repeated a few times: LZ4 halves it or better, yet the compressed value still
        # outgrows the record, so it is stored compressed out of line (the 0x04 external pointer).
        salt = @rng.range(0, 999)
        parts = (1..@rng.range(16, 60)).map { |i| "md5(#{x}::text || '#{salt}.#{i}')" }.join(" || ")
        "repeat(#{parts}, #{@rng.range(2, 4)})"
      when "long_chain"
        # Incompressible and several pages long, so its overflow chain has successors.
        salt = @rng.range(0, 999)
        (1..@rng.range(40, 100)).map { |i| "md5(#{x}::text || '#{salt}.#{i}')" }.join(" || ")
      else "NULL"
      end
    end

    # A short range whose bounds follow the row (S13: the GiST index's keys), now and then empty.
    def range_expr(x)
      return "'empty'::i32range" if @rng.chance(50)

      lo = "(#{x} * #{@rng.range(1, 97)}) % 1000"
      "i32range(#{lo}, #{lo} + #{@rng.range(1, 40)})"
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
        read_on(@writer, read_sql(t.name))
        return
      end
      return if @readers.empty?

      sid = @rng.pick(@readers.keys.sort)
      names = @readers[sid]
      return if names.empty?

      read_on(sid, read_sql(@rng.pick(names)))
    end

    # The sorting and grouping shapes spill under a small work_mem, so a spill fault may arm them.
    def read_on(sid, sql)
      return emit("on #{sid} #{sql}") unless sql.include?("ORDER BY md5") || sql.include?("GROUP BY")

      spill_on(sql, sid, "work_mem")
    end

    # The last two shapes sort and group on a non-key expression, so a small work_mem spills them.
    def read_sql(table)
      case @rng.range(0, 5)
      when 0 then "SELECT count(*), sum(id) FROM #{table}"
      when 1 then "SELECT * FROM #{table} WHERE id = #{@rng.range(1, 300)}"
      when 2 then "SELECT * FROM #{table} WHERE id BETWEEN #{(lo = @rng.range(1, 300))} AND #{lo + 20} ORDER BY id"
      when 3 then "SELECT * FROM #{table} ORDER BY id LIMIT #{@rng.range(1, 10)}"
      when 4 then "SELECT * FROM #{table} ORDER BY md5(id::text) LIMIT #{@rng.range(5, 40)}"
      else "SELECT id % #{@rng.range(2, 9)} AS k, count(*), max(id) FROM #{table} GROUP BY 1"
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
