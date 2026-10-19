# frozen_string_literal: true

require 'json'
require 'tmpdir'
require 'fileutils'
require 'etc'

module JedNativeTools
  # A sequential audit worker has a fresh heap, an OS allocation ceiling, and a
  # deadline enforced outside the engine. A watchdog survives the supervisor's
  # abrupt death and closes the entire worker process group.
  module BoundedProcess
    DEFAULT_MEMORY_BYTES = 512 * 1024 * 1024
    OUTPUT_BYTES = 8 * 1024 * 1024
    TAIL_BYTES = 64 * 1024
    PAGE_BYTES = Etc.sysconf(Etc::SC_PAGESIZE)
    Result = Struct.new(:status, :value, :stdout, :stderr, :timed_out, :interrupted,
                        :memory_exceeded, :output_exceeded, :peak_rss_bytes,
                        :memory_bytes, :address_space_bytes, :breadcrumb, keyword_init: true) do
      def success?
        status&.success? && !timed_out && !interrupted && !memory_exceeded && !output_exceeded
      end
      def reason
        return "interrupted by #{interrupted}" if interrupted
        return 'worker deadline exceeded' if timed_out
        return "worker memory limit exceeded (#{memory_bytes} bytes)" if memory_exceeded
        return 'worker output limit exceeded' if output_exceeded
        return "worker terminated by signal #{status.termsig}" if status&.signaled?
        "worker exited #{status&.exitstatus}" unless success?
      end
    end

    module_function

    def run(command = nil, timeout:, memory_bytes: DEFAULT_MEMORY_BYTES, address_space_bytes: nil, env: {}, chdir: nil, breadcrumb: nil, &block)
      raise ArgumentError, 'provide either command argv or a block' unless !!command ^ !!block
      timeout, memory_bytes = Float(timeout), Integer(memory_bytes)
      address_space_bytes = Integer(address_space_bytes || memory_bytes * 2)
      raise ArgumentError, 'worker limits must be finite and positive' unless timeout.finite? && timeout.positive? && memory_bytes.positive?
      raise ArgumentError, 'address-space limit must be positive' unless address_space_bytes.positive?
      raise 'bounded workers require fork and RLIMIT_AS' unless Process.respond_to?(:fork) && Process.const_defined?(:RLIMIT_AS)
      address_space_bytes = [address_space_bytes, Process.getrlimit(Process::RLIMIT_AS).last].min

      directory = Dir.mktmpdir('jed-ruby-worker-')
      stdout_path, stderr_path = File.join(directory, 'stdout'), File.join(directory, 'stderr')
      value_path = File.join(directory, 'value.json')
      breadcrumb ||= File.join(directory, 'active.json')
      life_reader, life_writer = IO.pipe
      start_reader, start_writer = IO.pipe
      result = Result.new(timed_out: false, memory_exceeded: false, output_exceeded: false,
                          peak_rss_bytes: 0, memory_bytes: memory_bytes, address_space_bytes: address_space_bytes)
      worker = fork do
        life_reader.close
        life_writer.close
        start_writer.close
        Process.setpgid(0, 0)
        $stdout.reopen(stdout_path, 'w')
        $stderr.reopen(stderr_path, 'w')
        $stdout.sync = $stderr.sync = true
        Process.setrlimit(Process::RLIMIT_CORE, 0, 0)
        Process.setrlimit(Process::RLIMIT_AS, address_space_bytes, address_space_bytes)
        env.each { |key, value| value.nil? ? ENV.delete(key.to_s) : ENV[key.to_s] = value.to_s }
        ENV['TMPDIR'] = directory
        ENV['JED_NATIVE_BREADCRUMB'] = breadcrumb
        Dir.chdir(chdir) if chdir
        unless start_reader.read(1) == '.'
          # The supervisor may die between forking us and creating its watchdog.
          # EOF is cancellation, never permission to run an unsupervised worker.
          start_reader.close
          FileUtils.remove_entry(directory, true)
          exit! 72
        end
        start_reader.close
        if command
          exec(*command)
        else
          begin
            File.write(value_path, JSON.generate(block.call(directory)))
            exit! 0
          rescue NoMemoryError
            $stderr.write("worker exhausted its address-space limit\n") rescue nil
            exit! 71
          rescue Exception => error
            $stderr.write("#{error.class}: #{error.message}\n#{error.backtrace&.first(15)&.join("\n")}\n") rescue nil
            exit! 70
          end
        end
      end
      begin
        Process.setpgid(worker, worker)
      rescue Errno::EACCES, Errno::ESRCH
        nil
      end
      start_reader.close
      watchdog = fork do
        Process.setpgid(0, 0)
        life_writer.close
        start_writer.close
        # Only the supervisor owns the writing end. EOF therefore also covers
        # SIGKILL, a crash, and a terminal/task host abandoning its process.
        if life_reader.read(1).nil?
          signal_group('KILL', worker)
          sleep 0.05
          FileUtils.remove_entry(directory) if File.exist?(directory)
        end
        exit! 0
      end
      life_reader.close
      previous_traps = %w[INT TERM HUP].to_h do |name|
        [name, Signal.trap(name) { result.interrupted ||= name }]
      end
      start_writer.write('.')
      start_writer.close
      deadline = monotonic + timeout
      loop do
        waited = Process.waitpid2(worker, Process::WNOHANG)
        if waited
          result.status = waited.last
          break
        end
        resident = group_rss(worker)
        result.peak_rss_bytes = [result.peak_rss_bytes, resident].max
        result.memory_exceeded = resident > memory_bytes
        result.output_exceeded = [stdout_path, stderr_path, value_path, breadcrumb].any? { |path| File.size?(path).to_i > OUTPUT_BYTES }
        result.timed_out = monotonic >= deadline
        break if result.interrupted || result.timed_out || result.memory_exceeded || result.output_exceeded
        sleep 0.05
      end
      unless result.status
        signal_group('TERM', worker)
        sleep 0.05
        signal_group('KILL', worker)
        result.status = Process.waitpid2(worker).last
      end
      result.memory_exceeded ||= result.status.exitstatus == 71
      result.output_exceeded ||= [stdout_path, stderr_path, value_path, breadcrumb].any? { |path| File.size?(path).to_i > OUTPUT_BYTES }
      result.stdout, result.stderr = tail(stdout_path), tail(stderr_path)
      result.memory_exceeded ||= !result.status.success? && result.stderr.match?(/NoMemoryError|failed to allocate memory|address-space limit|rb_darray_realloc[^\n]*failed|cannot allocate memory/i)
      result.value = read_json(value_path)
      result.breadcrumb = read_json(breadcrumb)
      result
    ensure
      signal_group('KILL', worker) if worker
      begin
        life_writer.write('.') if life_writer && !life_writer.closed?
      rescue Errno::EPIPE, IOError
        nil
      end
      [life_reader, life_writer, start_reader, start_writer].compact.each { |io| io.close unless io.closed? }
      [worker, watchdog].compact.each do |pid|
        begin
          Process.waitpid(pid)
        rescue Errno::ECHILD
          nil
        end
      end
      previous_traps&.each { |name, handler| Signal.trap(name, handler) }
      FileUtils.remove_entry(directory) if directory && File.exist?(directory)
    end

    def monotonic = Process.clock_gettime(Process::CLOCK_MONOTONIC)

    def signal_group(signal, pid)
      Process.kill(signal, -pid)
    rescue Errno::ESRCH
      nil
    end

    def tail(path)
      return '' unless File.file?(path)
      File.open(path, 'rb') do |file|
        if file.size > TAIL_BYTES
          prefix = file.read(TAIL_BYTES / 2)
          file.seek(file.size - TAIL_BYTES / 2)
          prefix + "\n... output truncated ...\n" + file.read
        else
          file.read
        end.force_encoding(Encoding::UTF_8).scrub
      end
    end

    def read_json(path)
      JSON.parse(File.read(path)) if File.file?(path) && File.size(path) <= OUTPUT_BYTES
    rescue JSON::ParserError, Errno::ENOENT
      nil
    end

    def group_rss(group)
      return 0 unless File.directory?('/proc')
      Dir.glob('/proc/[0-9]*/stat').sum do |path|
        begin
          fields = File.read(path).sub(/\A\d+ \(.*\) /, '').split
          next 0 unless fields[2].to_i == group
          File.read(File.join(File.dirname(path), 'statm')).split[1].to_i * PAGE_BYTES
        rescue Errno::ENOENT, Errno::EACCES, Errno::ESRCH
          0
        end
      end
    end
  end
end
