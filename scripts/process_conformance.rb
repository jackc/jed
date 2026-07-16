#!/usr/bin/env ruby
# Layer-4 real-process shared-file driver (spec/design/concurrency-testing.md §10).

require "open3"
require "tmpdir"
require "timeout"
require "toml-rb"

ROOT = File.expand_path("..", __dir__)
CORPUS = File.join(ROOT, "spec/conformance/process")
CORES = %w[rust go node].freeze
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

def actor_command(step)
  command = step.fetch("command").upcase
  argument = if step.key?("sql")
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
    database = File.join(dir, "shared.jed")
    actors = {}
    begin
      data.fetch("step").each_with_index do |step, index|
        begin
          actor_name = step.fetch("actor")
          command = step.fetch("command")
          if command == "start"
            core = actor_name == "a" ? first_core : second_core
            actors[actor_name] = Actor.new(core, step.fetch("argument"), database)
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
          wire, argument = actor_command(step)
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
    puts "PASS #{name} #{first_core}->#{second_core}"
  end
end

failures = []
Dir[File.join(CORPUS, "*.process.toml")].sort.each do |path|
  CORES.product(CORES).each do |first, second|
    begin
      run_scenario(path, first, second)
    rescue => error
      warn "FAIL #{File.basename(path)} #{first}->#{second}: #{error.message}"
      failures << [path, first, second]
    end
  end
end

abort "process conformance failed (#{failures.length} pairing(s))" unless failures.empty?
puts "\nprocess conformance OK (Rust, Go, and Node pairings)"
