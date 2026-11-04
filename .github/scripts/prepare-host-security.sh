#!/usr/bin/env bash
set -euo pipefail

smt_directory=/sys/devices/system/cpu/smt

if [[ "$(cat "$smt_directory/active")" == 1 ]]; then
    echo forceoff > "$smt_directory/control"
fi
test "$(cat "$smt_directory/active")" = 0
