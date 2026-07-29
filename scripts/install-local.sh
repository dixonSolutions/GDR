#!/usr/bin/env bash
# Install gdrd on *this* machine (controller == target). No SSH, no IP —
# binds loopback and saves a `local` profile in ~/.config/gdr/config.json.
#
# Usage:
#   ./scripts/install-local.sh
#   GDR_PROFILE_NAME=local GDR_BIND=127.0.0.1:7337 ./scripts/install-local.sh
#
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Always default to `local`. Ignore a stale GDR_PROFILE_NAME meant for remote
# deploy (e.g. desktop) so we never clobber Tailscale/LAN profiles.
if [ -n "${GDR_LOCAL_PROFILE_NAME:-}" ]; then
  PROFILE_NAME="$GDR_LOCAL_PROFILE_NAME"
elif [ "${GDR_PROFILE_NAME:-}" = "local" ] || [ "${GDR_PROFILE_NAME:-}" = "localhost" ]; then
  PROFILE_NAME="$GDR_PROFILE_NAME"
else
  if [ -n "${GDR_PROFILE_NAME:-}" ] && [ "${GDR_PROFILE_NAME}" != "local" ]; then
    echo "note: ignoring GDR_PROFILE_NAME='$GDR_PROFILE_NAME' (use GDR_LOCAL_PROFILE_NAME to override)"
  fi
  PROFILE_NAME="local"
fi
BIND="${GDR_BIND:-127.0.0.1:7337}"
CFG="${XDG_CONFIG_HOME:-$HOME/.config}/gdr/config.json"
BIN_DIR="${HOME}/.local/bin"
SHARE="${HOME}/.local/share/gdr"
UNIT_DIR="${HOME}/.config/systemd/user"

# Refuse to overwrite a non-loopback profile under the same name.
if [ -f "$CFG" ]; then
  existing_addr="$(node -e '
    const fs=require("fs");
    const cfg=JSON.parse(fs.readFileSync(process.argv[1],"utf8"));
    const p=(cfg.hosts||{})[process.argv[2]];
    process.stdout.write(p && p.address ? String(p.address) : "");
  ' "$CFG" "$PROFILE_NAME" 2>/dev/null || true)"
  case "${existing_addr,,}" in
    ""|local|localhost|loopback|this|.|127.0.0.1|::1) ;;
    *)
      echo "error: profile '$PROFILE_NAME' already points at '$existing_addr' (not loopback)." >&2
      echo "       Pick another name: GDR_LOCAL_PROFILE_NAME=my-local $0" >&2
      exit 1
      ;;
  esac
fi

echo "==> Building gdrd (release)..."
cargo build --release -p gdrd --manifest-path "$ROOT/Cargo.toml"

echo "==> Installing binary → $BIN_DIR/gdrd"
mkdir -p "$BIN_DIR" "$SHARE" "$UNIT_DIR"
install -m 755 "$ROOT/target/release/gdrd" "$BIN_DIR/gdrd"

TOKEN="$(openssl rand -hex 32)"

echo "==> Writing systemd --user unit (bind $BIND)..."
cat > "$UNIT_DIR/gdr.service" <<EOF
[Unit]
Description=gdr control server (GNOME desktop remote) — local
After=graphical-session.target

[Service]
Type=simple
ExecStart=%h/.local/bin/gdrd --bind $BIND
Environment=GDR_TOKEN=$TOKEN
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=2
Environment=XDG_RUNTIME_DIR=/run/user/%U

[Install]
WantedBy=default.target
EOF

echo "==> Seeding token store..."
GDR_TOKEN="$TOKEN" "$BIN_DIR/gdrd" --seed-token

echo "==> Enabling user service..."
systemctl --user daemon-reload
systemctl --user enable --now gdr.service
sleep 2

FP=""
if [ -f "$SHARE/cert.pem" ]; then
  FP="$(openssl x509 -in "$SHARE/cert.pem" -noout -fingerprint -sha256 2>/dev/null \
    | sed 's/.*=//;s/://g' | tr 'A-F' 'a-f' || true)"
fi

echo "==> Saving controller profile '$PROFILE_NAME' (address=local)..."
mkdir -p "$(dirname "$CFG")"
node -e '
  const fs = require("fs");
  const path = process.argv[1];
  const name = process.argv[2];
  const token = process.argv[3];
  const pin = process.argv[4] || null;
  let cfg = { hosts: {} };
  if (fs.existsSync(path)) cfg = JSON.parse(fs.readFileSync(path, "utf8"));
  cfg.hosts = cfg.hosts || {};
  const prev = cfg.hosts[name] || {};
  cfg.hosts[name] = {
    // "localhost" resolves via /etc/hosts; CLI/MCP also accept "local".
    address: "localhost",
    port: 7337,
    token,
    pin,
    ssh: prev.ssh ?? null,
    sudo_password: prev.sudo_password ?? null,
    user_password: prev.user_password ?? null,
  };
  if (!cfg.default_host) cfg.default_host = name;
  fs.writeFileSync(path, JSON.stringify(cfg, null, 2) + "\n", { mode: 0o600 });
  try { fs.chmodSync(path, 0o600); } catch (_) {}
  console.log("wrote", path);
' "$CFG" "$PROFILE_NAME" "$TOKEN" "$FP"

echo
echo "Local install OK."
echo "  Profile:          $PROFILE_NAME  (address=local → 127.0.0.1)"
echo "  Bind:             $BIND"
echo "  Token:            $TOKEN"
echo "  Cert fingerprint: ${FP:-"(pending)"}"
echo
echo "Try:  gdr --host $PROFILE_NAME ping"
echo "      gdr --host $PROFILE_NAME screenshot"
