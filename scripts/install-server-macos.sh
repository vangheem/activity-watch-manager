#!/usr/bin/env bash
# Build awm-server and register it to start at login.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"

ensure_macos
ensure_clt
ensure_rust
build_release -p awm-server
install_bin awm-server
ensure_path
"${BIN_DIR}/awm-server" install
