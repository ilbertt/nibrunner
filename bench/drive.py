#!/usr/bin/env python3
"""Drive apps on the invocation distribution Azure Functions reported (Shahrad et al., ATC'20):
45% at most hourly, 36% at most once a minute, the rest busier, ~3% more than once a second."""
import argparse, json, random, statistics, threading, time, urllib.request

parser = argparse.ArgumentParser()
parser.add_argument("--apps", type=int, default=400)
parser.add_argument("--seconds", type=int, default=600)
parser.add_argument("--proxy", default="http://127.0.0.1:8080")
parser.add_argument("--out", default="/root/drive-result.json")
args = parser.parse_args()

# share of apps, and the gap between invocations for one of them
CLASSES = [
    ("hourly", 0.45, 3600),
    ("minutely", 0.36, 90),
    ("busy", 0.16, 10),
    ("hot", 0.03, 1),
]

random.seed(1)
apps, cursor = [], 1
for name, share, period in CLASSES:
    count = round(args.apps * share)
    for _ in range(count):
        if cursor <= args.apps:
            apps.append((cursor, name, period))
            cursor += 1
while cursor <= args.apps:                      # rounding leftovers are the commonest class
    apps.append((cursor, "hourly", 3600))
    cursor += 1

latencies = {name: [] for name, _, _ in CLASSES}
lock = threading.Lock()
stop = threading.Event()

def call(index):
    request = urllib.request.Request(f"{args.proxy}/info", headers={"Host": f"app-{index}.bench.local"})
    started = time.perf_counter()
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            body = json.loads(response.read())
        return (time.perf_counter() - started) * 1000, body
    except Exception:
        return None, None

def worker(index, name, period):
    time.sleep(random.uniform(0, min(period, 30)))   # do not synchronise the fleet
    while not stop.is_set():
        ms, body = call(index)
        if ms is not None:
            with lock:
                latencies[name].append((ms, body.get("boots"), body.get("uptimeS")))
        if stop.wait(period * random.uniform(0.8, 1.2)):
            return

threads = [threading.Thread(target=worker, args=a, daemon=True) for a in apps]
for t in threads:
    t.start()
time.sleep(args.seconds)
stop.set()
time.sleep(2)

summary = {}
for name, _, _ in CLASSES:
    samples = [ms for ms, _, _ in latencies[name]]
    if not samples:
        continue
    samples.sort()
    boots = [b for _, b, _ in latencies[name] if b is not None]
    summary[name] = {
        "apps": sum(1 for _, n, _ in apps if n == name),
        "calls": len(samples),
        "p50_ms": round(statistics.median(samples), 2),
        "p95_ms": round(samples[int(len(samples) * 0.95)], 2),
        "max_ms": round(samples[-1], 2),
        "slow_over_10ms": sum(1 for s in samples if s > 10),
        "max_boots": max(boots) if boots else None,
    }
print(json.dumps(summary, indent=2))
open(args.out, "w").write(json.dumps(summary, indent=2))
