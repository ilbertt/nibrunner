#!/usr/bin/env bash
# Each step starts from an empty host, so a step is never judged on the previous step's wreckage.
set -uo pipefail
STATE=/var/lib/nibrunner
PROXY=http://127.0.0.1:8080
DIGEST=$(sha256sum "$STATE/artifact-store/todos" | cut -d' ' -f1)
SIZE=$(stat -c%s "$STATE/artifact-store/todos")
MEMORY_MIB=${MEMORY_MIB:-128}
STEPS=${STEPS:-"32 40 48 56 63"}
SETTLE=${SETTLE:-200}
RESULTS=${RESULTS:-/root/results-${MEMORY_MIB}mib.csv}
EMPTY='{"hostId":"bench-host","volumes":[],"instances":[],"checkpoints":[],"exports":[]}'

echo "count,memory_mib,converged_s,running,answered,mem_used_mib,mem_avail_mib,load1,states" > "$RESULTS"

for count in $STEPS; do
  echo "=== $count apps at ${MEMORY_MIB} MiB ==="
  echo "$EMPTY" > "$STATE/desired.json"
  for _ in $(seq 1 60); do
    remaining=$(pgrep -c firecracker || true)
    [[ "${remaining:-0}" == "0" ]] && break
    sleep 2
  done
  echo "  host empty, ${remaining:-0} vms left"

  python3 /root/gen-desired.py "$count" --digest "$DIGEST" --size "$SIZE" \
    --memory-mib "$MEMORY_MIB" --state running --out "$STATE/desired.json.tmp"
  mv "$STATE/desired.json.tmp" "$STATE/desired.json"

  start=$(date +%s); running=0
  while (( $(date +%s) - start < SETTLE )); do
    running=$(jq -r '[.instances[]? | select(.state=="running")] | length' "$STATE/reported.json" 2>/dev/null || echo 0)
    (( running >= count )) && break
    sleep 5
  done
  converged=$(( $(date +%s) - start ))

  answered=0
  for index in $(seq 1 "$count"); do
    host="app-$index.bench.local"
    post=$(curl -s -o /dev/null -w '%{http_code}' -m 20 -X POST "$PROXY/todos" \
      -H "Host: $host" -H 'content-type: application/json' -d "{\"title\":\"step $count\"}")
    [[ "$post" == "201" ]] && answered=$((answered + 1))
  done

  states=$(jq -r '[.instances[]?.state] | group_by(.) | map("\(.[0]):\(length)") | join(" ")' "$STATE/reported.json" 2>/dev/null)
  read -r mem_used mem_avail < <(free -m | awk '/^Mem:/ {print $3, $7}')
  load1=$(awk '{print $1}' /proc/loadavg)
  echo "$count,$MEMORY_MIB,$converged,$running,$answered,$mem_used,$mem_avail,$load1,$states" >> "$RESULTS"
  echo "  converged=${converged}s running=$running answered=$answered/$count mem_used=${mem_used}MiB states: $states"
done
echo "=== done ==="
cat "$RESULTS"
