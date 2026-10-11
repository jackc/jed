#!/usr/bin/env ruby
# Layer-4 real-process shared-file driver (spec/design/concurrency-testing.md §10).

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
  def initialize(core, action, path)
    command = case core
              when "rust" then [File.join(ROOT, "impl/rust/target/release/process_actor")]
              when "go" then [File.join(ROOT, "impl/go/process_actor")]
              when "node" then ["node", File.join(ROOT, "impl/ts/src/bin/process_actor.ts")]
              else raise "unknown actor core #{core}"
              end
    @stdin, @stdout, @stderr, @wait = Open3.popen3(*command, action, path)
    line = read_line(START_TIMEOUT)
    raise "#{core} actor failed to start: #{line || @stderr.read}" unless line == "READY\n"
  end

  def command(name, argument = "")
    @stdin.puts(argument.empty? ? name : "#{name}\t#{argument}")
    @stdin.flush
    parse(read_line(COMMAND_TIMEOUT))
  end

  def kill
    Process.kill("KILL", @wait.pid) if @wait.alive?
    @wait.join
    close_pipes
  end

  def close
    return unless @wait.alive?
    command("CLOSE")
    @wait.join(COMMAND_TIMEOUT) or raise "actor did not exit after CLOSE"
    close_pipes
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

def actor_command(step, dir)
  command = step.fetch("command").upcase
  argument = if step.fetch("command") == "attach"
               [step.fetch("name"), step.fetch("read_only", false) ? "1" : "0",
                File.join(dir, "#{step.fetch('path')}.jed")].join("\t")
             elsif step.key?("sql")
               step.fetch("sql").unpack1("H*")
             else
               step.fetch("argument", "").to_s
             end
  [command, argument]
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
    begin
      data.fetch("step").each_with_index do |step, index|
        begin
          actor_name = step.fetch("actor")
          command = step.fetch("command")
          if command == "start"
            core = actor_name == "a" ? first_core : second_core
            actor_path = File.join(dir, "#{step.fetch('path', 'shared')}.jed")
            actors[actor_name] = Actor.new(core, step.fetch("argument"), actor_path)
            next
          end
          actor = actors.fetch(actor_name)
          if command == "kill"
            actor.kill
            actors.delete(actor_name)
            next
          end
          if command == "close"
            actor.close
            actors.delete(actor_name)
            next
          end
          wire, argument = actor_command(step, dir)
          result = actor.command(wire, argument)
          if expected = step["expect_error"]
            unless result[0] == :error && result[1] == expected
              raise "expected #{expected}, got #{result.inspect}"
            end
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
      image = File.binread(File.join(dir, "shared.jed"))
      ps = image.byteslice(8, 4).unpack1("N")
      # Successful final commits have complete meta candidates; trim unreferenced
      # preallocation, which is not part of the logical file byte contract.
      page_count = [0, ps].map { |offset| image.byteslice(offset + 24, 4).unpack1("N") }.max
      if data["max_page_count"] && page_count > data["max_page_count"]
        raise "page reuse did not bound growth: #{page_count} > #{data['max_page_count']}"
      end
      image = image.byteslice(0, ps * page_count)
    end
    [name, image]
  end
end

# Every (scenario, pairing) runs in its own temp directory over its own files, so pairings share no
# file or lock and can run concurrently. A pairing spends most of its time waiting on the
# coordinators' real 1 s lease probe, so a serial run is mostly sleep. Results print in corpus order
# whatever the completion order; JED_PROCESS_JOBS=1 runs serially.
JOBS = [Integer(ENV.fetch("JED_PROCESS_JOBS", "8")), 1].max

jobs = Dir[File.join(CORPUS, "*.process.toml")].sort.flat_map do |path|
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
    error = "final incremental bytes differ across core pairings" unless job[:image] == reference
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
