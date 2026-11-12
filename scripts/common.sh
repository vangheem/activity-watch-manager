#!/usr/bin/env bash
# Shared helpers for the macOS installers. Source this; do not execute it.

if [[ -t 1 ]]; then
  BOLD=$'\033[1m'
  DIM=$'\033[2m'
  ACCENT=$'\033[38;5;173m'
  GREEN=$'\033[38;5;65m'
  RED=$'\033[38;5;167m'
  RESET=$'\033[0m'
else
  BOLD=""
  DIM=""
  ACCENT=""
  GREEN=""
  RED=""
  RESET=""
fi

info() { printf '%s%s%s\n' "$ACCENT" "$*" "$RESET"; }
ok() { printf '%s%s%s\n' "$GREEN" "$*" "$RESET"; }
warn() { printf '%s%s%s\n' "$RED" "$*" "$RESET" >&2; }
die() { warn "$*"; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${HOME}/.local/bin"
LAUNCH_DIR="${HOME}/Library/LaunchAgents"
LOG_DIR="${HOME}/Library/Logs/activity-watch-manager"
UID_NUM="$(id -u)"

SERVER_LABEL="com.activitywatch.manager.server"
COLLECTOR_LABEL="com.activitywatch.manager.collector"

ensure_macos() {
  [[ "$(uname)" == "Darwin" ]] || die "This installer is for macOS. Build with cargo on other systems."
}

ensure_clt() {
  if xcode-select -p >/dev/null 2>&1; then
    return
  fi
  warn "Xcode Command Line Tools are required to compile the bundled SQLite."
  xcode-select --install || true
  die "Re-run this script after the Command Line Tools finish installing."
}

ensure_rust() {
  if [[ -f "${HOME}/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "${HOME}/.cargo/env"
  fi
  if command -v cargo >/dev/null 2>&1; then
    return
  fi
  info "Installing Rust via rustup..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  source "${HOME}/.cargo/env"
  command -v cargo >/dev/null 2>&1 || die "Rust install failed."
}

ensure_path() {
  local line='export PATH="$HOME/.local/bin:$PATH"'
  if [[ ":${PATH}:" == *":${HOME}/.local/bin:"* ]]; then
    return
  fi
  local rc="${HOME}/.zshrc"
  if [[ -f "$rc" ]] && grep -q '.local/bin' "$rc"; then
    return
  fi
  touch "$rc"
  printf '\n# activity-watch-manager\n%s\n' "$line" >> "$rc"
  ok "Added ~/.local/bin to PATH in ~/.zshrc. Open a new terminal to use the commands directly."
}

build_release() {
  info "Building release binaries. The first build compiles SQLite and can take a few minutes."
  (cd "$ROOT" && cargo build --release "$@")
}

install_bin() {
  local name="$1"
  mkdir -p "$BIN_DIR"
  cp "${ROOT}/target/release/${name}" "${BIN_DIR}/${name}"
  chmod 755 "${BIN_DIR}/${name}"
}

xml_escape() {
  local s="$1"
  s=${s//&/&amp;}
  s=${s//</&lt;}
  s=${s//>/&gt;}
  printf '%s' "$s"
}

write_plist() {
  local label="$1"
  local bin="$2"
  local out_log="$3"
  local err_log="$4"
  local nice="${5:-0}"
  local plist="${LAUNCH_DIR}/${label}.plist"
  mkdir -p "$LAUNCH_DIR" "$LOG_DIR"
  local home_esc bin_esc out_esc err_esc
  home_esc="$(xml_escape "$HOME")"
  bin_esc="$(xml_escape "$bin")"
  out_esc="$(xml_escape "$out_log")"
  err_esc="$(xml_escape "$err_log")"
  cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${bin_esc}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Background</string>
  <key>Nice</key>
  <integer>${nice}</integer>
  <key>WorkingDirectory</key>
  <string>${home_esc}</string>
  <key>StandardOutPath</key>
  <string>${out_esc}</string>
  <key>StandardErrorPath</key>
  <string>${err_esc}</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
</dict>
</plist>
EOF
}

bootout_label() {
  launchctl bootout "gui/${UID_NUM}/$1" >/dev/null 2>&1 || true
}

bootstrap_plist() {
  local label="$1"
  local plist="${LAUNCH_DIR}/${label}.plist"
  if ! launchctl bootstrap "gui/${UID_NUM}" "$plist"; then
    warn "Could not register ${label}. If it is already loaded, this install still replaced the binary."
    return 0
  fi
  launchctl enable "gui/${UID_NUM}/${label}" >/dev/null 2>&1 || true
  launchctl kickstart -k "gui/${UID_NUM}/${label}" || warn "Could not start ${label}. Check ${LOG_DIR}."
}

lan_ip() {
  ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true
}
