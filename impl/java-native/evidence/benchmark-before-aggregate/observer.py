import os,time,json,datetime
OUT='/home/jack/.codex/worktrees/3926/jed/.dev/java-native/benchmark-controlled-second.context.jsonl'
STOP='/tmp/java-benchmark-context.stop'
def snap():
 r={}
 for p in os.listdir('/proc'):
  if not p.isdigit():continue
  try:
   s=open('/proc/'+p+'/stat').read();end=s.rfind(')');f=s[end+2:].split()
   io={}
   try:io=dict((k,int(v.strip())) for k,v in (line.split(':',1) for line in open('/proc/'+p+'/io')))
   except OSError:pass
   r[int(p)]=(s[s.find('(')+1:end],int(f[11])+int(f[12]),io.get('read_bytes',0),io.get('write_bytes',0))
  except (OSError,ValueError,IndexError):pass
 return r
hz=os.sysconf('SC_CLK_TCK');before=snap();start=time.monotonic()
with open(OUT,'w') as out:
 out.write(json.dumps({'event':'start','utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'cpus':os.cpu_count(),'affinity':sorted(os.sched_getaffinity(0)),'interval_seconds':10,'scope':'process names, CPU and actual /proc I/O byte deltas only; shared host diagnostic'})+'\n');out.flush()
 while not os.path.exists(STOP):
  time.sleep(10);after=snap();now=time.monotonic();dt=now-start;rows=[]
  for p,(name,ticks,read,write) in after.items():
   if p not in before:continue
   _,bt,br,bw=before[p];cpu=100*(ticks-bt)/hz/dt
   if cpu>=0.5 or read>br or write>bw:rows.append({'pid':p,'name':name,'cpu_percent':round(cpu,2),'read_bytes':max(0,read-br),'write_bytes':max(0,write-bw)})
  out.write(json.dumps({'utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'seconds':dt,'processes':sorted(rows,key=lambda r:-r['cpu_percent'])})+'\n');out.flush();before=after;start=now
