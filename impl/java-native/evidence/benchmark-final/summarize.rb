require 'json'
e = JSON.parse(File.read(ARGV.fetch(0)))
def med(a); x=a.sort; n=x.size; n.odd? ? x[n/2] : (x[n/2-1]+x[n/2])/2.0; end
e['summary'] = e.fetch('results').flat_map{|run|run['records'].map{|r|s=r['samples_ns']; n=[s.size/3,1].max; w=r['warmup_ns']; {"actor"=>run['actor'],"repeat"=>run['repeat'],"name"=>r['name'],"median_ns"=>med(s),"late_over_early"=>med(s.last(n)).to_f/med(s.first(n)),"warmup_tail_median_ns"=>med(w.last([w.size/3,1].max))}}}
actors=e.fetch('results').map{_1.fetch('actor')}.uniq
cases=e.fetch('results').first.fetch('records').map{_1.fetch('name')}
rows=cases.map do |name|
 metrics=actors.to_h do |actor|
  runs=e['results'].select{_1['actor']==actor}.map{|r|r['records'].find{_1['name']==name}}
  summaries=e.fetch('summary').select{_1['actor']==actor && _1['name']==name}
  [actor, {median_ns:med(summaries.map{_1['median_ns']}),repeat_medians_ns:summaries.map{_1['median_ns']},late_over_early:summaries.map{_1['late_over_early']},warmup_tail_ns:summaries.map{_1['warmup_tail_median_ns']},compilation_ms:runs.map{_1['compilation_millis']},alloc_bytes_per_execution:runs.map{|r|r['allocated_bytes'].is_a?(Numeric) ? r['allocated_bytes'].to_f/r['iterations'] : r['thread_allocated_bytes'].is_a?(Numeric) ? (r['thread_allocated_bytes']+r.fetch('worker_allocated_bytes',0)).to_f/r['iterations'] : nil},gc_collections:runs.map{_1['gc_collections']},gc_ms:runs.map{|r|r['gc_millis'] || (r['gc_pause_ns'] && r['gc_pause_ns']/1e6)}}]
 end
 {name:name,actors:metrics,stable_over_go:metrics.fetch('stable')[:median_ns].to_f/metrics.fetch('go')[:median_ns],stable_over_rust:metrics.fetch('stable')[:median_ns].to_f/metrics.fetch('rust')[:median_ns],ea_over_stable:metrics.fetch('ea')[:median_ns].to_f/metrics.fetch('stable')[:median_ns],valhalla_over_ea:metrics.fetch('valhalla')[:median_ns].to_f/metrics.fetch('ea')[:median_ns]}
end
out={source:e['java_source_sha256'],cases:rows,process:actors.to_h{|a|runs=e['results'].select{_1['actor']==a};[a,{startup_median_ns:med(runs.flat_map{_1['startup_ns']}),startup_samples_ns:runs.flat_map{_1['startup_ns']},resources:runs.map{_1['process_resources']}}]},retained:e['retained_memory'] && e['retained_memory']['results'].to_h{|r|[r['actor'],r['records'].map{|x|x.slice('name','retained_before','retained_live_database','retained_after_close')}]}}
out[:median_method] = 'Conventional sample median (mean of the middle pair for even counts), followed by median across three process medians; raw driver summary uses upper medians and is retained unchanged.'
out[:measurement_interpretation] = 'Shared-host bounded-warmup diagnostic; no idle-host or converged steady-state claim. See context-summary.json and BENCHMARKS.md.'
out[:correctness] = {timed_actor_runs:e.fetch('results').size, cases_per_run:cases.size, mismatches:e.fetch('mismatches'), retained_records:e.fetch('retained_memory').fetch('results').sum{|r|r.fetch('records').size}}
if ARGV[1]
 baseline=JSON.parse(File.read(ARGV[1]))
 out[:before_aggregate_optimization] = {source:baseline.fetch('source'), cases:rows.map do |r|
  old=baseline.fetch('cases').find{|x|x.fetch('name')==r[:name]}.fetch('actors').fetch('stable')
  now=r[:actors].fetch('stable')
  before_alloc=med(old.fetch('alloc_bytes_per_execution'))
  after_alloc=med(now.fetch(:alloc_bytes_per_execution))
  {name:r[:name],stable_before_ns:old.fetch('median_ns'),stable_after_ns:now.fetch(:median_ns),time_after_over_before:now.fetch(:median_ns).to_f/old.fetch('median_ns'),stable_before_alloc_bytes:before_alloc,stable_after_alloc_bytes:after_alloc,allocation_after_over_before:after_alloc/before_alloc}
 end}
end
puts JSON.pretty_generate(out)
