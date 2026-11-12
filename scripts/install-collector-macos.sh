#!/usr/bin/env bash
# Build awm-collector and register it to start at login.
# Usage:
#   ./scripts/install-collector-macos.sh
#   ./scripts/install-collector-macos.sh --server http://192.168.1.20:8787 --token <token>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"

args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --server|--token)
      args+=("$1" "${2:-}")
      shift 2
      ;;
    -h|--help)
      echo "Usage: install-collector-macos.sh [--server URL] [--token TOKEN]"
      exit 0
      ;;
    *)
      die "Unknown argument: $1"
      ;;
  esac
done

ensure_macos
ensure_clt
ensure_rust
build_release -p awm-collector
install_bin awm-collector
ensure_path
"${BIN_DIR}/awm-collector" install "${args[@]}"
