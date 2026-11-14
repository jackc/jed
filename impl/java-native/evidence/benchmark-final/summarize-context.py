import json, sys, statistics, collections
rows=[json.loads(s) for s in open(sys.argv[1])]
e=json.load(open(sys.argv[2]))
start=e['generated_at'].replace('Z','+00:00');end=e['completed_at'].replace('Z','+00:00')
samples=[r for r in rows if r['event']=='sample' and start <= r['utc'] <= end]
jvms=[r for r in rows if r['event']=='jvm_seen']
known={(r['pid'],r['start_ticks']) for r in jvms}
by=collections.defaultdict(list);unknown=[];overlaps=[];cpu=[]
for s in samples:
 java=[];background=[]
 for p in s['processes']:
  if p['name']=='java':
   java.append(p)
   if (p['pid'],p['start_ticks']) not in known:unknown.append(p)
  if p['name'] not in ('java','javac','performance-go','performance-rust'):
   by[p['name']].append((s['utc'],p));background.append(p)
 cpu.append(sum(p['cpu_percent'] for p in background))
 if len(java)>1:overlaps.append(s['utc'])
report={'scope':'Five-second process CPU/I/O sampling; excludes named actor processes; names alone do not establish ownership. Short-lived processes and new/exited CPU intervals may be missed. No proof of isolation.', 'sample_count':len(samples),'evidence_start':e['generated_at'],'evidence_complete':e['completed_at'],'jvms':jvms,'unknown_java_intervals':unknown,'overlapping_java_intervals':overlaps,'background_cpu_percent_of_one_core':{'median':statistics.median(cpu),'max':max(cpu)},'background':{k:{'samples':len(v),'first':v[0][0],'last':v[-1][0],'pids':sorted(set(p['pid'] for _,p in v)),'median_cpu_percent':statistics.median(p['cpu_percent'] for _,p in v),'max_cpu_percent':max(p['cpu_percent'] for _,p in v),'read_bytes':sum(p['read_bytes'] for _,p in v),'write_bytes':sum(p['write_bytes'] for _,p in v)} for k,v in by.items()}}
print(json.dumps(report,indent=2))
