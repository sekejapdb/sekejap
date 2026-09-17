"""Conservative sampled RSS guard where the Pi kernel lacks memory cgroups."""
import json,pathlib,subprocess,time
root=pathlib.Path('<scratch>')
unit='e4-phase2-native-20260917-r1.service'
invocation='5c6fe7f1644d4aac875ea8d30089bebb'
limit=1536*1024*1024
cg=pathlib.Path(contributor@example.invalid/app.slice')/unit
report={'mode':'sampled summed process RSS; no kernel memory controller', 'interval_seconds':0.25,
        'limit_bytes':limit,'minimum_available_bytes':512*1024*1024,'peak_rss_bytes':0,
        'samples':0,'stopped_unit':False,'invocation':invocation,'result':'RUNNING'}

def write():
 tmp=root/'memory-watch.json.tmp';tmp.write_text(json.dumps(report,indent=2)+'\n');tmp.replace(root/'memory-watch.json')

while True:
 state=subprocess.check_output(['systemctl','--user','show',unit,'--property=InvocationID','--property=ActiveState'],text=True)
 props=dict(line.split('=',1) for line in state.splitlines() if '=' in line)
 if props.get('InvocationID')!=invocation:
  report['result']='STOPPED_MONITOR_DIFFERENT_INVOCATION';break
 if props.get('ActiveState') not in ('active','activating','deactivating'):
  report['result']='UNIT_TERMINAL';break
 try:pids=(cg/'cgroup.procs').read_text().split()
 except FileNotFoundError:continue
 rss=0
 for pid in pids:
  try:lines=pathlib.Path('/proc',pid,'status').read_text().splitlines()
  except (FileNotFoundError,ProcessLookupError):continue
  rss+=sum(int(line.split()[1])*1024 for line in lines if line.startswith('VmRSS:'))
 available=next(int(line.split()[1])*1024 for line in pathlib.Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemAvailable:'))
 report.update(samples=report['samples']+1,peak_rss_bytes=max(report['peak_rss_bytes'],rss),last_rss_bytes=rss,last_available_bytes=available)
 if rss>limit or available<report['minimum_available_bytes']:
  report.update(result='RESOURCE_GUARD_STOP',stopped_unit=True);write()
  subprocess.run(['systemctl','--user','stop',unit],check=True)
  break
 if report['samples']%20==0:write()
 time.sleep(.25)
write()
