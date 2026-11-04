#!/usr/bin/env bash
set -euo pipefail

exec sudo --non-interactive env \
    "PATH=$PATH" \
    "NIBRUNNER_INTEGRATION=${NIBRUNNER_INTEGRATION:?}" \
    "NIBRUNNER_REMOTE_OCI=${NIBRUNNER_REMOTE_OCI:-0}" \
    "NIBRUNNER_TEST_TENANT=${NIBRUNNER_TEST_TENANT:-}" \
    "$@"
