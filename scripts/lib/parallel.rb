# frozen_string_literal: true

# scripts/lib/parallel.rb — run independent commands as concurrent child processes without mixing
# their output. Used by `mise run conformance:all` (every corpus walk at once) and scripts/gate.rb
# (`mise run test` / `mise run ci`).
#
# Each job's stdout and stderr go to its own log file. Jobs are reported in DECLARATION order, each
# as soon as it and every job declared before it have finished, so the output reads the same as a
# serial run whatever the completion order. A failing job does not stop the others: the caller sees
# every job's result and decides how to fail.

module Parallel
  module_function

  # `after` names jobs this one must not overlap: it starts only once they have finished (whether
  # they passed or failed). `status` is nil until the job finishes, then true or false.
  Job = Struct.new(:name, :argv, :env, :chdir, :after, :log, :status, :seconds, :started, keyword_init: true) do
    def finished? = !status.nil?
  end

  def now = Process.clock_gettime(Process::CLOCK_MONOTONIC)

  # Run `jobs`, at most `limit` at once, writing each log under `dir`. Among the jobs ready to start,
  # a lower `rank` starts first (ties in declaration order), so the slowest jobs can go early. Yields
  # each finished job in declaration order. If interrupted, stops every job still running.
  def run(jobs, dir:, limit: jobs.size, rank: ->(_job) { 0 })
    by_name = jobs.to_h { |job| [job.name, job] }
    jobs.each_with_index do |job, i|
      job.after ||= []
      job.env ||= {}
      job.log = File.join(dir, format("%02d.log", i))
      unknown = job.after.reject { |name| by_name.key?(name) }
      raise ArgumentError, "#{job.name}: unknown job(s) in after: #{unknown.join(', ')}" unless unknown.empty?
    end
    pending = jobs.each_with_index.sort_by { |job, i| [rank.(job), i] }.map(&:first)
    live = {}
    reported = 0
    begin
      while reported < jobs.size
        while live.size < [limit, 1].max &&
              (job = pending.find { |candidate| candidate.after.all? { |name| by_name[name].finished? } })
          pending.delete(job)
          job.started = now
          begin
            live[Process.spawn(job.env, *job.argv, chdir: job.chdir, out: job.log, err: %i[child out])] = job
          rescue SystemCallError => e
            File.write(job.log, "could not start #{job.argv.first}: #{e.message}\n")
            job.status = false
            job.seconds = 0.0
          end
        end
        while reported < jobs.size && jobs[reported].finished?
          yield jobs[reported]
          reported += 1
        end
        break if reported == jobs.size
        # Every ready job was started above, so nothing running means the rest wait on each other.
        raise ArgumentError, "jobs wait on each other: #{pending.map(&:name).join(', ')}" if live.empty?

        pid, status = Process.wait2(-1)
        next unless (job = live.delete(pid))

        job.status = status.success?
        job.seconds = now - job.started
      end
    ensure
      live.each_key { |pid| Process.kill("TERM", pid) rescue nil }
    end
    jobs
  end
end
