# frozen_string_literal: true

require_relative '../tools/bounded_process'
require 'rbconfig'

module RubyBoundedProcessTest
  extend self

  Worker = JedNativeTools::BoundedProcess
  def assert(value, message)
    raise message unless value
  end
  def running?(pid)
    status = File.read("/proc/#{pid}/stat")
    status.sub(/\A\d+ \(.*\) /, '').split.first != 'Z'
  rescue Errno::ENOENT, Errno::ESRCH
    false
  end

  result = Worker.run(timeout: 2) { { answer: 42 } }
  assert(result.success? && result.value == { 'answer' => 42 }, 'worker result was lost')
  result = Worker.run([RbConfig.ruby, '-e', '$stdout.puts "out"; $stderr.puts "err"; exit 3'], timeout: 2)
  assert(!result.success? && result.status.exitstatus == 3 && result.stdout == "out\n" && result.stderr == "err\n", 'command status/output was lost')

  result = Worker.run(timeout: 0.2) do |directory|
    child = fork { sleep 30 }
    File.write(File.join(directory, 'active.json'), JSON.generate(pid: Process.pid, child: child, sql: 'blocked'))
    sleep 30
  end
  assert(result.timed_out && result.breadcrumb['sql'] == 'blocked', 'deadline or breadcrumb missing')
  assert(!running?(result.breadcrumb['pid']) && !running?(result.breadcrumb['child']), 'timeout left a worker descendant running')

  result = Worker.run(timeout: 2, memory_bytes: 128 * 1024 * 1024, address_space_bytes: 128 * 1024 * 1024) do
    chunks = []
    loop { chunks << ('x' * (4 * 1024 * 1024)) }
  end
  assert(!result.success? && result.memory_exceeded, "allocation ceiling did not stop the worker: #{result.reason}")

  result = Worker.run(timeout: 2) do
    Process.kill('INT', Process.ppid)
    sleep 30
  end
  assert(result.interrupted == 'INT' && !result.success?, 'supervisor interruption did not stop the worker')

  result = Worker.run(timeout: 2) { { oversized: 'x' * (Worker::OUTPUT_BYTES + 1) } }
  assert(result.output_exceeded && result.value.nil? && !result.success?, 'oversized worker JSON was parsed into the supervisor')

  Dir.mktmpdir('jed-startup-death-test-') do |directory|
    marker = File.join(directory, 'worker.json')
    executed = File.join(directory, 'executed')
    supervisor = fork do
      ENV['TMPDIR'] = directory
      forks = 0
      Worker.singleton_class.prepend(Module.new do
        define_method(:fork) do |&block|
          forks += 1
          # Kill at the precise startup window: the worker exists, but its
          # watchdog has not yet been forked and no start token was written.
          Process.kill('KILL', Process.pid) if forks == 2
          super(&block).tap do |pid|
            workspace = Dir.glob(File.join(directory, 'jed-ruby-worker-*')).fetch(0)
            File.write(marker, JSON.generate(pid: pid, workspace: workspace))
          end
        end
      end)
      Worker.run(timeout: 30) do
        File.write(executed, 'unsupervised work ran')
        sleep 30
      end
      exit! 0
    end
    Process.waitpid(supervisor)
    info = JSON.parse(File.read(marker))
    deadline = Worker.monotonic + 2
    sleep 0.01 until (!running?(info['pid']) && !File.exist?(info['workspace'])) || Worker.monotonic >= deadline
    assert(!File.exist?(executed), 'startup EOF allowed unsupervised work to run')
    assert(!running?(info['pid']), 'startup supervisor death left a worker running')
    assert(!File.exist?(info['workspace']), 'startup cancellation retained worker temporary files')
  ensure
    Worker.signal_group('KILL', info['pid']) if info
  end

  Dir.mktmpdir('jed-watchdog-test-') do |directory|
    marker = File.join(directory, 'worker.json')
    supervisor = fork do
      Worker.run(timeout: 30) do |workspace|
        File.write(marker, JSON.generate(pid: Process.pid, workspace: workspace))
        sleep 30
      end
      exit! 0
    end
    deadline = Worker.monotonic + 2
    sleep 0.01 until File.exist?(marker) || Worker.monotonic >= deadline
    assert(File.exist?(marker), 'watchdog test worker did not start')
    info = JSON.parse(File.read(marker))
    Process.kill('KILL', supervisor)
    Process.waitpid(supervisor)
    sleep 0.01 until (!running?(info['pid']) && !File.exist?(info['workspace'])) || Worker.monotonic >= deadline
    assert(!running?(info['pid']), 'supervisor death left the worker running')
    assert(!File.exist?(info['workspace']), 'watchdog did not remove worker temporary files')
  end

  require_relative '../bin/conformance'
  corpus = NativeCorpus.new
  80.times { corpus.fail_record('diagnostic.test', 1, 'e' * 70_000, 's' * 70_000) }
  summary = corpus.summary
  assert(summary[:failed] == 80 && summary[:omitted_failure_details].positive?, 'failure retention lost counts or exceeded its cap')
  assert(summary[:failures].all? { |entry| entry[:error].bytesize <= 65_536 && entry[:sql].bytesize <= 65_536 && entry[:error_original_bytes] == 70_000 }, 'individual diagnostics were not bounded')

  puts 'ruby-native bounded workers: result/status, deadline descendants, allocation ceiling, parent-death cleanup, and bounded diagnostics passed'
end
