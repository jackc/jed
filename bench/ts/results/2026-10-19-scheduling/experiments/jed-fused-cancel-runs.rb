require 'fileutils'
repo='/home/jack/.codex/worktrees/2cdf/jed'
root='/tmp/jed-fused-cancellation'
FileUtils.mkdir_p(root)
order=%w[browser-baseline-1 browser-scheduler-1 browser-short-1 node-short-1 node-scheduler-1 node-baseline-1 node-baseline-2 node-short-2 node-scheduler-2 browser-scheduler-2 browser-short-2 browser-baseline-2 browser-short-3 browser-baseline-3 browser-scheduler-3 node-scheduler-3 node-baseline-3 node-short-3]
order.each do |label|
 host,variant,_=label.split('-')
 env={'JED_ASYNC_CANCEL_HOST'=>host,'JED_ASYNC_CANCEL_RESULTS'=>"#{root}/#{label}",'JED_PROBE_ROOT'=>"/tmp/jed-fused-#{variant}",'JED_ASYNC_HEAVY_CASES'=>'decimal-aggregate,typed-output'}
 puts "Starting cancellation #{label}"; $stdout.flush
 abort "Failed #{label}" unless system(env,'taskset','-c','0,2','mise','run','bench:ts_async_cancel',chdir:repo,out:"#{root}/#{label}.log",err:[:child,:out])
 puts "Finished cancellation #{label}"; $stdout.flush
end
