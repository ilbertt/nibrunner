#!/usr/bin/env bash
# Adds apps in steps until something stops answering. Run as root on the host.
set -uo pipefail

STATE=/var/lib/nibrunner
PROXY=http://127.0.0.1:8080
DIGEST=$(sha256sum "$STATE/artifact-store/todos" | cut -d' ' -f1)
SIZE=$(stat -c%s "$STATE/artifact-store/todos")
MEMORY_MIB=${MEMORY_MIB:-256}
STEPS=${STEPS:-"1 2 4 8 16 24 32 40 48 56 63 64"}
SETTLE=${SETTLE:-240}
RESULTS=${RESULTS:-/root/results.csv}

echo "count,converged_s,healthy,answered,mem_used_mib,mem_avail_mib,load1,note" > "$RESULTS"

healthy_count() {
  jq -r '[.instances[]? | select(.state=="running")] | length' "$STATE/reported.json" 2>/dev/null || echo 0
}

state_breakdown() {
  jq -r '[.instances[]?.state] | group_by(.) | map("\(.[0])=\(length)") | join(" ")' \
    "$STATE/reported.json" 2>/dev/null || echo "unreadable"
}

for count in $STEPS; do
  echo "=== $count apps ==="
  python3 /root/gen-desired.py "$count" --digest "$DIGEST" --size "$SIZE" \
    --memory-mib "$MEMORY_MIB" --state running --out "$STATE/desired.json.tmp"
  mv "$STATE/desired.json.tmp" "$STATE/desired.json"

  start=$(date +%s)
  healthy=0
  while (( $(date +%s) - start < SETTLE )); do
    healthy=$(healthy_count)
    (( healthy >= count )) && break
    sleep 5
  done
  converged=$(( $(date +%s) - start ))

  answered=0
  failures=""
  for index in $(seq 1 "$count"); do
    host="app-$index.bench.local"
    post=$(curl -s -o /dev/null -w '%{http_code}' -m 20 -X POST "$PROXY/todos" \
      -H "Host: $host" -H 'content-type: application/json' \
      -d "{\"title\":\"from step $count\"}")
    get=$(curl -s -m 20 "$PROXY/todos" -H "Host: $host")
    if [[ "$post" == "201" ]] && echo "$get" | jq -e 'type=="array" and length>0' >/dev/null 2>&1; then
      answered=$((answered + 1))
    else
      failures="$failures app-$index(post=$post)"
    fi
  done

  read -r mem_used mem_avail < <(free -m | awk '/^Mem:/ {print $3, $7}')
  load1=$(awk '{print $1}' /proc/loadavg)
  note="ok"
  (( answered < count )) && note="FAILED:${failures// /,}"
  echo "$count,$converged,$healthy,$answered,$mem_used,$mem_avail,$load1,$note" >> "$RESULTS"
  echo "  converged=${converged}s healthy=$healthy answered=$answered/$count mem_used=${mem_used}MiB avail=${mem_avail}MiB"
  echo "  states: $(state_breakdown)"

  if (( answered < count )); then
    echo "  broke at $count apps:$failures"
    systemctl is-active nibrunnerd || echo "  daemon is not active"
    break
  fi
done

echo "=== results ==="
column -s, -t "$RESULTS"
