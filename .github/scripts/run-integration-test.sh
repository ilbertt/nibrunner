#!/usr/bin/env bash
set -euo pipefail

fixture_environment=()
if [[ -n "${NIBRUNNER_CUSTOMER_FIXTURE:-}" ]]; then
    # The jail uses Unix sockets under this path. Keep it as short as /tmp, but visible to PrivateTmp units.
    fixture_environment+=("NIBRUNNER_CUSTOMER_FIXTURE=$NIBRUNNER_CUSTOMER_FIXTURE" "TMPDIR=/opt")
fi
exec sudo --non-interactive env \
    "${fixture_environment[@]}" \
    "PATH=$PATH" \
    "NIBRUNNER_INTEGRATION=${NIBRUNNER_INTEGRATION:?}" \
    "NIBRUNNER_REMOTE_OCI=${NIBRUNNER_REMOTE_OCI:-0}" \
    "NIBRUNNER_TEST_TENANT=${NIBRUNNER_TEST_TENANT:-}" \
    "$@"
