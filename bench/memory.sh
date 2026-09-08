#!/usr/bin/env bash
# What a tenant that actually uses memory costs the host, at a fixed app count.
set -uo pipefail
STATE=/var/lib/nibrunner; PROXY=http://127.0.0.1:8080
D=$(sha256sum $STATE/artifact-store/loadgen | cut -d' ' -f1)
S=$(stat -c%s $STATE/artifact-store/loadgen)
N=${N:-128}
HOLDS=${HOLDS:-"0 32 64 128 256"}
RESULTS=/root/memory-n${N}.csv
EMPTY='{"hostId":"bench-host","volumes":[],"instances":[],"checkpoints":[],"exports":[]}'

echo "apps,hold_mib,declared_mib,running,mem_used_mib,mem_per_app_mib,apps_that_would_fit,load1" > "$RESULTS"
total=$(free -m | awk '/^Mem:/ {print $2}')

for hold in $HOLDS; do
  # the guest must be able to hold it: bun's own footprint plus what we ask it to keep
  declared=$(( hold + 256 ))
  echo "=== $N apps holding ${hold} MiB, ${declared} MiB declared ==="
  echo "$EMPTY" > "$STATE/desired.json"
  for _ in $(seq 1 300); do
    left=$(pgrep -c firecracker 2>/dev/null || true); [[ "${left:-0}" == "0" ]] && break; sleep 2
  done
  base=$(free -m | awk '/^Mem:/ {print $3}')

  python3 /root/gen-desired.py "$N" --digest "$D" --size "$S" --object-key loadgen \
    --memory-mib "$declared" --volume-mib 64 --env "SPIN_PCT=0" --env "HOLD_MIB=$hold" \
    --state running --out "$STATE/desired.json.tmp"
  mv "$STATE/desired.json.tmp" "$STATE/desired.json"

  start=$(date +%s); running=0
  while (( $(date +%s) - start < 600 )); do
    running=$(jq -r '[.instances[]? | select(.state=="running")] | length' "$STATE/reported.json" 2>/dev/null || echo 0)
    (( running >= N )) && break
    sleep 5
  done
  sleep 45

  used=$(free -m | awk '/^Mem:/ {print $3}')
  per=$(( running > 0 ? (used - base) / running : 0 ))
  fit=$(( per > 0 ? (total - 2000) / per : 0 ))
  load1=$(awk '{print $1}' /proc/loadavg)
  echo "$N,$hold,$declared,$running,$used,$per,$fit,$load1" >> "$RESULTS"
  echo "  running=$running used=${used}MiB per-app=${per}MiB -> a 64 GB host would hold ~${fit}"
done
echo "=== done ==="
cat "$RESULTS"
