#!/usr/bin/env bash
# Interactive installer/manager for gdr (gnome desktop remote).
#
# Usage:
#   ./deploy.sh                         interactive menu, will ask for target
#   ./deploy.sh user@host               interactive menu, target pre-filled
#   ./deploy.sh user@host shell         connect only, skip the menu
#   ./deploy.sh user@host install       full install, skip the menu
#   ./deploy.sh user@host mcp           MCP server setup only, skip the menu
#   ./deploy.sh user@host full          install + MCP
#
# Non-interactive helpers (for agents / CI):
#   GDR_SUDO_PASSWORD=... ./deploy.sh user@host install
#   GDR_PROFILE_NAME=desktop GDR_SAVE_SUDO=1 ./deploy.sh user@host install
#
# The defaults everywhere favor asking rather than assuming.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

LAST_TOKEN=""
LAST_FINGERPRINT=""
SUDO_PASS="${GDR_SUDO_PASSWORD:-}"

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------

confirm() {
  local reply
  # Non-interactive auto-yes when GDR_YES=1
  if [ "${GDR_YES:-}" = "1" ]; then
    return 0
  fi
  read -r -p "$1 [y/N] " reply
  [[ "$reply" =~ ^[Yy]$ ]]
}

ask() { # ask VAR "prompt" "default"
  local __var="$1" __prompt="$2" __default="${3:-}"
  local __input
  if [ -n "$__default" ]; then
    read -r -p "$__prompt [$__default]: " __input
    __input="${__input:-$__default}"
  else
    read -r -p "$__prompt: " __input
  fi
  printf -v "$__var" '%s' "$__input"
}

# Run a remote command with optional sudo -S (password via stdin, not argv).
remote() {
  local target="$1"; shift
  ssh -o ConnectTimeout=15 "$target" "$@"
}

remote_sudo() {
  local target="$1"; shift
  local cmd="$*"
  if [ -n "$SUDO_PASS" ]; then
    # shellcheck disable=SC2029
    printf '%s\n' "$SUDO_PASS" | ssh -o ConnectTimeout=15 "$target" \
      "sudo -S -p '' bash -lc $(printf '%q' "$cmd")"
  else
    ssh -t -o ConnectTimeout=15 "$target" "sudo bash -lc $(printf '%q' "$cmd")"
  fi
}

detect_pkg_manager() {
  remote "$1" 'if command -v apt-get >/dev/null 2>&1; then echo apt;
    elif command -v dnf >/dev/null 2>&1; then echo dnf;
    else echo unknown; fi'
}

controller_config_path() {
  echo "${XDG_CONFIG_HOME:-$HOME/.config}/gdr/config.json"
}

# ---------------------------------------------------------------------------
# system dependency install
# ---------------------------------------------------------------------------

install_system_deps() {
  local target="$1" pkgmgr="$2" need_build_deps="$3"
  local pkgs

  case "$pkgmgr" in
    apt)
      pkgs="gstreamer1.0-pipewire gstreamer1.0-plugins-good gstreamer1.0-plugins-base"
      [ "$need_build_deps" = "yes" ] && pkgs="$pkgs libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libdbus-1-dev pkg-config build-essential curl rsync"
      echo "Installing via apt: $pkgs"
      remote_sudo "$target" "DEBIAN_FRONTEND=noninteractive apt-get install -y $pkgs"
      ;;
    dnf)
      pkgs="pipewire-gstreamer gstreamer1-plugins-good gstreamer1-plugins-base"
      [ "$need_build_deps" = "yes" ] && pkgs="$pkgs gstreamer1-devel gstreamer1-plugins-base-devel dbus-devel pkgconf-pkg-config gcc make curl rsync"
      echo "Installing via dnf: $pkgs"
      remote_sudo "$target" "dnf install -y $pkgs"
      ;;
    *)
      echo "Couldn't detect apt or dnf on $target."
      echo "Install these manually then re-run: gstreamer + pipewire plugins"
      [ "$need_build_deps" = "yes" ] && echo "(and build deps: gstreamer/dbus headers, C toolchain, rsync)"
      confirm "Continue anyway?" || exit 1
      ;;
  esac
}

# ---------------------------------------------------------------------------
# get gdrd onto the target
# ---------------------------------------------------------------------------

install_prebuilt_binary() {
  local target="$1"
  local bin="$SCRIPT_DIR/target/release/gdrd"
  if [ ! -f "$bin" ]; then
    echo "No local build found at $bin"
    echo "Build first: cargo build --release -p gdrd"
    return 1
  fi
  echo "Copying prebuilt binary (target OS/arch/glibc must match)..."
  remote "$target" 'mkdir -p ~/.local/bin'
  scp "$bin" "$target:~/.local/bin/gdrd"
}

build_from_source_on_target() {
  local target="$1"
  echo "Syncing source tree to $target:~/gdr-src ..."
  remote "$target" 'mkdir -p ~/gdr-src'
  rsync -az --delete \
    --exclude target --exclude node_modules --exclude dist --exclude .git \
    --exclude '*.png' --exclude screenshot.png \
    "$SCRIPT_DIR/" "$target:~/gdr-src/"

  echo "Checking for a Rust toolchain on $target..."
  if ! remote "$target" 'command -v cargo >/dev/null 2>&1'; then
    if confirm "No Rust toolchain found on $target. Install via rustup?"; then
      remote "$target" 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
    else
      echo "Can't build without Rust. Aborting this path."
      return 1
    fi
  fi

  echo "Building gdrd on $target (this can take a few minutes the first time)..."
  remote "$target" 'source "$HOME/.cargo/env" 2>/dev/null; cd ~/gdr-src && cargo build --release -p gdrd'
  remote "$target" 'mkdir -p ~/.local/bin && cp ~/gdr-src/target/release/gdrd ~/.local/bin/gdrd'
}

# ---------------------------------------------------------------------------
# systemd --user unit + token store seed
# ---------------------------------------------------------------------------

setup_systemd() {
  local target="$1"
  local token
  token=$(openssl rand -hex 32)

  echo "Enabling linger so the user service can run without an active login..."
  remote_sudo "$target" 'loginctl enable-linger '"$(remote "$target" 'printf %s "$USER"')"

  # Write unit with token in Environment= (legacy path) AND seed tokens.json.
  remote "$target" "mkdir -p ~/.config/systemd/user ~/.local/share/gdr && cat > ~/.config/systemd/user/gdr.service" <<EOF
[Unit]
Description=gdr control server (GNOME desktop remote)
After=graphical-session.target

[Service]
Type=simple
ExecStart=%h/.local/bin/gdrd --bind 0.0.0.0:7337
Environment=GDR_TOKEN=$token
Environment=RUST_LOG=info
Restart=on-failure
RestartSec=2

# Ensure session bus / runtime dir for Mutter + PipeWire
Environment=XDG_RUNTIME_DIR=/run/user/%U

[Install]
WantedBy=default.target
EOF

  # Seed hashed token store (scope=all, never expire) using the binary itself.
  remote "$target" "GDR_TOKEN=$token ~/.local/bin/gdrd --seed-token" || {
    echo "Warning: --seed-token failed; falling back to env-only auth (GDR_TOKEN)."
  }

  remote "$target" 'systemctl --user daemon-reload && systemctl --user enable --now gdr.service'

  # Give it a moment to generate its cert on first start
  sleep 2
  local fingerprint
  fingerprint=$(remote "$target" 'openssl x509 -in ~/.local/share/gdr/cert.pem -noout -fingerprint -sha256 2>/dev/null | sed "s/.*=//;s/://g" | tr A-F a-f' || echo "")

  LAST_TOKEN="$token"
  LAST_FINGERPRINT="$fingerprint"

  echo
  echo "Installed and running."
  echo "  Token:            $token"
  if [ -n "$fingerprint" ]; then
    echo "  Cert fingerprint: $fingerprint"
  else
    echo "  Cert fingerprint: (not ready yet — ./scripts/status.sh $target)"
  fi
}

save_controller_profile() {
  local target="$1" token="$2" fingerprint="$3"
  local host="${target#*@}"
  local profile_name="${GDR_PROFILE_NAME:-}"
  local save_sudo="${GDR_SAVE_SUDO:-}"
  local save_user="${GDR_SAVE_USER:-}"

  if [ -z "$profile_name" ]; then
    if [ "${GDR_YES:-}" = "1" ]; then
      profile_name="${host%%.*}"
    else
      ask profile_name "Save as host profile name" "${host%%.*}"
    fi
  fi

  if [ -z "$save_sudo" ] && [ -n "$SUDO_PASS" ]; then
    if [ "${GDR_YES:-}" = "1" ] || confirm "Also store the sudo password in the local profile? (plaintext, chmod 600)"; then
      save_sudo=1
    fi
  fi

  local cfg
  cfg=$(controller_config_path)
  mkdir -p "$(dirname "$cfg")"

  node -e '
    const fs = require("fs");
    const path = process.argv[1];
    const name = process.argv[2];
    const address = process.argv[3];
    const token = process.argv[4];
    const pin = process.argv[5] || null;
    const ssh = process.argv[6];
    const sudoPass = process.argv[7] || null;
    const userPass = process.argv[8] || null;
    let cfg = { hosts: {} };
    if (fs.existsSync(path)) cfg = JSON.parse(fs.readFileSync(path, "utf8"));
    cfg.hosts = cfg.hosts || {};
    const prev = cfg.hosts[name] || {};
    cfg.hosts[name] = {
      address,
      port: 7337,
      token,
      pin: pin || prev.pin || null,
      ssh,
      sudo_password: sudoPass !== null && sudoPass !== "" ? sudoPass : (prev.sudo_password || null),
      user_password: userPass !== null && userPass !== "" ? userPass : (prev.user_password || null),
    };
    if (!cfg.default_host) cfg.default_host = name;
    fs.writeFileSync(path, JSON.stringify(cfg, null, 2));
    fs.chmodSync(path, 0o600);
    console.log("Wrote", path, "host=", name);
  ' "$cfg" "$profile_name" "$host" "$token" "$fingerprint" "$target" \
    "$( [ "${save_sudo:-}" = "1" ] && printf '%s' "$SUDO_PASS" || true )" \
    "$( [ "${save_user:-}" = "1" ] && printf '%s' "${GDR_USER_PASSWORD:-}" || true )"
}

# ---------------------------------------------------------------------------
# full install flow
# ---------------------------------------------------------------------------

do_install() {
  local target="$1"

  if [ -z "$SUDO_PASS" ] && [ -t 0 ] && [ "${GDR_YES:-}" != "1" ]; then
    if confirm "Provide sudo password now for non-interactive remote apt/dnf?"; then
      read -r -s -p "sudo password for $target: " SUDO_PASS
      echo
    fi
  fi

  local method="${GDR_INSTALL_METHOD:-}"
  if [ -z "$method" ]; then
    echo
    echo "How should gdrd get onto $target?"
    echo "  1) Copy a prebuilt binary from this machine (fast, needs matching OS/arch/glibc)"
    echo "  2) Build from source on the target (rsyncs this tree, installs Rust if needed)"
    if [ "${GDR_YES:-}" = "1" ]; then
      method=2
    else
      ask method "Choice" "2"
    fi
  fi

  local pkgmgr
  pkgmgr=$(detect_pkg_manager "$target")
  echo "Detected package manager on $target: $pkgmgr"

  if [ "$method" = "2" ] || [ "$method" = "source" ]; then
    install_system_deps "$target" "$pkgmgr" "yes"
    build_from_source_on_target "$target"
  else
    install_system_deps "$target" "$pkgmgr" "no"
    install_prebuilt_binary "$target" || {
      echo "Falling back to building from source instead."
      install_system_deps "$target" "$pkgmgr" "yes"
      build_from_source_on_target "$target"
    }
  fi

  setup_systemd "$target"

  if [ "${GDR_YES:-}" = "1" ] || confirm "Save this connection to ~/.config/gdr/config.json?"; then
    save_controller_profile "$target" "$LAST_TOKEN" "$LAST_FINGERPRINT"
  fi

  if [ "${GDR_SKIP_MCP:-}" != "1" ]; then
    if [ "${GDR_YES:-}" = "1" ] || confirm "Also set up the local MCP server config for this host now?"; then
      do_mcp_setup "$target" "$LAST_TOKEN" "$LAST_FINGERPRINT"
    fi
  fi
}

# ---------------------------------------------------------------------------
# MCP server local setup
# ---------------------------------------------------------------------------

do_mcp_setup() {
  local target="$1" token="${2:-}" fingerprint="${3:-}"
  local mcp_dir="$SCRIPT_DIR/mcp-server"
  local host="${target#*@}"

  if [ -z "$token" ]; then
    # Prefer remembered profile
    local cfg
    cfg=$(controller_config_path)
    if [ -f "$cfg" ]; then
      token=$(node -e '
        const c=require(process.argv[1]);
        const host=process.argv[2];
        const name=Object.keys(c.hosts||{}).find(n=> (c.hosts[n].address===host) || (c.hosts[n].ssh||"").endsWith(host));
        const p = name ? c.hosts[name] : (c.default_host && c.hosts[c.default_host]);
        if(p) { console.log(p.token||""); }
      ' "$cfg" "$host" 2>/dev/null || true)
      fingerprint=$(node -e '
        const c=require(process.argv[1]);
        const host=process.argv[2];
        const name=Object.keys(c.hosts||{}).find(n=> (c.hosts[n].address===host) || (c.hosts[n].ssh||"").endsWith(host));
        const p = name ? c.hosts[name] : (c.default_host && c.hosts[c.default_host]);
        if(p && p.pin) console.log(p.pin);
      ' "$cfg" "$host" 2>/dev/null || true)
    fi
  fi
  if [ -z "$token" ]; then
    ask token "GDR_TOKEN for $target" ""
  fi
  if [ -z "$fingerprint" ]; then
    ask fingerprint "Cert fingerprint for $target (leave blank to trust-on-first-use)" ""
  fi

  if [ ! -d "$mcp_dir/dist" ]; then
    if [ "${GDR_YES:-}" = "1" ] || confirm "mcp-server isn't built yet. Run npm install && npm run build now?"; then
      (cd "$mcp_dir" && npm install && npm run build)
    else
      echo "Skipping build - you'll need to build mcp-server before using it."
    fi
  fi

  # Prefer config.json-based MCP (no secrets in Claude Desktop config).
  local entry
  entry=$(cat <<EOF
{
  "gdr": {
    "command": "node",
    "args": ["$mcp_dir/dist/index.js"]
  }
}
EOF
)

  echo
  echo "Recommended MCP config (reads ~/.config/gdr/config.json — no secrets in host config):"
  echo "$entry"
  echo
  echo "Legacy per-host env form (still supported):"
  cat <<EOF
{
  "gdr-$host": {
    "command": "node",
    "args": ["$mcp_dir/dist/index.js"],
    "env": {
      "GDR_HOST": "$host",
      "GDR_PORT": "7337",
      "GDR_TOKEN": "$token",
      "GDR_PIN": "$fingerprint"
    }
  }
}
EOF

  local claude_config=""
  case "$(uname -s)" in
    Darwin) claude_config="$HOME/Library/Application Support/Claude/claude_desktop_config.json" ;;
    Linux)  claude_config="$HOME/.config/Claude/claude_desktop_config.json" ;;
  esac

  if [ -n "$claude_config" ] && [ -f "$claude_config" ]; then
    if [ "${GDR_YES:-}" = "1" ] || confirm "Found Claude Desktop config at $claude_config. Merge the recommended entry in?"; then
      node -e '
        const fs = require("fs");
        const path = process.argv[1];
        const entry = JSON.parse(process.argv[2]);
        const cfg = JSON.parse(fs.readFileSync(path, "utf8"));
        cfg.mcpServers = { ...(cfg.mcpServers || {}), ...entry };
        fs.writeFileSync(path, JSON.stringify(cfg, null, 2));
        console.log("Updated", path);
      ' "$claude_config" "$entry"
    else
      echo "Skipped - paste the entry above into $claude_config under \"mcpServers\" yourself."
    fi
  else
    echo "No Claude Desktop config auto-detected - paste the entry above into your MCP host's config under \"mcpServers\"."
  fi
}

do_shell() {
  ssh -t "$1"
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

TARGET="${1:-}"
ACTION="${2:-}"

if [ -z "$TARGET" ]; then
  ask TARGET "Target (user@host)" ""
fi
[ -n "$TARGET" ] || { echo "Need a target."; exit 1; }

if [ -z "$ACTION" ]; then
  if [ "${GDR_YES:-}" = "1" ]; then
    ACTION=full
  else
    echo
    echo "What do you want to do with $TARGET?"
    echo "  1) Connect only (interactive shell, no setup)"
    echo "  2) Install gdrd (system deps + binary + systemd service)"
    echo "  3) Set up local MCP server config for this host"
    echo "  4) Full setup (install + save profile + MCP)"
    choice=""
    ask choice "Choice" "4"
    case "$choice" in
      1) ACTION=shell ;;
      2) ACTION=install ;;
      3) ACTION=mcp ;;
      *) ACTION=full ;;
    esac
  fi
fi

case "$ACTION" in
  shell)   do_shell "$TARGET" ;;
  install) do_install "$TARGET" ;;
  mcp)     do_mcp_setup "$TARGET" ;;
  full)    do_install "$TARGET" ;;
  *) echo "Unknown action: $ACTION"; exit 1 ;;
esac
