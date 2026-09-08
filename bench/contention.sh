#!/usr/bin/env bash
# How hard do microVMs fight? Every app spins at SPIN_PCT and holds HOLD_MIB, and a fixed
# unit of work is timed inside one of them. Fixed work, so time is contention.
set -uo pipefail
STATE=/var/lib/nibrunner; PROXY=http://127.0.0.1:8080
D=$(sha256sum $STATE/artifact-store/loadgen | cut -d' ' -f1)
S=$(stat -c%s $STATE/artifact-store/loadgen)
SPIN=${SPIN:-100}
HOLD=${HOLD:-0}
ROUNDS=${ROUNDS:-200000}
STEPS=${STEPS:-"1 6 12 24 48 96 192"}
RESULTS=${RESULTS:-/root/contention-spin${SPIN}-hold${HOLD}.csv}
EMPTY='{"hostId":"bench-host","volumes":[],"instances":[],"checkpoints":[],"exports":[]}'

cpu_busy_pct() {   # sample /proc/stat over one second
  read -r _ a b c idle rest < /proc/stat; local t1=$((a+b+c+idle)) i1=$idle
  sleep 1
  read -r _ a b c idle rest < /proc/stat; local t2=$((a+b+c+idle)) i2=$idle
  echo $(( 100 - (100 * (i2-i1)) / (t2-t1) ))
}

echo "apps,spin_pct,hold_mib,running,work_ms_median,work_ms_max,cpu_busy_pct,mem_used_mib,mem_per_app_mib,load1" > "$RESULTS"

for n in $STEPS; do
  echo "=== $n apps, spin=${SPIN}% hold=${HOLD}MiB ==="
  echo "$EMPTY" > "$STATE/desired.json"
  for _ in $(seq 1 300); do
    left=$(pgrep -c firecracker 2>/dev/null || true); [[ "${left:-0}" == "0" ]] && break; sleep 2
  done
  base_mem=$(free -m | awk '/^Mem:/ {print $3}')

  python3 /root/gen-desired.py "$n" --digest "$D" --size "$S" --object-key loadgen \
    --memory-mib 512 --volume-mib 64 --env "SPIN_PCT=$SPIN" --env "HOLD_MIB=$HOLD" \
    --state running --out "$STATE/desired.json.tmp"
  mv "$STATE/desired.json.tmp" "$STATE/desired.json"

  start=$(date +%s); running=0
  while (( $(date +%s) - start < 600 )); do
    running=$(jq -r '[.instances[]? | select(.state=="running")] | length' "$STATE/reported.json" 2>/dev/null || echo 0)
    (( running >= n )) && break
    sleep 5
  done
  sleep 45   # let every spinner reach its duty cycle before judging anything

  times=()
  for i in $(seq 1 5); do
    app=$(( (i % n) + 1 ))
    ms=$(curl -s -m 120 "$PROXY/work?rounds=$ROUNDS" -H "Host: app-$app.bench.local" | jq -r '.ms // empty')
    [[ -n "$ms" ]] && times+=("$ms")
  done
  median=$(printf '%s\n' "${times[@]}" | sort -n | awk '{a[NR]=$1} END{print (NR? a[int((NR+1)/2)] : "none")}')
  worst=$(printf '%s\n' "${times[@]}" | sort -n | tail -1)

  busy=$(cpu_busy_pct)
  used=$(free -m | awk '/^Mem:/ {print $3}')
  per=$(( n > 0 ? (used - base_mem) / n : 0 ))
  load1=$(awk '{print $1}' /proc/loadavg)
  echo "$n,$SPIN,$HOLD,$running,${median:-none},${worst:-none},$busy,$used,$per,$load1" >> "$RESULTS"
  echo "  running=$running work=${median}ms (worst ${worst}ms) cpu=${busy}% mem=${used}MiB (${per}MiB/app) load=$load1"
done
echo "=== done ==="
cat "$RESULTS"
