#!/usr/bin/env bash
# A throwaway GNOME desktop to test the window plane and subscription hooks
# against, without touching the one you are sitting in front of.
#
# It starts a *nested, headless* gnome-shell on its own D-Bus session with a
# 1920x1080 virtual monitor, enables the gdr-windows extension from this
# working tree inside it, and runs a gdrd against it. Windows opened in there
# — chromium, say — are real managed windows on a real compositor that nobody
# can see.
#
# Why nested rather than the desktop you are on: GNOME/Wayland scans for
# extensions only at session start and cannot reload one in place, so testing
# a change to shell-extension/ on your own session means logging out. A nested
# shell starts fresh every time, and picks up the working tree as it is.
#
#   ./scripts/hook-test-rig.sh start     # bring it up, print how to reach it
#   ./scripts/hook-test-rig.sh env       # eval this to point gdr at it
#   ./scripts/hook-test-rig.sh chromium [url]
#   ./scripts/hook-test-rig.sh status
#   ./scripts/hook-test-rig.sh stop      # and take the whole session with it
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RIG="${GDR_RIG_DIR:-${TMPDIR:-/tmp}/gdr-hook-rig}"
PORT="${GDR_RIG_PORT:-7339}"
DISPLAY_NAME="${GDR_RIG_WAYLAND:-gdr-rig}"
# Virtual monitor spec. Overridable because monitor geometry is itself a
# source of capture bugs — a fractionally scaled panel produces an odd-numbered
# stream height, and "does gdrd preroll at WxH" is a question worth being able
# to ask directly.
MONITOR="${GDR_RIG_MONITOR:-1920x1080}"
EXT_UUID="gdr-windows@gdr.dixonsolutions.github.io"
GDRD="${GDR_RIG_GDRD:-$HERE/target/release/gdrd}"

say() { printf '%s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

rig_bus() {
  [ -f "$RIG/bus" ] || die "rig is not running (no $RIG/bus) — ./scripts/hook-test-rig.sh start"
  sed 's/^DBUS=//' "$RIG/bus"
}

cmd_start() {
  [ -x "$GDRD" ] || die "no gdrd at $GDRD — cargo build --release"
  if [ -f "$RIG/shell.pid" ] && kill -0 "$(cat "$RIG/shell.pid")" 2>/dev/null; then
    say "rig already running (shell pid $(cat "$RIG/shell.pid"))"
    cmd_status
    return
  fi
  command -v gnome-shell >/dev/null || die "gnome-shell is not installed"

  rm -rf "$RIG"
  mkdir -p "$RIG/data/gnome-shell/extensions" "$RIG/home"
  cp -r "$HERE/shell-extension/$EXT_UUID" "$RIG/data/gnome-shell/extensions/"

  # Its own session bus, so the extension can own org.gdr.Windows without
  # fighting the shell you are actually using for the name.
  cat > "$RIG/shell.sh" <<EOF
#!/usr/bin/env bash
export XDG_DATA_HOME="$RIG/data"
export XDG_DATA_DIRS="$RIG/data:/usr/local/share:/usr/share"
export GNOME_SHELL_SESSION_MODE=user
unset WAYLAND_SOCKET
exec dbus-run-session -- bash -c '
  echo "DBUS=\$DBUS_SESSION_BUS_ADDRESS" > "$RIG/bus"
  exec gnome-shell --headless --virtual-monitor $MONITOR --wayland-display $DISPLAY_NAME
'
EOF
  chmod +x "$RIG/shell.sh"
  nohup "$RIG/shell.sh" > "$RIG/shell.log" 2>&1 &
  echo $! > "$RIG/shell.pid"

  say "waiting for the nested shell…"
  for _ in $(seq 1 30); do
    [ -S "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/$DISPLAY_NAME" ] && [ -f "$RIG/bus" ] && break
    sleep 1
  done
  [ -f "$RIG/bus" ] || { tail -20 "$RIG/shell.log" >&2; die "nested shell did not start"; }

  DBUS_SESSION_BUS_ADDRESS="$(rig_bus)" XDG_DATA_HOME="$RIG/data" \
    gnome-extensions enable "$EXT_UUID" || die "could not enable $EXT_UUID in the rig"

  # Never let the rig lock or blank. It is a test session with no human at it,
  # and an idle lock five minutes in silently ends whatever is being tested —
  # the screen keeps streaming, so the symptom is "my window disappeared and
  # the numbers stopped changing", not "it locked".
  DBUS_SESSION_BUS_ADDRESS="$(rig_bus)" gsettings set org.gnome.desktop.screensaver lock-enabled false 2>/dev/null || true
  DBUS_SESSION_BUS_ADDRESS="$(rig_bus)" gsettings set org.gnome.desktop.session idle-delay 0 2>/dev/null || true
  DBUS_SESSION_BUS_ADDRESS="$(rig_bus)" gsettings set org.gnome.settings-daemon.plugins.power sleep-inactive-ac-type nothing 2>/dev/null || true

  local token
  token="$(head -c 24 /dev/urandom | base64 | tr -d '/+=')"
  printf '%s' "$token" > "$RIG/token"
  chmod 600 "$RIG/token"
  "$GDRD" --seed-token --token "$token" --tokens-path "$RIG/tokens.json" >/dev/null

  cat > "$RIG/gdrd.sh" <<EOF
#!/usr/bin/env bash
export DBUS_SESSION_BUS_ADDRESS="\$(sed 's/^DBUS=//' "$RIG/bus")"
export WAYLAND_DISPLAY=$DISPLAY_NAME
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
export RUST_LOG=\${RUST_LOG:-info}
exec "$GDRD" --bind 127.0.0.1:$PORT \\
  --tokens-path "$RIG/tokens.json" --audit-path "$RIG/audit.jsonl" \\
  --cert-path "$RIG/cert.pem" --key-path "$RIG/key.pem" \\
  --connector Meta-0 --eager-display
EOF
  chmod +x "$RIG/gdrd.sh"
  nohup "$RIG/gdrd.sh" > "$RIG/gdrd.log" 2>&1 &
  echo $! > "$RIG/gdrd.pid"

  for _ in $(seq 1 20); do
    grep -q "listening on" "$RIG/gdrd.log" 2>/dev/null && break
    sleep 1
  done
  grep -q "listening on" "$RIG/gdrd.log" || { tail -20 "$RIG/gdrd.log" >&2; die "gdrd did not start"; }

  # A HOME of its own, so the MCP server can resolve a device pointing at the
  # rig without editing the real ~/.config/gdr/config.json.
  local pin
  pin="$(grep -o 'fingerprint=[0-9a-f]*' "$RIG/gdrd.log" | head -1 | cut -d= -f2)"
  mkdir -p "$RIG/home/.config/gdr"
  cat > "$RIG/home/.config/gdr/config.json" <<EOF
{
  "default_host": "rig",
  "hosts": {
    "rig": {
      "address": "127.0.0.1",
      "port": $PORT,
      "token": "$token",
      "pin": "$pin",
      "label": "hook test rig"
    }
  }
}
EOF
  chmod 600 "$RIG/home/.config/gdr/config.json"
  cmd_status
}

cmd_env() {
  local pin
  pin="$(grep -o 'fingerprint=[0-9a-f]*' "$RIG/gdrd.log" | head -1 | cut -d= -f2)"
  echo "export GDR_ADDR=127.0.0.1:$PORT"
  echo "export GDR_TOKEN=$(cat "$RIG/token")"
  echo "export GDR_PIN=$pin"
}

# Launch a browser window into the rig.
#
# `--disable-extensions` is not tidiness: this machine's chromium auto-loads
# an extension whose onboarding page opens in a new foreground tab, which
# pushes the page under test into the background — and Chromium does not
# repaint a background tab, so anything watching the screen sees nothing and
# the test looks like a detection bug. `--new-window` for the same reason:
# a second invocation must not become a tab behind the first.
cmd_chromium() {
  local url="${1:-about:blank}"
  command -v chromium >/dev/null || die "chromium is not installed"
  mkdir -p "$RIG/chrome-profile"
  DBUS_SESSION_BUS_ADDRESS="$(rig_bus)" \
  WAYLAND_DISPLAY="$DISPLAY_NAME" \
  XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" \
    nohup chromium --ozone-platform=wayland --user-data-dir="$RIG/chrome-profile" \
      --no-first-run --no-default-browser-check --disable-gpu \
      --disable-extensions --disable-component-extensions-with-background-pages \
      --new-window \
      "$url" >> "$RIG/chromium.log" 2>&1 &
  say "chromium launched into the rig (pid $!)"
}

cmd_status() {
  if [ ! -f "$RIG/bus" ]; then
    say "rig is not running"
    return 1
  fi
  say "rig at $RIG"
  say "  shell pid : $(cat "$RIG/shell.pid" 2>/dev/null || echo '?')  (wayland display $DISPLAY_NAME)"
  say "  gdrd pid  : $(cat "$RIG/gdrd.pid" 2>/dev/null || echo '?')  (127.0.0.1:$PORT)"
  say "  device    : HOME=$RIG/home  →  device 'rig'"
  say ""
  say "  eval \"\$(./scripts/hook-test-rig.sh env)\"   # then: gdr hooks"
  say "  HOME=$RIG/home node mcp-server/e2e-hooks.mjs rig"
}

cmd_stop() {
  # Killing the dbus-run-session takes the whole nested session with it,
  # chromium and gdrd included — they all live on that bus.
  for f in gdrd.pid shell.pid; do
    [ -f "$RIG/$f" ] || continue
    pid="$(cat "$RIG/$f")"
    kill -- -"$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
  done
  pkill -f "user-data-dir=$RIG/chrome-profile" 2>/dev/null || true
  pkill -f "wayland-display $DISPLAY_NAME" 2>/dev/null || true
  sleep 1
  say "rig stopped (files left in $RIG)"
}

case "${1:-status}" in
  start) cmd_start ;;
  env) cmd_env ;;
  chromium) shift; cmd_chromium "$@" ;;
  status) cmd_status ;;
  stop) cmd_stop ;;
  *) die "usage: $0 {start|env|chromium [url]|status|stop}" ;;
esac
