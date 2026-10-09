#!/usr/bin/env bash
# Invoke with bash; kept non-executable for the disposable-runner patch policy.
set -euo pipefail
exec node "$(dirname "$0")/check.mjs"
