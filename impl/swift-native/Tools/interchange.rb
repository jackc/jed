# frozen_string_literal: true

require_relative "swift_task"
require "digest"
require "json"
require "open3"
require "set"
require "tmpdir"

# Opt-in public-host interchange. No engine or default task imports this file.
module SwiftInterchange
  BASELINE = "362e0798ac3a8bd04bd2da556044b6ec73bf468a"
  FIXTURES = File.join(SwiftNativeTasks::PACKAGE, "Tests/JedTests/Fixtures")
  TOOLS = File.join(SwiftNativeTasks::PACKAGE, "Tools/Interop")
  PEERS = %w[rust go ts].freeze
  KEY = "4a" * 32
  PASS = "Jed interchange test passphrase — 2026".unpack1("H*")
  CHECKERS = {
    "verify.rb" => "308cbc629fbabad4792a75de0ea75de6646f4b5898fd5302db798243ea91ebb9",
    "lz4.rb" => "c4eb2a3fd9fe6d455d83351deb6512f8277661f1b6c3e287125e65417663e609",
    "xaes.rb" => "aeebcb1541f0c3312006852ec0928ce2aef0fa50fce328bc0b8bbde24b7d1882",
    "expr.rb" => "7b09424c5beb719a4527d8fed113010cd219bfc55752b1ccf9120bdff6111e76"
  }.freeze
  CATALOGS = {
    "jed_tables" => %w[name], "jed_columns" => %w[table_name name ordinal type not_null pk_ordinal column_default identity_generation],
    "jed_indexes" => %w[name table_name columns is_unique method predicate descending nulls_first],
    "jed_constraints" => %w[name table_name type columns expression ref_table ref_columns operators match_type],
    "jed_statistics" => %w[table_name column_name analyzed_rows is_stale null_count distinct_count sample_rows average_width mcv_count histogram_count term_count],
    "jed_sequences" => %w[name start_value min_value max_value increment_by cycle cache_size last_value owned_by_table owned_by_column],
    "jed_types" => %w[name kind], "jed_type_fields" => %w[type_name name ordinal type not_null],
    "jed_views" => %w[name definition columns check_option security_barrier security_invoker]
  }.freeze
  OPEN_ERRORS = %w[cow_invalid_free_list malformed_fk_type malformed_missing_type malformed_interior_cycle].freeze
  LAZY = {
    "malformed_leaf_directory" => ["SELECT v::text FROM t WHERE id=1", [["10"]], "SELECT id,v FROM t ORDER BY id"],
    "malformed_wrong_tree_leaf" => ["SELECT id::text,a::text,b::text,c::text FROM u ORDER BY id", [["1","x","y","true"],["2","z",nil,"false"]], "SELECT id,v FROM t ORDER BY id"],
    "malformed_decimal_group" => ["SELECT count(*)::text FROM t", [["5"]], "SELECT d FROM t WHERE id=2"],
    "malformed_overflow_cycle" => ["SELECT id::text FROM t ORDER BY id", [["1"],["2"],["3"]], "SELECT body,blob FROM t WHERE id=1"],
    "malformed_row_count" => ["SELECT v::text FROM t WHERE id=20", [["200"]], "DELETE FROM t"]
  }.freeze
  PROBE = "__swift_interop_probe"
  RANGE_TYPES = %w[i32range i64range numrange daterange tsrange tstzrange].freeze
  # The untouched opaque-definition fixture cannot materialize jed_columns,
  # even with a table filter: its invalid views are planned before filtering.
  # These authored schemas come from the SHA-pinned fixture generator's
  # UNPARSABLE_DEFINITIONS value (spec/fileformat/verify.rb:1846–1873).
  OPAQUE_COLUMNS = {
    "c" => [%w[id i32], %w[v i32]], "d" => [%w[id i32], %w[v i32], %w[k i32]],
    "e" => [%w[id i32], %w[r i32range]], "p" => [%w[id i32], %w[v i32]],
    "t" => [%w[id i32], %w[v i32]], "x" => [%w[id i32], %w[v i32]]
  }.freeze
  PROBE_COLUMNS = [%w[id i64], %w[v text], %w[n decimal(9,2)], %w[payload bytea]].freeze
  module_function

  def inventory
    snapshot = JSON.parse(File.read(File.join(FIXTURES, "SpecSnapshot.json")))
    raise "unexpected frozen baseline" unless snapshot.fetch("baseline") == BASELINE
    hashes = snapshot.fetch("source_hashes").select { |path, _| path.start_with?("spec/fileformat/fixtures/") && path.end_with?(".jed") }
    raise "frozen fixture inventory must contain 112 images" unless hashes.length == 112
    entries = hashes.sort.map do |path, hash|
      name = File.basename(path, ".jed")
      file = File.join(FIXTURES, File.basename(path))
      raise "missing/modified frozen fixture #{name}" unless File.file?(file) && Digest::SHA256.file(file).hexdigest == hash
      kind = if OPEN_ERRORS.include?(name) then "open-rejection"
             elsif LAZY.key?(name) then "lazy-corruption"
             elsif name == "collation_skew_corrupt" then "skew-repair"
             elsif name == "unparsable_definitions" then "opaque-definitions"
             elsif name.start_with?("cow_", "torn_meta_") then "recovery"
             else "ordinary" end
      { name: name, path: file, source: path, sha256: hash, kind: kind,
        credential: name.start_with?("encrypted_") ? ["key", KEY] : ["plain", ""],
        mutation: !%w[open-rejection lazy-corruption].include?(kind) }
    end
    # Every byte fixture class remains explicit, including those which cannot be
    # legally rewritten before the corruption is reported.
    raise "missing special fixture" unless (OPEN_ERRORS + LAZY.keys + %w[collation_skew_corrupt unparsable_definitions] - entries.map { _1[:name] }).empty?
    %w[unicode.jucd tzdata.jtz].each do |name|
      path, hash = snapshot.fetch("source_hashes").find { |path, _| File.basename(path) == name }
      raise "missing frozen environment #{name}" unless path && Digest::SHA256.file(File.join(FIXTURES, name)).hexdigest == hash
    end
    evidence = JSON.parse(File.read(File.join(TOOLS, "fixtures.json")))
    raise "applicability baseline differs" unless evidence.fetch("baseline") == BASELINE
    annotated = evidence.fetch("fixtures")
    raise "applicability names differ" unless annotated.map { _1.fetch("file") }.sort == entries.map { _1[:name] + ".jed" }.sort
    entries.each do |entry|
      details = annotated.find { _1.fetch("file") == entry[:name] + ".jed" }
      raise "applicability hash differs: #{entry[:name]}" unless details.fetch("sha256") == entry[:sha256]
      raise "applicability mutation differs" unless details.fetch("sidecar_mutation") == entry[:mutation]
      if entry[:kind] == "opaque-definitions"
        raise "opaque fixture schema differs" unless details.fetch("base_columns") == OPAQUE_COLUMNS
      end
      entry[:applicability] = details
    end
    entries
  end

  def checker_paths
    CHECKERS.map do |name, hash|
      path = File.join(SwiftNativeTasks::ROOT, "spec/fileformat", name)
      raise "independent checker changed: #{name}" unless Digest::SHA256.file(path).hexdigest == hash
      path
    end
  end

  class Actor
    attr_reader :core
    def initialize(core, command, action, path, credential, environment: "full")
      @core = core
      @stdin, @stdout, @stderr, @wait = Open3.popen3(*command, action, path, FIXTURES, *credential, environment)
      @errors = +""
      @stderr_reader = Thread.new do
        while (part = @stderr.read(8192))
          @errors << part.byteslice(0, [1_048_576 - @errors.bytesize, 0].max)
        end
      rescue IOError
        # Cleanup may close a pipe after forcibly terminating a faulty actor.
      end
      @pending = +""
      line = read_line
      return if line == "READY"
      result = parse(line)
      raise OpenFailure.new(result[1], result[2]) if result[0] == :error
      raise "#{core}: actor did not announce READY"
    rescue Exception
      cleanup
      raise
    end
    class OpenFailure < StandardError
      attr_reader :code
      def initialize(code, detail); @code = code; super("open #{code}: #{detail}"); end
    end
    def read_line
      deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 120
      loop do
        if (end_at = @pending.index("\n"))
          return @pending.slice!(0..end_at).delete_suffix("\n").delete_suffix("\r")
        end
        remaining = deadline - Process.clock_gettime(Process::CLOCK_MONOTONIC)
        raise "#{core}: actor response timeout" if remaining <= 0 || !IO.select([@stdout], nil, nil, remaining)
        piece = @stdout.read_nonblock(65_536, exception: false)
        next if piece == :wait_readable
        raise "#{core}: actor exited without reply #{@errors}" if piece.nil?
        @pending << piece
        raise "#{core}: actor reply exceeds64MiB" if @pending.bytesize > 64 * 1024 * 1024
      end
    end
    def parse(line)
      fields = line.split("\t", -1)
      return [:ok, fields[1]] if fields.length == 2 && fields[0] == "OK"
      if fields.length == 3 && fields[0] == "ERR" && fields[1].match?(/\A[0-9A-Z]{5}\z/) && fields[2].match?(/\A(?:[0-9a-f]{2})*\z/)
        return [:error, fields[1], [fields[2]].pack("H*").force_encoding(Encoding::UTF_8)]
      end
      raise "#{core}: malformed actor reply #{line.byteslice(0,120).inspect}"
    end
    def command(name, *args)
      @stdin.write(([name, *args].join("\t") + "\n")); @stdin.flush
      parse(read_line)
    end
    def expect(name, *args, error: nil)
      reply = command(name, *args)
      raise "#{core} #{name}: expected #{error || 'success'}, got #{reply.inspect}" unless error ? reply[0] == :error && reply[1] == error : reply[0] == :ok
      reply[1]
    end
    def execute(sql, error: nil) = expect("EXEC", sql.unpack1("H*"), error: error)
    def query(sql)
      payload = expect("QUERY", sql.unpack1("H*"))
      count, body = payload.split(";", 2)
      raise "#{core}: malformed row count" unless count&.match?(/\A(?:0|[1-9][0-9]*)\z/) && body
      rows = count == "0" ? [] : (body.empty? ? [""] : body.split(",", -1)).map { |row| (row.empty? ? [""] : row.split(":", -1)).map { |cell|
        next nil if cell == "-"
        raise "#{core}: malformed text cell" unless cell.match?(/\A(?:[0-9a-f]{2})*\z/)
        text = [cell].pack("H*").force_encoding(Encoding::UTF_8)
        raise "#{core}: invalid UTF-8 cell" unless text.valid_encoding?
        text
      } }
      raise "#{core}: mismatched row count" unless rows.length == Integer(count) && (count != "0" || body.empty?)
      rows
    end
    def close
      expect("CLOSE")
      raise "#{core}: failed to exit after close" unless @wait.join(15)
      raise "#{core}: nonzero actor exit #{@wait.value.exitstatus}: #{@errors}" unless @wait.value.success?
      @closed = true
    ensure
      cleanup
    end
    def cleanup
      if @wait&.alive?
        Process.kill("KILL", @wait.pid) rescue Errno::ESRCH
        @wait.join
      end
      [@stdin, @stdout, @stderr].compact.each { |io| io.close unless io.closed? }
      @stderr_reader&.join
    end
  end

  def quote(name) = '"' + name.gsub('"', '""') + '"'
  def literal(text) = "'" + text.gsub("'", "''") + "'"
  def ordered(rows) = rows.sort_by { JSON.generate(_1).b }
  def snapshot(actor, domain: "main", opaque: false)
    result = {}
    CATALOGS.each do |name, columns|
      if opaque && name == "jed_columns"
        actor.execute("SELECT name FROM jed_columns", error: "XX003")
        result["jed_columns_expected_error"] = "XX003"
        next
      end
      result[name] = ordered(actor.query("SELECT #{columns.map { quote(_1) + '::text' }.join(',')} FROM #{quote(domain)}.#{name}"))
    end
    tables = result.fetch("jed_tables").map(&:first)
    if opaque
      expected = OPAQUE_COLUMNS.keys + (tables.include?(PROBE) ? [PROBE] : [])
      raise "opaque fixture table inventory differs" unless tables.sort == expected.sort
    end
    tables.each do |table|
      column_sql = "SELECT name,type FROM #{quote(domain)}.jed_columns WHERE table_name=#{literal(table)} ORDER BY ordinal"
      columns = if opaque
        actor.execute(column_sql, error: "XX003")
        result["jed_columns_expected_error:" + table] = "XX003"
        table == PROBE ? PROBE_COLUMNS : OPAQUE_COLUMNS.fetch(table)
      else
        actor.query(column_sql)
      end
      raise "table #{table} has no columns" if columns.empty?
      rendered = columns.map do |name, type|
        column = quote(name)
        # Ranges have a canonical JSON string image but no explicit text cast.
        # Keep the ordinary text image for every other family so array bounds
        # and decimal display scale remain visible in semantic snapshots.
        RANGE_TYPES.include?(type) ? "CASE WHEN #{column} IS NULL THEN NULL::text ELSE to_json(#{column})::text END" : "#{column}::text"
      end
      result["table:" + table] = ordered(actor.query("SELECT #{rendered.join(',')} FROM #{quote(domain)}.#{quote(table)}"))
    end
    result
  end

  def mutate(actor, step, domain: "main")
    table = "#{quote(domain)}.#{quote(PROBE)}"
    if step == 1
      actor.execute("CREATE TABLE #{table}(id i64 PRIMARY KEY,v text,n decimal(9,2),payload bytea)")
      actor.execute("INSERT INTO #{table} VALUES(1,'first',12.50,decode('000102ff','hex')),(2,repeat('overflow-',400),-0.01,NULL)")
    else
      actor.execute("UPDATE #{table} SET v='second',n=22.75 WHERE id=1")
      actor.execute("INSERT INTO #{table} VALUES(3,'reopened',123.45,decode('deadbeef','hex'))")
      actor.execute("DELETE FROM #{table} WHERE id=2")
    end
  end

  def with_actor(core, commands, action, path, credential, environment: "full")
    actor = Actor.new(core, commands.fetch(core), action, path, credential, environment: environment)
    begin
      result = yield actor
      actor.close; actor = nil
      result
    ensure
      actor&.cleanup
    end
  end

  def check_image(path)
    stdout, stderr, status = Open3.capture3("ruby", checker_paths.first, "--check-image", path)
    lines = stdout.lines.reject { _1.strip.empty? }.map { JSON.parse(_1) }
    raise "independent image checker failed for #{path}: #{stderr} #{stdout}" unless status.success? && lines.length == 1 && lines[0]["file"] == path && !lines[0]["error"]
    info = lines[0]
    %w[version live pages free].each { |key| raise "invalid image checker #{key}" unless info[key].is_a?(Integer) && info[key] >= 0 }
    info
  end

  def committed_bytes(path, info)
    bytes = File.binread(path)
    raise "short meta" if bytes.bytesize < 64
    page_size = bytes.byteslice(8, 4).unpack1("N")
    raise "invalid page size" unless page_size >= 256 && page_size <= 65_536 && page_size & (page_size - 1) == 0
    extent = page_size * info.fetch("pages")
    raise "truncated committed image" if bytes.bytesize < extent
    bytes.byteslice(0, extent)
  end
end

module SwiftInterchange
  module_function
  def host_requirements(actor, entry)
    case entry[:kind]
    when "skew-repair"
      expected = [["1"],["3"],["4"],["2"]]
      raise "skew read used obsolete keys" unless actor.query('SELECT id::text FROM t ORDER BY name COLLATE "unicode"') == expected
      actor.execute("UPDATE t SET name=name WHERE id=1", error: "XX002")
    when "opaque-definitions"
      actor.execute("SELECT name FROM jed_columns", error: "XX003")
      actor.execute("SELECT id FROM u", error: "XX003")
      actor.execute("UPDATE c SET v=v", error: "XX003")
    end
  end

  def self_update(actor, entry)
    # Unknown CHECK/index SQL blocks its owning objects. Exercise the two
    # documented writable original tables without repairing stored SQL.
    if entry[:kind] == "opaque-definitions"
      actor.execute("UPDATE t SET v=v")
      actor.execute("UPDATE d SET v=v")
      return
    end
    actor.expect("UPGRADE") if entry[:kind] == "skew-repair"
    actor.query("SELECT name FROM jed_tables ORDER BY name").each do |row|
      table = row.fetch(0)
      next if table == PROBE
      cols = actor.query("SELECT name FROM jed_columns WHERE table_name=#{literal(table)} AND identity_generation IS NULL ORDER BY ordinal")
      raise "table #{table} has no writable field" if cols.empty?
      field = quote(cols.last.fetch(0))
      actor.execute("UPDATE #{quote(table)} SET #{field}=#{field}")
    end
  end

  def safe_probe(actor, entry)
    sql, expected, corruption = LAZY.fetch(entry[:name])
    raise "intact rows differ in #{entry[:name]}" unless actor.query(sql) == expected
    actor.execute(corruption, error: "XX001")
    raise "intact rows changed after corruption" unless actor.query(sql) == expected
  end

  def invoke_build(arguments, directory, log)
    File.open(log, "w") do |output|
      raise "build failed: #{log}" unless system(*arguments, chdir: directory, out: output, err: [:child, :out])
    end
  end

  def commands(output, peers, configuration, no_build, skip_swift_build)
    build_root = File.join(SwiftNativeTasks::BUILD, "interop")
    FileUtils.mkdir_p(build_root)
    unless no_build
      unless skip_swift_build
        env, argv = SwiftNativeTasks.command("build", configuration: configuration, extra: ["--product", "jed-interop"])
        invoke_build([env, *argv], SwiftNativeTasks::PACKAGE, File.join(output, "build-swift.log"))
      end
      if peers.include?("go")
        invoke_build(["go", "build", "-o", File.join(build_root,"go"), File.join(TOOLS,"go.go")], File.join(SwiftNativeTasks::ROOT,"impl/go"), File.join(output,"build-go.log"))
      end
      if peers.include?("rust")
        # Keep Cargo's wrapper bookkeeping in the evidence directory. Seed the
        # supported core's lock so this transport adds no dependency version.
        cargo = File.join(output, "rust-build"); FileUtils.mkdir_p(cargo)
        manifest = File.read(File.join(TOOLS,"rust/Cargo.toml"))
          .sub('path = "../rust.rs"', "path = #{JSON.generate(File.join(TOOLS, 'rust.rs'))}")
          .sub('path = "../../../../rust"', "path = #{JSON.generate(File.join(SwiftNativeTasks::ROOT, 'impl/rust'))}")
        File.write(File.join(cargo,"Cargo.toml"),manifest)
        original_lock = File.join(SwiftNativeTasks::ROOT,"impl/rust/Cargo.lock")
        FileUtils.copy_file(original_lock,File.join(cargo,"Cargo.lock"))
        invoke_build(["cargo","build","--offline","--release","--manifest-path",File.join(cargo,"Cargo.toml"),"--target-dir",File.join(build_root,"rust")], SwiftNativeTasks::ROOT, File.join(output,"build-rust.log"))
        raise "Rust wrapper changed approved dependency versions" unless (registry_packages(File.join(cargo,"Cargo.lock")) - registry_packages(original_lock)).empty?
      end
    end
    sources = {"swift"=>File.join(SwiftNativeTasks::BUILD,configuration,"jed-interop"),
      "go"=>File.join(build_root,"go"),"rust"=>File.join(build_root,"rust/release/jed-interop-rust"),"ts"=>File.join(TOOLS,"node.ts")}
    directory = File.join(output,"actors");FileUtils.mkdir_p(directory)
    manifest = {}
    commands = (["swift"] + peers).to_h do |core|
      source = sources.fetch(core)
      raise "missing #{core} actor #{source}" unless File.file?(source) && (core=="ts" || File.executable?(source))
      path = File.join(directory, core == "ts" ? "node.ts" : core)
      FileUtils.copy_file(source,path);File.chmod(0o700,path)
      manifest[core] = {path:path,sha256:Digest::SHA256.file(path).hexdigest}
      [core,core=="ts" ? ["node",path] : [path]]
    end
    ENV["JED_INTEROP_TS_TOOLING"] = File.join(SwiftNativeTasks::ROOT,"impl/ts/src/tooling.ts")
    File.write(File.join(output,"actors.json"),JSON.pretty_generate(manifest)+"\n")
    commands
  end

  def registry_packages(path)
    File.read(path).split("[[package]]").filter_map do |block|
      fields = %w[name version source checksum].map { |key| block[/^#{key} = "([^"]+)"$/, 1] }
      fields if fields[2]&.start_with?("registry+")
    end.to_set
  end

  def source_identity
    Dir.glob(File.join(SwiftNativeTasks::ROOT,"impl/ts/src/**/*.ts")).sort.to_h { |path| [path,Digest::SHA256.file(path).hexdigest] }
  end

  def record(records, key, **fields)
    raise "duplicate interchange phase #{key.inspect}" if records.key?(key)
    records[key] = {key:key,**fields}
  end

  def run_fixture(entry, peer, commands, output, records)
    name = entry.fetch(:name)
    directory = File.join(output,"images",name,peer);FileUtils.mkdir_p(directory)
    images = {}; snapshots = {}
    [["swift-first","swift",peer],["peer-first",peer,"swift"]].each do |direction,first,second|
      path = File.join(directory,"#{direction}.jed");FileUtils.copy_file(entry[:path],path)
      if entry[:kind] == "open-rejection"
        [first,second].each do |core|
          begin
            with_actor(core,commands,"open",path,entry[:credential]) { raise "invalid image unexpectedly opened" }
            raise "expected XX001"
          rescue Actor::OpenFailure => e
            raise "expected XX001, got #{e.code}" unless e.code == "XX001"
          end
          record(records,[name,peer,direction,"reject-#{core}"],code:"XX001")
        end
        raise "rejected image was modified" unless File.binread(path) == File.binread(entry[:path])
        next
      end
      if entry[:kind] == "lazy-corruption"
        [first,second].each do |core|
          with_actor(core,commands,"open",path,entry[:credential]) { safe_probe(_1,entry) }
          record(records,[name,peer,direction,"lazy-#{core}"],code:"XX001",intact_rows:true)
        end
        raise "lazy corrupt fixture changed after rejected operation" unless File.binread(path) == File.binread(entry[:path])
        next
      end
      initial = nil
      with_actor(first,commands,"open",path,entry[:credential]) do |actor|
        host_requirements(actor,entry)
        if entry[:kind] == "recovery"
          expected = entry.fetch(:applicability).fetch("probes").first.fetch("rows")
          raise "selected recovery root differs" unless actor.query("SELECT id::text,v::text FROM t ORDER BY id") == expected
        end
        initial = snapshot(actor,opaque:entry[:kind]=="opaque-definitions")
        raise "reserved probe table collides with fixture" if initial.fetch("jed_tables").include?([PROBE])
        self_update(actor,entry)
        mutate(actor,1)
        snapshots[[direction,0]] = initial
        snapshots[[direction,1]] = snapshot(actor,opaque:entry[:kind]=="opaque-definitions")
      end
      record(records,[name,peer,direction,"write-1"],core:first,source_readback:Digest::SHA256.hexdigest(JSON.generate(initial)))
      stage1 = path.sub(/\.jed\z/,"-stage1.jed");FileUtils.copy_file(path,stage1)
      info1=check_image(stage1);images[[direction,1]]=[stage1,info1]
      with_actor(second,commands,"open",path,entry[:credential]) do |actor|
        raise "foreign generation readback differs" unless snapshot(actor,opaque:entry[:kind]=="opaque-definitions") == snapshots[[direction,1]]
        self_update(actor,entry)
        mutate(actor,2)
        snapshots[[direction,2]]=snapshot(actor,opaque:entry[:kind]=="opaque-definitions")
      end
      record(records,[name,peer,direction,"foreign-write-2"],core:second)
      info2=check_image(path);images[[direction,2]]=[path,info2]
      with_actor(first,commands,"open",path,entry[:credential]) do |actor|
        raise "reopened final generation differs" unless snapshot(actor,opaque:entry[:kind]=="opaque-definitions") == snapshots[[direction,2]]
      end
      record(records,[name,peer,direction,"final-reopen"],core:first,checker:info2)
    end
    return if %w[open-rejection lazy-corruption].include?(entry[:kind])
    raise "initial fixture SQL snapshots differ" unless snapshots.fetch(["swift-first",0]) == snapshots.fetch(["peer-first",0])
    snapshots.each { |(direction, phase), state| File.write(File.join(directory, "#{direction}-sql-#{phase}.json"), JSON.pretty_generate(state) + "\n") }
    [1,2].each do |phase|
      a,ai=images.fetch(["swift-first",phase]);b,bi=images.fetch(["peer-first",phase])
      raise "phase#{phase} SQL snapshots differ between writer directions" unless snapshots.fetch(["swift-first",phase]) == snapshots.fetch(["peer-first",phase])
      raise "phase#{phase} exact committed image differs between writer directions" unless committed_bytes(a,ai) == committed_bytes(b,bi)
      record(records,[name,peer,"both","bytes-#{phase}"],sha256:Digest::SHA256.hexdigest(committed_bytes(a,ai)),checker:ai)
    end
  end

  GENERATED_SQL = [
    "CREATE TABLE payload(id i64 PRIMARY KEY,v text,n decimal(12,4),f f64,a i32[],r int8range)",
    "INSERT INTO payload VALUES(1,repeat('large-',1000),12.5000,'-0.0',ARRAY[1,NULL,3],'[1,9)'),(2,'small',-0.0001,'NaN','{}'::i32[],'empty')",
    "CREATE INDEX payload_n ON payload(n DESC)",
    "CREATE VIEW visible AS SELECT id,v FROM payload WHERE id>0",
    "CREATE SEQUENCE seq START 10",
    "CREATE TABLE hosted(id i64 PRIMARY KEY,v i64 CHECK(geo_hash(v)>0))",
    "INSERT INTO hosted VALUES(1,2)",
    "CREATE INDEX hosted_geo ON hosted(geo_hash(v))"
  ].freeze

  def checked_passphrase_image(core,commands,path,output)
    copy=File.join(output,File.basename(path,".jed")+"-key-check.jed");FileUtils.copy_file(path,copy)
    before=File.binread(copy);ps=before.byteslice(8,4).unpack1("N")
    with_actor(core,commands,"open",copy,["pass",PASS]) { _1.expect("REWRAP","key",KEY) }
    after=File.binread(copy)
    raise "credential-only rewrap changed meta/body bytes" unless before.bytesize==after.bytesize && before.byteslice(0,ps*2)==after.byteslice(0,ps*2) && before.byteslice(ps*4..)==after.byteslice(ps*4..)
    check_image(copy)
  end

  def run_generated(peer,commands,output,records)
    %w[plain key pass].each do |kind|
      credential = [kind,kind=="key" ? KEY : kind=="pass" ? PASS : ""]
      name="generated-#{kind}";directory=File.join(output,"images",name,peer);FileUtils.mkdir_p(directory)
      branches={}
      [["swift-first","swift",peer],["peer-first",peer,"swift"]].each do |direction,first,second|
        path=File.join(directory,"#{direction}.jed")
        baseline=with_actor(first,commands,"create",path,credential) do |actor|
          GENERATED_SQL.each { actor.execute(_1) };mutate(actor,1);snapshot(actor)
        end
        record(records,[name,peer,direction,"create"],core:first)
        second_state=with_actor(second,commands,"open",path,credential) do |actor|
          raise "generated foreign readback differs" unless snapshot(actor)==baseline
          actor.execute("UPDATE payload SET v=repeat('reopened-',1200),n=n+1 WHERE id=1")
          mutate(actor,2);snapshot(actor)
        end
        record(records,[name,peer,direction,"foreign-write"],core:second)
        File.write(File.join(directory,"#{direction}-sql-initial.json"),JSON.pretty_generate(baseline)+"\n")
        File.write(File.join(directory,"#{direction}-sql-final.json"),JSON.pretty_generate(second_state)+"\n")
        with_actor(first,commands,"open",path,credential) { raise "generated final readback differs" unless snapshot(_1)==second_state }
        record(records,[name,peer,direction,"final-reopen"],core:first)
        # Exercise this same foreign-written file through an actual attachment.
        main=File.join(directory,"#{direction}-main.jed")
        with_actor(first,commands,"create",main,["plain",""]) do |actor|
          actor.expect("ATTACH",path.unpack1("H*"),*credential)
          attached = snapshot(actor,domain:"aux")
          File.write(File.join(directory,"#{direction}-sql-attached.json"),JSON.pretty_generate(attached)+"\n")
          # Attached views retain their raw catalog rows, but their output
          # columns are intentionally unavailable (introspection.md §5).
          expected_attached = second_state.dup
          views = second_state.fetch("jed_views").map(&:first).to_set
          expected_attached["jed_columns"] = second_state.fetch("jed_columns").reject { views.include?(_1.first) }
          unless attached == expected_attached
            key = (attached.keys | expected_attached.keys).find { attached[_1] != expected_attached[_1] }
            raise "attached foreign image differs at #{key}; expected #{expected_attached[key].inspect.byteslice(0,512)}, got #{attached[key].inspect.byteslice(0,512)}"
          end
          actor.execute("UPDATE aux.#{quote(PROBE)} SET v='attachment' WHERE id=3")
        end
        with_actor(second,commands,"open",path,credential) do |actor|
          raise "attachment mutation missing" unless actor.query("SELECT v FROM #{quote(PROBE)} WHERE id=3")==[["attachment"]]
        end
        record(records,[name,peer,direction,"attachment-write-reopen"],core:first,reader:second,main_checker:check_image(main))
        info=kind=="pass" ? checked_passphrase_image(second,commands,path,directory) : check_image(path)
        branches[direction]=[path,info]
        # Expected missing/wrong/plaintext credential errors are native opens.
        denied=kind=="plain" ? ["key",KEY] : ["plain",""]
        code=kind=="plain" ? "28P03" : "28P02"
        begin
          with_actor(second,commands,"open",path,denied){raise "credential admission unexpectedly succeeded"}
        rescue Actor::OpenFailure=>e
          raise "credential denial #{e.code} != #{code}" unless e.code==code
        end
        record(records,[name,peer,direction,"credential-admission"],code:code)
        if kind != "plain"
          wrong = kind == "key" ? ["key", "4b" * 32] : ["pass", "wrong-passphrase".unpack1("H*")]
          begin
            with_actor(second,commands,"open",path,wrong) { raise "wrong credential succeeded" }
          rescue Actor::OpenFailure => e
            raise "wrong credential returned #{e.code}" unless e.code == "28P01"
          end
          record(records,[name,peer,direction,"wrong-credential"],code:"28P01")
        end
      end
      a,ai=branches.fetch("swift-first");b,bi=branches.fetch("peer-first")
      raise "generated exact committed bytes differ" unless committed_bytes(a,ai)==committed_bytes(b,bi)
      record(records,[name,peer,"both","bytes"],sha256:Digest::SHA256.hexdigest(committed_bytes(a,ai)))
    end
  end
end

module SwiftInterchange
  module_function

  def wanted_phases(entries, peers, generated: true)
    keys = []
    entries.each do |entry|
      peers.each do |peer|
        [["swift-first", "swift", peer], ["peer-first", peer, "swift"]].each do |direction, first, second|
          names = case entry[:kind]
                  when "open-rejection" then ["reject-#{first}", "reject-#{second}"]
                  when "lazy-corruption" then ["lazy-#{first}", "lazy-#{second}"]
                  else %w[write-1 foreign-write-2 final-reopen] end
          names.each { keys << [entry[:name], peer, direction, _1] }
        end
        if entry[:mutation]
          keys << [entry[:name], peer, "both", "bytes-1"]
          keys << [entry[:name], peer, "both", "bytes-2"]
        end
      end
    end
    if generated
      peers.each do |peer|
        %w[plain key pass].each do |kind|
          %w[swift-first peer-first].each do |direction|
            names = %w[create foreign-write final-reopen attachment-write-reopen credential-admission]
            names += ["wrong-credential"] unless kind == "plain"
            names.each { keys << ["generated-#{kind}", peer, direction, _1] }
          end
          keys << ["generated-#{kind}", peer, "both", "bytes"]
        end
      end
    end
    raise "duplicate planned phase" unless keys.uniq.length == keys.length
    keys.to_set
  end

  def coverage(wanted, records)
    missing = (wanted - records.keys.to_set).sort.map { "missing phase #{_1.join('/')}" }
    extra = (records.keys.to_set - wanted).sort.map { "unexpected phase #{_1.join('/')}" }
    missing + extra
  end

  def check_adapter
    actor = Actor.allocate
    actor.instance_variable_set(:@core, "protocol-check")
    raise "OK parser changed" unless actor.parse("OK\t") == [:ok, ""]
    raise "ERR parser changed" unless actor.parse("ERR\tXX001\t78") == [:error, "XX001", "x"]
    ["OK", "OK\tx\ty", "ERR\tXX01\t", "ERR\tXX001\tx", "READY"].each do |bad|
      begin
        actor.parse(bad)
      rescue RuntimeError
        next
      end
      raise "malformed protocol accepted: #{bad.inspect}"
    end
    actor.define_singleton_method(:expect) { |*_| @payload }
    {"0;" => [], "1;" => [[""]], "2;," => [[""],[""]], "1;-:00:cebb" => [[nil,"\0","λ"]]}.each do |payload, expected|
      actor.instance_variable_set(:@payload, payload)
      raise "text/NULL framing changed" unless actor.query("unused") == expected
    end
    ["0;00", "01;00", "2;00", "1;f", "1;ff"].each do |payload|
      actor.instance_variable_set(:@payload, payload)
      begin
        actor.query("unused")
      rescue RuntimeError
        next
      end
      raise "malformed query frame accepted: #{payload}"
    end
    entries = inventory
    wanted = wanted_phases(entries, PEERS)
    raise "coverage inventory changed" unless wanted.length == 2691
    key = wanted.first
    records = {}; record(records, key, checked: true)
    begin
      record(records, key)
      raise "duplicate phase accepted"
    rescue RuntimeError => e
      raise unless e.message.start_with?("duplicate interchange phase")
    end
    raise "missing coverage not reported" unless coverage(wanted, records).length == wanted.length - 1
    record(records, ["outside", "go", "swift-first", "extra"])
    raise "extra coverage not reported" unless coverage(wanted, records).any? { _1.start_with?("unexpected") }
    checker_paths
    puts "interchange adapter: 112 pinned applicability entries, #{wanted.length} required phases; framing/coverage guards PASS"
    0
  end

  def run
    return check_adapter if ENV["usage_check_adapter"] == "true"
    configuration = ENV.fetch("usage_configuration", "debug")
    raise "configuration must be debug or release" unless %w[debug release].include?(configuration)
    peers = ENV.fetch("usage_peers", PEERS.join(",")).split(",", -1)
    raise "peers must be unique rust,go,ts names" unless !peers.empty? && peers.uniq == peers && (peers - PEERS).empty?
    all_entries = inventory
    filter = ENV["usage_filter"]
    entries = all_entries.select { !filter || _1[:name].include?(filter) }
    raise "filter selected no fixtures" if entries.empty?
    generated = ENV["usage_fixtures_only"] != "true"
    wanted = wanted_phases(entries, peers, generated: generated)
    output = ENV["usage_output"] ? File.expand_path(ENV["usage_output"]) : Dir.mktmpdir("jed-swift-interchange-")
    raise "output must be absent or empty" if File.exist?(output) && !Dir.empty?(output)
    FileUtils.mkdir_p(output)
    File.write(File.join(output, "inventory.json"), JSON.pretty_generate({baseline: BASELINE, selected: entries, all_fixture_count: all_entries.length}) + "\n")
    File.write(File.join(output, "expected-phases.json"), JSON.pretty_generate(wanted.to_a.sort) + "\n")
    checker_paths
    puts "interchange evidence: #{output}"; $stdout.flush
    if ENV["usage_inventory"] == "true"
      puts "inventory only: #{entries.length} fixtures, #{peers.length} peers, #{wanted.length} required phases; no acceptance run"
      return 0
    end
    identity = source_identity
    actor_commands = commands(output, peers, configuration, ENV["usage_no_build"] == "true", ENV["usage_skip_swift_build"] == "true")
    records = {}; failures = []; completed = []
    entries.each do |entry|
      peers.each do |peer|
        label = "#{entry[:name]}/#{peer}"
        begin
          run_fixture(entry, peer, actor_commands, output, records)
          completed << label
          puts "PASS #{label}"
        rescue StandardError => error
          failures << "#{label}: #{error.class}: #{error.message}"
          warn "FAIL #{failures.last}"
        ensure
          $stdout.flush
          File.write(File.join(output, "progress.json"), JSON.pretty_generate({completed: completed, records: records.values, failures: failures}) + "\n")
        end
      end
    end
    if generated
      peers.each do |peer|
        begin
          run_generated(peer, actor_commands, output, records)
          completed << "generated/#{peer}"
          puts "PASS generated/#{peer}"
        rescue StandardError => error
          failures << "generated/#{peer}: #{error.class}: #{error.message}"
          warn "FAIL #{failures.last}"
        end
      end
    end
    failures << "TypeScript engine source changed during run" unless source_identity == identity
    begin
      inventory
    rescue StandardError => error
      failures << "frozen inputs changed during run: #{error.message}"
    end
    # Executables and script transport are copied before the first open. Verify
    # their exact bytes again so a concurrent build cannot alter the actors.
    JSON.parse(File.read(File.join(output, "actors.json"))).each do |core, facts|
      failures << "captured actor changed: #{core}" unless Digest::SHA256.file(facts.fetch("path")).hexdigest == facts.fetch("sha256")
    end
    failures.concat(coverage(wanted, records))
    report = {baseline: BASELINE, fixtures: entries.length, peers: peers, generated: generated,
      complete_matrix: entries.length == 112 && peers.to_set == PEERS.to_set && generated && !filter,
      expected_phases: wanted.length, completed_phases: records.length, completed_cases: completed,
      records: records.values, failures: failures, source_identity: identity}
    report[:accepted] = report[:complete_matrix] && failures.empty?
    File.write(File.join(output, "report.json"), JSON.pretty_generate(report) + "\n")
    warn failures.first(80).join("\n") unless failures.empty?
    puts "interchange: #{records.length}/#{wanted.length} phases, #{failures.length} failures; #{report[:complete_matrix] ? 'full' : 'diagnostic subset'} matrix"
    failures.empty? ? 0 : 1
  rescue StandardError => error
    warn "interchange setup failed: #{error.class}: #{error.message}"
    1
  end
end
