#!/usr/bin/env bash
# Install or update Fusebox on macOS (launchd) or Linux (systemd --user) from
# the latest GitHub release, and keep it running in the background on
# http://127.0.0.1:8317. Running the script again updates to the newest release.
#
#   curl -fsSL https://raw.githubusercontent.com/hiepnhoc/FuseBox/main/install/install.sh | bash
#   bash install.sh [--codex] [--no-service] [--force] [--uninstall]
#
# Environment overrides:
#   FUSEBOX_REPO=DigitalPals/Fusebox   GitHub repo whose releases are used
#   FUSEBOX_VERSION=latest             or a tag such as v0.14.0
#   FUSEBOX_BIN_DIR=~/.local/bin       where the binary goes
#   FUSEBOX_HOME=~/.fusebox            config.yaml, sign-ins, usage database
set -euo pipefail

REPO="${FUSEBOX_REPO:-DigitalPals/Fusebox}"
VERSION="${FUSEBOX_VERSION:-latest}"
BIN_DIR="${FUSEBOX_BIN_DIR:-$HOME/.local/bin}"
FUSEBOX_HOME="${FUSEBOX_HOME:-$HOME/.fusebox}"
CONFIG="$FUSEBOX_HOME/config.yaml"
PORT=8317
LABEL="com.fusebox.proxy"

WITH_CODEX=0; WITH_SERVICE=1; FORCE=0; UNINSTALL=0
for arg in "$@"; do
  case "$arg" in
    --codex) WITH_CODEX=1 ;;
    --no-service) WITH_SERVICE=0 ;;
    --force) FORCE=1 ;;
    --uninstall) UNINSTALL=1 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required"; }

OS="$(uname -s)"
case "$OS" in
  Darwin) PLATFORM=apple-darwin ;;
  Linux)  PLATFORM=unknown-linux-gnu ;;
  *) die "unsupported OS: $OS (use install.ps1 on Windows)" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH=aarch64 ;;
  x86_64|amd64)  ARCH=x86_64 ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac
TARGET="$ARCH-$PLATFORM"

# ---------------------------------------------------------------- service ----
service_stop() {
  if [[ "$OS" == Darwin ]]; then
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  elif command -v systemctl >/dev/null 2>&1; then
    systemctl --user stop fusebox.service 2>/dev/null || true
  fi
}

service_install() {
  mkdir -p "$FUSEBOX_HOME"
  if [[ "$OS" == Darwin ]]; then
    local plist="$HOME/Library/LaunchAgents/$LABEL.plist"
    mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
    cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN_DIR/fusebox</string>
    <string>--config</string>
    <string>$CONFIG</string>
  </array>
  <key>WorkingDirectory</key><string>$FUSEBOX_HOME</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HOME</key><string>$HOME</string>
    <key>FUSEBOX_CONFIG</key><string>$CONFIG</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/fusebox.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/fusebox.log</string>
</dict>
</plist>
PLIST
    launchctl bootstrap "gui/$(id -u)" "$plist"
    launchctl kickstart -k "gui/$(id -u)/$LABEL"
    say "launchd agent $LABEL installed (log: ~/Library/Logs/fusebox.log)"
  elif command -v systemctl >/dev/null 2>&1; then
    local unit_dir="$HOME/.config/systemd/user"
    mkdir -p "$unit_dir"
    cat > "$unit_dir/fusebox.service" <<UNIT
[Unit]
Description=Fusebox
After=network-online.target

[Service]
ExecStart=$BIN_DIR/fusebox --config $CONFIG
WorkingDirectory=$FUSEBOX_HOME
Environment=FUSEBOX_CONFIG=$CONFIG
Restart=always
RestartSec=2
TimeoutStopSec=90s

[Install]
WantedBy=default.target
UNIT
    systemctl --user daemon-reload
    systemctl --user enable --now fusebox.service
    systemctl --user restart fusebox.service
    say "systemd user service fusebox.service installed (logs: journalctl --user -u fusebox)"
    if command -v loginctl >/dev/null 2>&1 && ! loginctl show-user "$USER" 2>/dev/null | grep -q 'Linger=yes'; then
      warn "run 'loginctl enable-linger $USER' so Fusebox also runs when you are not logged in"
    fi
  else
    warn "no launchd or systemd found; start it yourself: fusebox --config $CONFIG"
  fi
}

service_remove() {
  service_stop
  if [[ "$OS" == Darwin ]]; then
    rm -f "$HOME/Library/LaunchAgents/$LABEL.plist"
  elif command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable fusebox.service 2>/dev/null || true
    rm -f "$HOME/.config/systemd/user/fusebox.service"
    systemctl --user daemon-reload 2>/dev/null || true
  fi
}

healthy() { curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; }

wait_healthy() {
  local i
  for i in $(seq 1 30); do
    if healthy; then return 0; fi
    sleep 0.5
  done
  return 1
}

# -------------------------------------------------------------- uninstall ----
if [[ $UNINSTALL -eq 1 ]]; then
  say "Removing Fusebox service and binary"
  service_remove
  rm -f "$BIN_DIR/fusebox"
  say "Done. Sign-ins, config and usage database are still in $FUSEBOX_HOME; delete that folder to remove them too."
  exit 0
fi

# ------------------------------------------------------------- resolve tag ----
need curl; need tar
if [[ "$VERSION" == latest ]]; then
  TAG="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")"
  TAG="${TAG##*/}"
  [[ "$TAG" == v* ]] || die "could not resolve the latest release of $REPO"
else
  TAG="$VERSION"; [[ "$TAG" == v* ]] || TAG="v$TAG"
fi

if [[ $FORCE -eq 0 && -x "$BIN_DIR/fusebox" ]] && "$BIN_DIR/fusebox" --version 2>/dev/null | grep -qF "${TAG#v}"; then
  say "Fusebox $TAG is already installed at $BIN_DIR/fusebox"
  if [[ $WITH_SERVICE -eq 1 ]] && ! healthy; then
    say "It is not running; (re)installing the service"
    service_install
    if wait_healthy; then say "Fusebox is up at http://127.0.0.1:$PORT"; else warn "not answering yet; check the log"; fi
  fi
else
  # ---------------------------------------------------------- download ----
  ASSET="fusebox-$TAG-$TARGET.tar.gz"
  BASE="https://github.com/$REPO/releases/download/$TAG"
  TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
  say "Downloading $ASSET from $REPO"
  curl -fsSL --retry 3 -o "$TMP/$ASSET" "$BASE/$ASSET"
  curl -fsSL --retry 3 -o "$TMP/SHA256SUMS" "$BASE/SHA256SUMS"

  say "Verifying checksum"
  expected="$(grep -F " $ASSET" "$TMP/SHA256SUMS" | awk '{print $1}')"
  [[ -n "$expected" ]] || die "$ASSET is not listed in SHA256SUMS"
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$TMP/$ASSET" | awk '{print $1}')"
  else
    actual="$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')"
  fi
  [[ "$expected" == "$actual" ]] || die "checksum mismatch for $ASSET"

  tar -xzf "$TMP/$ASSET" -C "$TMP"
  BIN="$(find "$TMP" -type f -name fusebox | head -n1)"
  [[ -n "$BIN" ]] || die "fusebox binary not found in $ASSET"

  # ----------------------------------------------------------- install ----
  mkdir -p "$BIN_DIR" "$FUSEBOX_HOME"
  if [[ $WITH_SERVICE -eq 1 ]]; then service_stop; fi
  install -m 0755 "$BIN" "$BIN_DIR/fusebox"
  if [[ "$OS" == Darwin ]]; then xattr -d com.apple.quarantine "$BIN_DIR/fusebox" 2>/dev/null || true; fi
  say "Installed $("$BIN_DIR/fusebox" --version 2>/dev/null || echo "fusebox $TAG") to $BIN_DIR/fusebox"

  if [[ $WITH_SERVICE -eq 1 ]]; then
    service_install
    if wait_healthy; then
      say "Fusebox is up at http://127.0.0.1:$PORT"
    else
      warn "Fusebox is not answering on port $PORT yet; check the log"
    fi
  fi
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) warn "$BIN_DIR is not in your PATH. Add to your shell profile:  export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

# --------------------------------------------------------------- codex ----
CODEX_DONE="or rerun this script with --codex"
if [[ $WITH_CODEX -eq 1 ]]; then
  CODEX_CFG="${CODEX_HOME:-$HOME/.codex}/config.toml"
  mkdir -p "$(dirname "$CODEX_CFG")"
  if [[ -f "$CODEX_CFG" ]] && grep -q '^\[model_providers\.fusebox\]' "$CODEX_CFG"; then
    say "Codex already has [model_providers.fusebox] in $CODEX_CFG; left as is"
  else
    KEPT=""
    if [[ -f "$CODEX_CFG" ]]; then cp "$CODEX_CFG" "$CODEX_CFG.bak"; KEPT="; previous file kept as config.toml.bak"; fi
    {
      # Top-level keys must come before any [table]: new ones first, then the
      # existing file, then the provider table at the end.
      grep -q '^model_provider *=' "$CODEX_CFG" 2>/dev/null || printf 'model_provider = "fusebox"\n'
      grep -q '^model *=' "$CODEX_CFG" 2>/dev/null || printf 'model = "gpt-6-astra"\n'
      [[ -f "$CODEX_CFG" ]] && cat "$CODEX_CFG"
      printf '\n[model_providers.fusebox]\nname = "Fusebox"\nbase_url = "http://127.0.0.1:%s/v1"\nwire_api = "responses"\n' "$PORT"
    } > "$CODEX_CFG.new"
    mv "$CODEX_CFG.new" "$CODEX_CFG"
    say "Codex now uses Fusebox ($CODEX_CFG$KEPT)"
  fi
  CODEX_DONE="already done"
fi

cat <<NEXT

Next steps
  1. Sign in once on this machine (tokens stay in $FUSEBOX_HOME):
       open http://127.0.0.1:$PORT   and click "Connect account", or
       fusebox --config $CONFIG login codex
       fusebox --config $CONFIG login claude
  2. Point Codex at it ($CODEX_DONE), in ~/.codex/config.toml:

       model = "gpt-6-astra"
       model_provider = "fusebox"

       [model_providers.fusebox]
       name = "Fusebox"
       base_url = "http://127.0.0.1:$PORT/v1"
       wire_api = "responses"

  Update later: run this script again.   Remove: bash install.sh --uninstall
NEXT
