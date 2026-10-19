require 'json'
require 'digest'
require 'fileutils'
def median(v)
  s=v.sort
  (s[(s.size-1)/2]+s[s.size/2])/2.0
end
root='/tmp/jed-fused-cancellation'
out='/home/jack/.codex/worktrees/2cdf/jed/bench/ts/results/2026-10-19-scheduling'
FileUtils.mkdir_p(out+'/cancellation')
groups={}
runs=[]
Dir.glob(root+'/*/results.json').sort.each do |p|
  label=File.basename(File.dirname(p)); host,variant,_=label.split('-');x=JSON.parse(File.read(p))
  FileUtils.cp(p,out+'/cancellation/'+label+'.json')
  runs << {label:label,date:x['recordedAt'],sha256:Digest::SHA256.file(p).hexdigest}
  x['results'].each do |c|
    g=(groups[[host,variant,c['id']]] ||= {host:host,variant:variant,case:c['id'],processMedians:[],samples:[],markerDelays:[],submitDelays:[],workerDelays:[],states:Hash.new(0)})
    g[:processMedians] << c['issueToSettleMs']['median'] if c['issueToSettleMs']
    c['samples'].each do |s|
      abort 'failed reuse/watchdog' unless s['cleanupVerified'] && !s['watchdogFired']
      g[:states][s['state']]+=1
      g[:samples] << s['issueToSettleMs'] if s['state']=='canceled'
      g[:markerDelays] << s['markerToIssueMs'] if s['markerToIssueMs']
      g[:submitDelays] << s['submitToMarkerMs'] if s['submitToMarkerMs']
      g[:workerDelays] << s['receivedToErrorSentMs'] if s['receivedToErrorSentMs']
    end
  end
end
groups=groups.values.map do |g|
  g.merge(median:median(g[:processMedians]),pooledMedian:median(g[:samples]),worst:g[:samples].max,markerDelayMedian:median(g[:markerDelays]),markerDelayWorst:g[:markerDelays].max,submitDelayMedian:median(g[:submitDelays]),submitDelayWorst:g[:submitDelays].max,workerDelayMedian:g[:workerDelays].empty? ? nil : median(g[:workerDelays]),workerDelayWorst:g[:workerDelays].max)
end
File.write(out+'/cancellation-summary.json',JSON.pretty_generate({runs:runs.sort_by{|r|r[:date]},groups:groups})+"\n")
groups.each{|g|puts '%-7s %-9s %-20s n%-2d median/worst %6.2f/%6.2f ms timer %5.2f/%5.2f worker %6.2f/%6.2f' % [g[:host],g[:variant],g[:case],g[:samples].length,g[:median],g[:worst],g[:markerDelayMedian],g[:markerDelayWorst],g[:workerDelayMedian]||0,g[:workerDelayWorst]||0]}
