#!/usr/bin/env bash
# Ask both binaries whether they are running. Either may be absent.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"

ensure_macos
export PATH="${BIN_DIR}:$PATH"

failed=0
if command -v awm-server >/dev/null 2>&1; then
  awm-server status || failed=1
else
  warn "awm-server is not installed"
  failed=1
fi
echo
if command -v awm-collector >/dev/null 2>&1; then
  awm-collector status || failed=1
else
  warn "awm-collector is not installed"
  failed=1
fi
exit "$failed"
