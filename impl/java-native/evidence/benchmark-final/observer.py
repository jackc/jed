#!/usr/bin/env python3
"""Low-frequency process-name CPU/I/O sampling, with safe JVM ownership metadata.
No full argument list or environment is emitted. PID plus kernel start time is
identity. New/exited processes can lack a CPU interval; short-lived work between
samples remains invisible. This is context, not proof of machine isolation.
"""
import datetime
import json
import os
import sys
import time

output, stop_file = sys.argv[1:3]
interval = 5
hz = os.sysconf('SC_CLK_TCK')

def utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()

def process(pid):
    text = open(f'/proc/{pid}/stat').read()
    end = text.rfind(')')
    fields = text[end + 2:].split()
    return {'pid': pid, 'name': text[text.find('(') + 1:end],
            'ppid': int(fields[1]), 'start_ticks': int(fields[19]),
            'cpu_ticks': int(fields[11]) + int(fields[12])}

def link(pid, suffix):
    try:
        return os.readlink(f'/proc/{pid}/{suffix}')
    except OSError:
        return None

def ownership(item):
    pid = item['pid']
    result = dict(item, cwd=link(pid, 'cwd'), executable=link(pid, 'exe'),
                  stdout=link(pid, 'fd/1'), stderr=link(pid, 'fd/2'))
    try:
        tokens = open(f'/proc/{pid}/cmdline', 'rb').read().split(b'\0')
        # Only these two exact known benchmark/check entry points are retained. Never print other arguments.
        allowed = {b'org.jed.PerformanceDiagnostics', b'org.jed.Check'}
        result['jed_main_classes'] = [x.decode('ascii') for x in tokens if x in allowed]
    except OSError:
        result['jed_main_classes'] = []
    ancestors = []
    parent = item['ppid']
    for _ in range(4):
        if parent <= 1:
            break
        try:
            info = process(parent)
            ancestor = dict(info, cwd=link(parent, 'cwd'), executable=link(parent, 'exe'), stdout=link(parent, 'fd/1'))
            if process(parent)['start_ticks'] != info['start_ticks']:
                break
            ancestors.append(ancestor)
            parent = info['ppid']
        except (OSError, ValueError, IndexError):
            break
    result['ancestors'] = ancestors
    try:
        if process(pid)['start_ticks'] == item['start_ticks']:
            return result
    except (OSError, ValueError, IndexError):
        pass
    return dict(item, ownership_unavailable='process exited or PID identity changed during metadata read')

def snapshot():
    records = {}
    for name in os.listdir('/proc'):
        if not name.isdigit():
            continue
        try:
            item = process(int(name))
            try:
                io = dict((k, int(v.strip())) for k, v in
                          (line.split(':', 1) for line in open(f'/proc/{name}/io')))
            except OSError:
                io = {}
            item['read_bytes'] = io.get('read_bytes', 0)
            item['write_bytes'] = io.get('write_bytes', 0)
            records[(item['pid'], item['start_ticks'])] = item
        except (OSError, ValueError, IndexError):
            pass
    return records

with open(output, 'w') as out:
    def emit(value):
        out.write(json.dumps(value) + '\n')
        out.flush()
    emit({'event': 'start', 'utc': utc(), 'cpus': os.cpu_count(),
          'affinity': sorted(os.sched_getaffinity(0)), 'interval_seconds': interval,
          'clock_ticks_per_second': hz, 'kernel': os.uname().release,
          'scope': 'PID/start-time identity; process CPU and physical I/O deltas; safe JVM ownership only; no full args or environment'})
    before = {}
    seen = set()
    started = time.monotonic()
    while True:
        current = snapshot()
        ended = time.monotonic()
        elapsed = ended - started
        for key, item in current.items():
            if key not in seen and item['name'] in ('java', 'javac'):
                emit({'event': 'jvm_seen', 'utc': utc(), **ownership(item)})
                seen.add(key)
        rows = []
        for key, item in current.items():
            old = before.get(key)
            if old is None:
                continue
            cpu = 100 * (item['cpu_ticks'] - old['cpu_ticks']) / hz / elapsed
            read = max(0, item['read_bytes'] - old['read_bytes'])
            written = max(0, item['write_bytes'] - old['write_bytes'])
            if cpu >= 0.5 or read or written:
                rows.append({'pid': item['pid'], 'start_ticks': item['start_ticks'],
                             'ppid': item['ppid'], 'name': item['name'],
                             'cpu_percent': round(cpu, 2), 'read_bytes': read,
                             'write_bytes': written})
        if before:
            emit({'event': 'sample', 'utc': utc(), 'seconds': elapsed,
                  'processes': sorted(rows, key=lambda r: -r['cpu_percent'])})
        before, started = current, ended
        if os.path.exists(stop_file):
            emit({'event': 'stop', 'utc': utc()})
            break
        time.sleep(interval)
