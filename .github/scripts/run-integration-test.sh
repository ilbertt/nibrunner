#!/usr/bin/env bash
set -euo pipefail

fixture_environment=()
if [[ -n "${NIBRUNNER_CUSTOMER_FIXTURE:-}" ]]; then
    fixture_environment+=("NIBRUNNER_CUSTOMER_FIXTURE=$NIBRUNNER_CUSTOMER_FIXTURE" "TMPDIR=$NIBRUNNER_CUSTOMER_FIXTURE")
fi
exec sudo --non-interactive env \
    "${fixture_environment[@]}" \
    "PATH=$PATH" \
    "NIBRUNNER_INTEGRATION=${NIBRUNNER_INTEGRATION:?}" \
    "NIBRUNNER_REMOTE_OCI=${NIBRUNNER_REMOTE_OCI:-0}" \
    "NIBRUNNER_TEST_TENANT=${NIBRUNNER_TEST_TENANT:-}" \
    "$@"
