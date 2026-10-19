require 'fileutils'
repo='/home/jack/.codex/worktrees/2cdf/jed'
root='/tmp/jed-latency-cancellation'
FileUtils.mkdir_p(root)
sources={'baseline'=>'/tmp/jed-fused-baseline','current'=>'/tmp/jed-fused-short','eighth'=>'/tmp/jed-latency-eighth'}
order=ARGV.empty? ? %w[browser-baseline-1 browser-current-1 browser-eighth-1 browser-eighth-2 browser-baseline-2 browser-current-2 browser-current-3 browser-eighth-3 browser-baseline-3] : ARGV
order.each do |label|
 host,variant,_=label.split('-')
 env={'JED_ASYNC_CANCEL_HOST'=>host,'JED_ASYNC_CANCEL_RESULTS'=>"#{root}/#{label}",'JED_PROBE_ROOT'=>sources.fetch(variant),'JED_ASYNC_HEAVY_CASES'=>'decimal-aggregate,typed-output'}
 puts "Starting #{label}"; $stdout.flush
 abort "Failed #{label}" unless system(env,'taskset','-c','0,2','mise','run','bench:ts_async_cancel',chdir:repo,out:"#{root}/#{label}.log",err:[:child,:out])
 puts "Finished #{label}"; $stdout.flush
end
