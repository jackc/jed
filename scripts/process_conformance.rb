#!/usr/bin/env ruby
# Layer-4 real-process shared-file driver (spec/design/concurrency-testing.md §10).

require "json"
require "open3"
require "tmpdir"
require "timeout"
require_relative "lib/bundle_setup" # works under mise and when invoked directly
require "toml-rb"

ROOT = File.expand_path("..", __dir__)
CORPUS = File.join(ROOT, "spec/conformance/process")
# JED_PROCESS_CORES=rust,go narrows the pairings while a core is being brought up; CI runs all three.
CORES = (ENV["JED_PROCESS_CORES"]&.split(",") || %w[rust go node]).freeze
START_TIMEOUT = 15
COMMAND_TIMEOUT = 10

class Actor
  # The per-command reply deadline; the page-service benchmark (mise-tasks/bench/page_service) raises it.
  class << self
    attr_writer :command_timeout

    def command_timeout = @command_timeout || COMMAND_TIMEOUT
  end

  def initialize(core, action, path)
    command = case core
              when "rust" then [File.join(ROOT, "impl/rust/target/release/process_actor")]
              when "go" then [File.join(ROOT, "impl/go/process_actor")]
              when "node" then ["node", File.join(ROOT, "impl/ts/src/bin/process_actor.ts")]
              else raise "unknown actor core #{core}"
              end
    # `action` may carry extra actor arguments after the action word ("create 5000 300": the
    # open timeout, then the owner's idle-in-transaction timeout), passed after the path.
    action, *extra = action.split
    @stdin, @stdout, @stderr, @wait = Open3.popen3(*command, action, path, *extra)
    line = read_line(START_TIMEOUT)
    return if line == "READY\n"
    # A failed open answers ERR <sqlstate> and exits; a `start` step with expect_error checks the code.
    status, code = line.to_s.chomp.split("\t", 3)
    raise StartFailed, code if status == "ERR"
    raise "#{core} actor failed to start: #{line || @stderr.read}"
  end

  class StartFailed < StandardError; end

  def command(name, argument = "")
    send_command(name, argument)
    reply
  end

  # An `async` step sends its command and leaves the reply for a later `await` step.
  def send_command(name, argument = "")
    @stdin.puts(argument.empty? ? name : "#{name}\t#{argument}")
    @stdin.flush
  end

  def reply
    parse(read_line(Actor.command_timeout))
  end

  # An `async` close: the actor acknowledges CLOSE at once, then closes its database (an owner drains its
  # clients first, spec/pageservice/README.md §6.5); `await` waits for it to exit.
  def start_close
    command("CLOSE")
  end

  def finish_close
    @wait.join(Actor.command_timeout) or raise "actor did not exit after CLOSE"
    status = @wait.value
    raise "actor exited with #{status.exitstatus} after CLOSE; stderr=#{@stderr.read}" unless status.success?
    close_pipes
  end

  def kill
    Process.kill("KILL", @wait.pid) if @wait.alive?
    @wait.join
    close_pipes
  end

  def close
    return unless @wait.alive?
    command("CLOSE")
    finish_close
  end

  def cleanup
    kill if @wait&.alive?
  rescue Errno::ESRCH, IOError
    # Already gone.
  end

  private

  def read_line(seconds)
    Timeout.timeout(seconds) { @stdout.gets }
  rescue Timeout::Error
    raise "actor response timed out; stderr=#{@stderr.read_nonblock(4096, exception: false).inspect}"
  end

  def parse(line)
    raise "actor exited; stderr=#{@stderr.read}" if line.nil?
    status, value, detail = line.chomp.split("\t", 3)
    return [:ok, value.to_s] if status == "OK"
    return [:error, value.to_s, [detail.to_s].pack("H*")] if status == "ERR"
    raise "malformed actor response: #{line.inspect}"
  end

  def close_pipes
    [@stdin, @stdout, @stderr].each { |io| io.close unless io.closed? }
  end
end

# Loaded as a library (the page-service benchmark reuses Actor), the corpus run below is skipped.
return if ENV["JED_PROCESS_DRIVER_LIBRARY"]

# A step's SQL may write `{{repeat <char> <count>}}` for a run of one character, so a corpus file can
# hold a statement too long to spell out (cow_serialization_error.process.toml).
def expand_sql(sql)
  sql.gsub(/\{\{repeat (\S) (\d+)\}\}/) { Regexp.last_match(1) * Regexp.last_match(2).to_i }
end

def actor_command(step, dir)
  command = step.fetch("command").upcase
  argument = if step.fetch("command") == "attach"
               [step.fetch("name"), step.fetch("read_only", false) ? "1" : "0",
                File.join(dir, "#{step.fetch('path')}.jed")].join("\t")
             elsif step.fetch("command") == "backup"
               # BACKUP <form>\t<dest>: a backup of main to <path>.jed (spec/design/page-service.md §9).
               [step.fetch("form"), File.join(dir, "#{step.fetch('path')}.jed")].join("\t")
             elsif step.key?("sql")
               expand_sql(step.fetch("sql")).unpack1("H*")
             else
               step.fetch("argument", "").to_s
             end
  [command, argument]
end

def step_satisfied?(step, result)
  if (expected = step["expect_error"])
    result[0] == :error && result[1] == expected
  elsif (bound = step["expect_max"])
    result[0] == :ok && Integer(result[1]) <= bound
  else
    result[0] == :ok && result[1] == step.fetch("expect", "")
  end
end

def run_scenario(path, first_core, second_core)
  data = TomlRB.load_file(path)
  raise "#{path}: schema_version must be 1" unless data["schema_version"] == 1
  name = data.fetch("name")
  Dir.mktmpdir("jed-process-") do |dir|
    if fixture = data["fixture"]
      fixture_path = File.expand_path(fixture, File.join(ROOT, "spec"))
      fixture_root = File.join(ROOT, "spec/fileformat/fixtures/")
      raise "fixture must name a file-format golden" unless fixture_path.start_with?(fixture_root)
      File.binwrite(File.join(dir, "shared.jed"), File.binread(fixture_path))
    end
    actors = {}
    closing = {}
    begin
      data.fetch("step").each_with_index do |step, index|
        begin
          actor_name = step.fetch("actor")
          command = step.fetch("command")
          if command == "start"
            core = actor_name == "a" ? first_core : second_core
            actor_path = File.join(dir, "#{step.fetch('path', 'shared')}.jed")
            begin
              actors[actor_name] = Actor.new(core, step.fetch("argument"), actor_path)
            rescue Actor::StartFailed => failed
              raise "start failed with #{failed.message}" unless step["expect_error"] == failed.message
              next
            end
            raise "expected start to fail with #{step['expect_error']}" if step["expect_error"]
            next
          end
          if command == "sleep"
            # A pause between steps, for a timeout to expire (the idle-in-transaction timeout).
            sleep Integer(step.fetch("argument")) / 1000.0
            next
          end
          if command == "check_image"
            # The independent Ruby decoder (spec/fileformat/verify.rb --check-image) on a file the
            # scenario wrote (a backup): it must decode, and `expect` names its "txid:live:pages:free".
            check_image(File.join(dir, "#{step.fetch('path')}.jed"), step["expect"])
            next
          end
          if command == "same_bytes"
            # Two files' committed images must be byte-identical (a compacted backup and the file host
            # compaction leaves for the same snapshot).
            first, second = step.fetch("paths").map { |p| committed_image(File.join(dir, "#{p}.jed")) }
            raise "#{step.fetch('paths').join(' and ')} differ" unless first == second
            next
          end
          actor = actors.fetch(actor_name)
          if command == "await"
            # The reply of this actor's earlier `async` step: an exit after an async close, or a command's
            # reply checked against this step's expectation.
            if closing.delete(actor_name)
              actor.finish_close
              actors.delete(actor_name)
              next
            end
            result = actor.reply
            if expected = step["expect_error"]
              raise "expected #{expected}, got #{result.inspect}" unless result[0] == :error && result[1] == expected
            else
              raise "command failed: #{result.inspect}" unless result[0] == :ok
              raise "expected #{step.fetch('expect', '').inspect}, got #{result[1].inspect}" unless result[1] == step.fetch("expect", "")
            end
            next
          end
          if command == "kill"
            actor.kill
            actors.delete(actor_name)
            next
          end
          if command == "close"
            if step["async"]
              actor.start_close
              closing[actor_name] = true
            else
              actor.close
              actors.delete(actor_name)
            end
            next
          end
          wire, argument = actor_command(step, dir)
          if step["async"]
            actor.send_command(wire, argument)
            next
          end
          # `repeat`: run the command this many times; every run must satisfy the step.
          (step.fetch("repeat", 1) - 1).times do
            result = actor.command(wire, argument)
            raise "repeated run failed: #{result.inspect}" unless step_satisfied?(step, result)
          end
          result = actor.command(wire, argument)
          # `retry_ms`: an outcome that holds only eventually, such as an owner observing a page
          # client's asynchronous end_read (spec/pageservice/README.md §6.3), is retried until the
          # step's expectation holds or the bound passes.
          if (retry_ms = step["retry_ms"])
            deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + retry_ms / 1000.0
            until step_satisfied?(step, result) || Process.clock_gettime(Process::CLOCK_MONOTONIC) > deadline
              sleep 0.02
              result = actor.command(wire, argument)
            end
          end
          if expected = step["expect_error"]
            unless result[0] == :error && result[1] == expected
              raise "expected #{expected}, got #{result.inspect}"
            end
          elsif (bound = step["expect_max"])
            raise "expected at most #{bound}, got #{result.inspect}" unless step_satisfied?(step, result)
          else
            raise "command failed: #{result.inspect}" unless result[0] == :ok
            expected = step.fetch("expect", "")
            raise "expected #{expected.inspect}, got #{result[1].inspect}" unless result[1] == expected
          end
        rescue => error
          raise "step #{index + 1} (#{step.fetch('actor')}:#{step.fetch('command')}): #{error.message}"
        end
      end
    ensure
      actors.each_value(&:cleanup)
    end
    image = nil
    if data["byte_identical"]
      image = committed_image(File.join(dir, "shared.jed"))
      page_count = image.bytesize / image.byteslice(8, 4).unpack1("N")
      if data["max_page_count"] && page_count > data["max_page_count"]
        raise "page reuse did not bound growth: #{page_count} > #{data['max_page_count']}"
      end
    end
    # byte_identical_files: further files (backups) whose bytes must agree across core pairings.
    files = data.fetch("byte_identical_files", []).to_h { |p| [p, File.binread(File.join(dir, "#{p}.jed"))] }
    [name, image ? { "shared" => image }.merge(files) : (files.empty? ? nil : files)]
  end
end

# A database file's committed image: successful final commits have complete meta candidates, so trim
# unreferenced preallocation past the larger slot's page count, which is not part of the logical file
# byte contract.
def committed_image(path)
  image = File.binread(path)
  ps = image.byteslice(8, 4).unpack1("N")
  page_count = [0, ps].map { |offset| image.byteslice(offset + 24, 4).unpack1("N") }.max
  image.byteslice(0, ps * page_count)
end

def check_image(path, expected)
  stdout, stderr, status = Open3.capture3("ruby", File.join(ROOT, "spec/fileformat/verify.rb"), "--check-image", path)
  result = JSON.parse(stdout.lines.last || "{}")
  raise "verify.rb --check-image #{File.basename(path)}: #{result['error'] || stderr}" unless status.success? && result["error"].nil?
  got = result.values_at("version", "live", "pages", "free").join(":")
  raise "check_image #{File.basename(path)}: expected #{expected}, got #{got}" if expected && got != expected
end

# Every (scenario, pairing) runs in its own temp directory over its own files, so pairings share no
# file or lock and can run concurrently. A pairing spends most of its time waiting on the
# coordinators' real 1 s lease probe, so a serial run is mostly sleep. Results print in corpus order
# whatever the completion order; JED_PROCESS_JOBS=1 runs serially.
JOBS = [Integer(ENV.fetch("JED_PROCESS_JOBS", "8")), 1].max

# JED_PROCESS_FILTER=service runs only the scenarios whose file name contains it.
filter = ENV["JED_PROCESS_FILTER"]
jobs = Dir[File.join(CORPUS, "*.process.toml")].sort.select { |path| filter.nil? || File.basename(path).include?(filter) }.flat_map do |path|
  CORES.product(CORES).map { |first, second| { path: path, first: first, second: second } }
end
queue = Queue.new
jobs.each { |job| queue << job }
queue.close
Array.new(JOBS) do
  Thread.new do
    while (job = queue.pop)
      begin
        job[:name], job[:image] = run_scenario(job[:path], job[:first], job[:second])
      rescue => error
        job[:error] = error
      end
    end
  end
end.each(&:join)

failures = []
references = {}
jobs.each do |job|
  pairing = "#{job[:first]}->#{job[:second]}"
  error = job[:error]&.message
  if error.nil? && job[:image]
    reference = (references[job[:path]] ||= job[:image])
    differing = job[:image].keys.reject { |file| job[:image][file] == reference[file] }
    error = "final bytes of #{differing.join(', ')} differ across core pairings" unless differing.empty?
  end
  if error
    warn "FAIL #{File.basename(job[:path])} #{pairing}: #{error}"
    failures << job
  else
    puts "PASS #{job[:name]} #{pairing}"
  end
end

abort "process conformance failed (#{failures.length} pairing(s))" unless failures.empty?
puts "\nprocess conformance OK (#{CORES.join(", ")} pairings)"
