# frozen_string_literal: true
require_relative "tasks"
require "time"

# Public-API benchmark transport. Engine semantics come from immutable actor sources.
module JavaPerformance
  module_function
  def capture!(*command, env: {}, directory: JavaNativeTasks::ROOT)
    out, err, status = Open3.capture3(env, *command, chdir: directory)
    abort "#{command.first} failed: #{out}\n#{err}" unless status.success?
    out.strip
  end
  def hash(path) = Digest::SHA256.file(path).hexdigest
  def peers(destination)
    revision = JavaNativeTasks::BASELINE.fetch("semantic_revision")
    source = File.join(destination, "peer-source")
    FileUtils.mkdir_p(source)
    statuses = Open3.pipeline(["git", "archive", revision, "impl/go", "impl/rust", "spec", { chdir: JavaNativeTasks::ROOT }], ["tar", "-x", "-C", source])
    abort "Cannot export frozen benchmark peers" unless statuses.all?(&:success?)
    go = File.join(destination, "performance-go")
    rust = File.join(destination, "performance-rust")
    FileUtils.cp(File.join(__dir__, "performance.rs"), File.join(source, "impl/rust/src/bin/java_performance.rs"))
    FileUtils.cp(File.join(__dir__, "performance.go"), File.join(destination, "performance.go"))
    go_cache = File.join(JavaNativeTasks::DEV, "go-cache"); FileUtils.mkdir_p(go_cache)
    capture!("go", "build", "-trimpath", "-o", go, File.join(destination, "performance.go"), env: {"GOPROXY"=>"off","GOSUMDB"=>"off","GOTOOLCHAIN"=>"local","GOCACHE"=>go_cache}, directory: File.join(source,"impl/go"))
    rustc = capture!("rustc", "--version")
    target = File.join(JavaNativeTasks::DEV,"benchmark-rust-target")
    capture!("cargo","build","--offline","--locked","--release","--bin","java_performance", env: {"CARGO_TARGET_DIR"=>target,"JED_BENCH_RUSTC"=>rustc}, directory: File.join(source,"impl/rust"))
    FileUtils.cp(File.join(target,"release/java_performance"),rust)
    {
      "go"=>{command:[go], semantic_revision:revision, toolchain:capture!("go","version"), executable_sha256:hash(go), driver_sha256:hash(File.join(destination,"performance.go")), module_sha256:hash(File.join(source,"impl/go/go.sum"))},
      "rust"=>{command:[rust], semantic_revision:revision, toolchain:rustc, executable_sha256:hash(rust), driver_sha256:hash(File.join(source,"impl/rust/src/bin/java_performance.rs")), cargo_lock_sha256:hash(File.join(source,"impl/rust/Cargo.lock")), flags:"--offline --locked --release; default features; no benchmark dependencies"}
    }
  end
  def workloads(smoke)
    n = smoke ? 64 : 4096; point = n/4; upper = [point+100,n].min
    setup = ["CREATE TABLE t (id i32 PRIMARY KEY, v i64, k text, payload text)",
      "INSERT INTO t SELECT i, i::i64*2, 'g'||(i%17)::text, repeat(i::text||'x',32) FROM generate_series(1,#{n}) AS g(i)",
      "CREATE INDEX t_v ON t(v)"]
    cases = [
      ["scalar","query","SELECT 12345*6789",100,1000],
      ["parallel_point","parallel","SELECT id,v FROM t WHERE id=#{point}",5,25],
      ["point","query","SELECT id,v FROM t WHERE id=#{point}",100,1000],
      ["range","query","SELECT id,v FROM t WHERE id BETWEEN #{point} AND #{upper} ORDER BY id",20,100],
      ["secondary","query","SELECT id,v FROM t WHERE v BETWEEN #{point*2} AND #{upper*2} ORDER BY v",20,100],
      ["aggregate","query","SELECT count(*),sum(v) FROM t",10,50],
      ["join","query","SELECT count(*),sum(a.v+b.v) FROM t a JOIN t b ON a.id=b.id",5,25],
      ["sort","query","SELECT id,v FROM t ORDER BY v DESC,id LIMIT 100",10,50],
      ["grouped","query","SELECT k,count(*),sum(v) FROM t GROUP BY k ORDER BY k",10,50],
      ["distinct","query","SELECT DISTINCT v%64 AS k FROM t ORDER BY k",10,50],
      ["distinct_aggregate","query","SELECT count(DISTINCT v%64) FROM t",10,50],
      ["window","query","SELECT id,sum(v) OVER(ORDER BY id ROWS BETWEEN 3 PRECEDING AND CURRENT ROW) FROM t ORDER BY id LIMIT 100",10,50],
      ["ranking","query","SELECT id,rank() OVER(PARTITION BY k ORDER BY v%8) FROM t ORDER BY id LIMIT 100",10,50],
      ["text_filter","query","SELECT count(*) FROM t WHERE v::text LIKE '12%'",10,50],
      ["float_aggregate","query","SELECT sum(v::f64) FROM t",10,50],
      ["decimal_aggregate","query","SELECT sum(v::decimal/10) FROM t",10,50],
      ["expression_projection","query","SELECT id,v*3+7 FROM t WHERE id BETWEEN #{point} AND #{upper} ORDER BY id",20,100],
      ["json","query","SELECT jsonb_build_object('id',id,'value',v)->>'value' FROM t WHERE id BETWEEN #{point} AND #{upper} ORDER BY id",10,50],
      ["array","query","SELECT array_length(ARRAY[id,id+1,id+2],1) FROM t WHERE id BETWEEN #{point} AND #{upper} ORDER BY id",10,50],
      ["rollback","rollback","UPDATE t SET v=v+1 WHERE id=#{point}",20,100],
      ["durable","durable","UPDATE t SET v=v+1 WHERE id=#{point}",10,50],
      ["text_rollback","rollback","UPDATE t SET v=v+1,payload=reverse(payload) WHERE id=#{point} RETURNING payload",20,100],
      ["text_durable","durable","UPDATE t SET v=v+1,payload=reverse(payload) WHERE id=#{point} RETURNING payload",10,50],
      ["cold_open","cold_open","SELECT id,v FROM t WHERE id=#{point}",5,25]
    ]
    cases.each do |item|
      if smoke
        item[3]=1; item[4]=2
      else
        item[3]=%w[scalar point].include?(item[0]) ? 5000 : item[1]=="query" ? 200 : 100
        item[4]=%w[scalar point].include?(item[0]) ? 5000 : item[1]=="query" ? 100 : 50
      end
    end
    [setup,cases]
  end
  def run
    profile = ENV.fetch("usage_profile","smoke"); abort "Unknown profile" unless %w[smoke diagnostic].include?(profile)
    repeats = Integer(ENV.fetch("usage_repeats",profile=="smoke" ? "1" : "3")); abort "repeats must be positive" unless repeats.positive?
    variants = ENV.fetch("usage_variants","stable,ea,valhalla").split(","); abort "Invalid variants" if variants.empty? || (variants-JavaNativeTasks::VARIANTS).any? || variants.uniq!=variants
    base = File.expand_path(ENV.fetch("usage_directory",File.join(JavaNativeTasks::DEV,"benchmarks"))); FileUtils.mkdir_p(base)
    destination = Dir.mktmpdir("matched-",base)
    data_base = File.expand_path(ENV.fetch("usage_data_directory",destination)); FileUtils.mkdir_p(data_base)
    data = Dir.mktmpdir("jed-benchmark-data-",data_base)
    puts "Benchmark evidence: #{destination}; database files: #{data}"; $stdout.flush
    policy_paths = [__FILE__,File.join(__dir__,"tasks.rb"),File.join(__dir__,"performance.go"),File.join(__dir__,"performance.rs"),File.join(JavaNativeTasks::ROOT,"mise-tasks/java-native/benchmark")]
    policy = policy_paths.to_h { |path| [path,hash(path)] }
    FileUtils.mkdir_p(File.join(destination,"policy"))
    policy_paths.each { |path| FileUtils.cp(path,File.join(destination,"policy",File.basename(path))) }
    actors = {}; source_root = JavaNativeTasks::ROOT; source_identity = nil
    variants.each do |variant|
      built = JavaNativeTasks.build(variant,source_root:source_root)
      actor = JavaNativeTasks.capture(built,variant); metadata = JSON.parse(File.read(File.join(actor,"build.json")))
      source_identity ||= metadata.fetch("source_sha256"); abort "Java variants have different sources" unless source_identity==metadata.fetch("source_sha256")
      source_root = File.join(actor,"sources")
      command = JavaNativeTasks.actor_command(actor,variant); command[-1]="org.jed.PerformanceDiagnostics"; command.delete("-ea")
      actors[variant]={command:command, launcher_sha256:hash(command.first), actor:actor, build:metadata, artifacts:JSON.parse(File.read(File.join(actor,"artifacts.json")))}
    end
    actors.merge!(peers(destination))
    setup,cases = workloads(profile=="smoke")
    if ENV["usage_cases"]
      selected=ENV.fetch("usage_cases").split(","); abort "Unknown workload" unless (selected-cases.map(&:first)).empty?
      cases.select! { |item| selected.include?(item.first) }
    end
    input = File.join(destination,"input.tsv")
    File.write(input,setup.map { |sql| "setup\t#{sql}\n" }.join + cases.map { |name,mode,sql,warmup,iterations| ["case",name,mode,warmup,iterations,sql].join("\t")+"\n" }.join)
    mounts, = Open3.capture2("findmnt","--json","--target",data)
    evidence={generated_at:Time.now.utc.iso8601,profile:profile,policy_sha256:policy,runtime_environment:ENV.select { |name,_| %w[JAVA_TOOL_OPTIONS JDK_JAVA_OPTIONS GOGC GOMEMLIMIT GOMAXPROCS RUSTFLAGS CARGO_BUILD_RUSTFLAGS].include?(name) },measurement_status:profile=="smoke" ? "correctness smoke; timings not performance evidence" : "diagnostic; accept timings only after confirming an otherwise idle host",semantic_revision:JavaNativeTasks::BASELINE.fetch("semantic_revision"),java_source_sha256:source_identity,input_sha256:hash(input),data_directory:data,filesystem:JSON.parse(mounts),platform:RUBY_PLATFORM,cpu:File.readlines("/proc/cpuinfo").find { _1.start_with?("model name") }&.strip,actors:actors,results:[],mismatches:[],cache_bytes:268435456,work_mem:268435456,parallel_read_workers:4,parallel_queries_per_worker:64,notes:"Synchronous on, fsync enabled, page size8192. Parallel workloads include platform-thread launch, per-thread session/preparation,64 drains per worker and deterministic worker-order checksum aggregation. Cold open includes prepare/query/drain/close with warm OS cache. Timed drains include canonical rendering/FNV checksum. No forced GC. Java allocations separate the calling thread and parallel workers; Go allocations are process-wide; Rust allocation/GC counters unavailable. Setup excluded. Repeats run serially with rotating actor order; sample arrays retained. Tmpfs commit latency is not physical-disk durability latency."}
    output=File.join(destination,"evidence.json")
    repeats.times do |repeat|
      actors.keys.rotate(repeat%actors.size).each do |name|
        command=actors.fetch(name).fetch(:command); puts "Matched #{profile} repeat#{repeat+1}: #{name}"; $stdout.flush
        startup=(profile=="smoke" ? 1 : 5).times.map do
          start=Process.clock_gettime(Process::CLOCK_MONOTONIC,:nanosecond)
          result=capture!(*command,"ready"); abort "Startup result differs" unless result=="ready"
          Process.clock_gettime(Process::CLOCK_MONOTONIC,:nanosecond)-start
        end
        directory=File.join(data,"repeat#{repeat+1}-#{name}"); FileUtils.mkdir_p(directory)
        resource_path=File.join(destination,"repeat#{repeat+1}-#{name}.resources")
        measured=File.executable?("/usr/bin/time") ? ["/usr/bin/time","-f","%U %S %M","-o",resource_path,*command] : command
        out,err,status=Open3.capture3(*measured,input,directory,chdir:JavaNativeTasks::ROOT)
        File.write(File.join(destination,"repeat#{repeat+1}-#{name}.jsonl"),out)
        File.write(File.join(destination,"repeat#{repeat+1}-#{name}.stderr"),err)
        record={actor:name,repeat:repeat+1,startup_ns:startup,exitstatus:status.exitstatus,records:out.lines.map { JSON.parse(_1) }}
        if File.file?(resource_path)
          cpu_user,cpu_system,peak_rss=File.read(resource_path).lines.last.split
          record[:process_resources]={user_seconds:Float(cpu_user),system_seconds:Float(cpu_system),peak_rss_kib:Integer(peak_rss),scope:"whole actor process including setup/warmup/all workloads"}
        end
        evidence[:results]<<record; File.write(output,JSON.pretty_generate(evidence)+"\n")
        abort "Benchmark #{name} failed; retained #{output}: #{err}" unless status.success?
        abort "Incomplete #{name} workload records" unless record[:records].map { _1.fetch("name") }==cases.map(&:first)
      end
    end
    evidence[:results].group_by { _1[:repeat] }.each_value do |runs|
      baseline=runs.find { _1[:actor]=="go" }.fetch(:records)
      runs.each do |run|
        run[:records].zip(baseline).each do |ours,peer|
          %w[name mode warmup iterations rows checksum cost].each do |key|
            evidence[:mismatches]<<"repeat#{run[:repeat]} #{run[:actor]} #{ours['name']}.#{key}: #{ours[key]} != Go #{peer[key]}" unless ours[key]==peer[key]
          end
        end
      end
    end
    actors.each do |name,actor|
      if actor[:artifacts]
        actor[:artifacts].each { |path,digest| abort "Captured actor changed" unless hash(File.join(actor[:actor],path))==digest }
      else
        abort "Peer actor changed" unless hash(actor[:command].first)==actor[:executable_sha256]
      end
    end
    abort "Benchmark policy changed during execution" unless policy.all? { |path,digest| hash(path)==digest }
    evidence[:summary]=evidence[:results].flat_map do |run|
      run[:records].map do |record|
        samples=record.fetch("samples_ns"); sorted=samples.sort
        segment=[samples.size/3,1].max
        early=samples.first(segment).sort[segment/2]; late=samples.last(segment).sort[segment/2]
        warmups=record.fetch("warmup_ns"); warm_tail=warmups.last([warmups.size/3,1].max).sort
        {early_median_ns:early,late_median_ns:late,late_over_early:early.zero? ? nil : late.to_f/early,
         warmup_tail_median_ns:warm_tail.empty? ? nil : warm_tail[warm_tail.size/2],actor:run[:actor],repeat:run[:repeat],name:record.fetch("name"),median_ns:sorted[sorted.size/2],min_ns:sorted.first,max_ns:sorted.last,p95_ns:sorted[[(sorted.size*0.95).ceil-1,0].max]}
      end
    end
    if ENV["usage_retained_memory"]=="true"
      evidence[:retained_memory]={notes:"Separate Java processes; no timings from these runs are comparison evidence. System.gc is requested before warmup, with live database after drain, and after closing database/session. Heap excludes native/JIT/metaspace. Explicit GC may be disabled by host flags.",results:[]}
      actors.select { |name,_| JavaNativeTasks::VARIANTS.include?(name) }.each do |name,actor|
        directory=File.join(data,"retained-#{name}"); FileUtils.mkdir_p(directory)
        out=capture!(*actor.fetch(:command),input,directory,"retained")
        File.write(File.join(destination,"retained-#{name}.jsonl"),out+"\n")
        records=out.lines.map { JSON.parse(_1) }
        abort "Incomplete retained #{name} workload records" unless records.map { _1.fetch("name") }==cases.map(&:first)
        baseline=evidence[:results].find { _1[:actor]==name }.fetch(:records)
        records.zip(baseline).each { |ours,ordinary| %w[name rows checksum cost].each { |key| abort "Retained probe answer mismatch" unless ours[key]==ordinary[key] } }
        evidence[:retained_memory][:results]<<{actor:name,records:records}
      end
    end
    actors.each_value do |actor|
      if actor[:artifacts]
        actor[:artifacts].each { |path,digest| abort "Captured actor changed after retained probes" unless hash(File.join(actor[:actor],path))==digest }
      else
        abort "Peer actor changed after retained probes" unless hash(actor[:command].first)==actor[:executable_sha256]
      end
    end
    abort "Benchmark policy changed after retained probes" unless policy.all? { |path,digest| hash(path)==digest }
    evidence[:completed_at]=Time.now.utc.iso8601; File.write(output,JSON.pretty_generate(evidence)+"\n")
    abort "Matched benchmark differs: #{evidence[:mismatches].join('; ')}; evidence #{output}" unless evidence[:mismatches].empty?
    puts "PASS #{cases.size} workloads x #{actors.size} actors x #{repeats} repeats, exact values/rows/costs; evidence #{output}"
  end
end
