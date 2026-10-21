#!/usr/bin/env ruby
# Generate controls outside the repository; preserve the archived plan unchanged.
require 'json'
require 'fileutils'
out = ARGV.fetch(0, '/tmp/jed-prepared-join')
FileUtils.mkdir_p(out)
base = JSON.parse(File.read(File.join(__dir__, 'plan.json')))
targets = base['workloads'].select { |w| %w[indexed_join join_text_predicate join_text_projection join_integer_outputs].include?(w['name']) }
write = ->(name, workloads, setup=[]) { File.write(File.join(out,"#{name}.json"),JSON.pretty_generate(base.merge('workloads'=>workloads,'setup'=>setup))) }
write.call('targets', targets)
controls = targets.flat_map do |w|
 [1,8,9,20,50].map { |span| w.merge('name'=>"#{w['name']}_bindings#{span}",'iterations'=>span*10,'warmup'=>200,'span'=>span,'step'=>1,'offset'=>17) } +
 [w.merge('name'=>"#{w['name']}_miss",'offset'=>1000)]
end
# Different output expressions and join-side orientation exercise the structural guard independently
# of the four original benchmark SQL strings.
controls += [
 targets[0].merge('name'=>'reverse_key','sql'=>'SELECT b.id,a.score+1 FROM items a JOIN items b ON a.id=b.id WHERE b.id=$1 ORDER BY a.id'),
 targets[0].merge('name'=>'key_range','sql'=>'SELECT a.id,b.id FROM items a JOIN items b ON b.id>=a.id WHERE a.id=$1 AND b.id<$1+3 ORDER BY b.id LIMIT 3'),
 targets[0].merge('name'=>'skew_equality','sql'=>'SELECT a.id,b.id FROM items a JOIN items b ON b.id=a.id WHERE a.bucket=$1 ORDER BY a.id','span'=>10,'step'=>1,'iterations'=>20)
]
write.call('controls',controls)
write.call('analyzed',controls,['ANALYZE items'])
write.call('skewed',controls,["UPDATE items SET bucket=1 WHERE id%10<>0",'CREATE INDEX items_bucket ON items(bucket)','ANALYZE items'])
