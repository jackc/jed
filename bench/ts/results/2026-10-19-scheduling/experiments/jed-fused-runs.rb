require 'fileutils'
repo = '/home/jack/.codex/worktrees/2cdf/jed'
root = '/tmp/jed-fused-results'
FileUtils.mkdir_p(root)
ARGV.each do |label|
  host, variant, _run = label.split('-')
  source = "/tmp/jed-fused-#{variant}"
  directory = "#{root}/#{label}"
  FileUtils.mkdir_p(directory)
  env = {'JED_ASYNC_HEAVY_WARMUPS'=>'3', 'JED_ASYNC_HEAVY_SAMPLES'=>'7', 'JED_ASYNC_HEAVY_RESPONSE_SAMPLES'=>'3', 'JED_BENCH_REVISION'=>'2a8bdfcb', 'JED_BENCH_DIRTY'=>(variant != 'baseline').to_s}
  env['JED_ASYNC_HEAVY_CASES'] = ENV['JED_FUSED_CASES'] if ENV['JED_FUSED_CASES']
  if host == 'browser'
    File.symlink("#{source}/site", "#{directory}/site") unless File.exist?("#{directory}/site")
    script = "#{source}/bench/ts/browser/run.mjs"
  else
    script = "#{source}/bench/ts/src/bench-async-heavy.ts"
  end
  File.write("#{root}/active", label)
  puts "Starting #{label}"
  $stdout.flush
  abort "Failed #{label}" unless system(env, 'taskset', '-c', '0,2', 'mise', 'exec', '--', 'node', script, directory, chdir:repo, out:"#{root}/#{label}.log", err:[:child,:out])
  puts "Finished #{label}"
  $stdout.flush
end
File.write("#{root}/active", 'complete')
