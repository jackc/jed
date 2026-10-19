require 'json'
require 'digest'
def median(values)
  ordered=values.sort
  (ordered[(ordered.length-1)/2]+ordered[ordered.length/2])/2.0
end
root='/tmp/jed-fused-results'
files=Dir.glob("#{root}/*/results.json").select { |p| File.basename(File.dirname(p)).match?(/-(?:baseline|scheduler|combined|short)-[123]$/) }
groups={}
expected={}
runs=[]
files.sort.each do |path|
  label=File.basename(File.dirname(path))
  host,variant,_=label.split('-')
  raw=JSON.parse(File.read(path)); suite=raw['suite'] || raw
  runs << {label:label, sha256:Digest::SHA256.file(path).hexdigest, date:raw.dig('metadata','dateUtc') || raw['recordedAt']}
  suite['cases'].each do |c|
    expected[c['id']] ||= c['validated']
    abort "Result mismatch #{label} #{c['id']}" unless expected[c['id']] == c['validated']
    group=(groups[[host,variant,c['id']]] ||= {host:host,variant:variant,case:c['id'],direct:[],worker:[],timer:[],frames:[],longTasks:0,observations:0,validated:c['validated'],runs:[]})
    group[:runs] << label
    group[:direct] << c['latencyMs']['direct']['p50']
    group[:worker] << c['latencyMs']['worker']['p50']
    c['responsiveness']['worker'].each do |sample|
      abort "Response mismatch" unless sample['result'] == c['validated']
      group[:timer] << sample['timer']['gaps']['max']
      group[:observations] += 1
      next unless sample['extra']
      group[:frames] << sample['extra']['animationFrames']['summary']['maxMs']
      group[:longTasks] += sample['extra']['longTasks']['count']
    end
  end
end
summary=groups.values.map do |g|
  g.merge(directMedian:median(g[:direct]),workerMedian:median(g[:worker]),overheadPercent:(median(g[:worker])/median(g[:direct])-1)*100,
    timerMaxMedian:median(g[:timer]),timerMaxWorst:g[:timer].max,
    frameMaxMedian:g[:frames].empty? ? nil : median(g[:frames]),frameMaxWorst:g[:frames].max)
end
output={runs:runs.sort_by{|r| r[:date]},groups:summary}
File.write('/tmp/jed-fused-final-summary.json',JSON.pretty_generate(output)+"\n")
summary.each { |g| puts '%-7s %-9s %-20s n%d direct %.1f worker %.1f overhead %+.1f%% frame %.1f/%.1f longtasks%d' % [g[:host],g[:variant],g[:case],g[:worker].length,g[:directMedian],g[:workerMedian],g[:overheadPercent],g[:frameMaxMedian] || 0,g[:frameMaxWorst] || 0,g[:longTasks]] }
