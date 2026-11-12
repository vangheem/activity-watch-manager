#!/usr/bin/env bash
# Remove the login items. Data is kept unless --purge is passed.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/common.sh"

ensure_macos

purge=()
if [[ "${1:-}" == "--purge" ]]; then
  purge=(--purge)
elif [[ -n "${1:-}" ]]; then
  die "Usage: uninstall-macos.sh [--purge]"
fi

if [[ -x "${BIN_DIR}/awm-server" ]]; then
  "${BIN_DIR}/awm-server" uninstall "${purge[@]}" || true
fi
if [[ -x "${BIN_DIR}/awm-collector" ]]; then
  "${BIN_DIR}/awm-collector" uninstall "${purge[@]}" || true
fi
rm -f "${BIN_DIR}/awm-server" "${BIN_DIR}/awm-collector"
ok "Removed ~/.local/bin copies. Homebrew installs are left in place; use brew uninstall."
