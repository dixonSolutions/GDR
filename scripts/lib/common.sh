#!/usr/bin/env bash
# Shared helpers for gdr install / update scripts.
# shellcheck shell=bash

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
VERSION="${VERSION:-0.1.0}"
DIST_DIR="${GDR_DIST_DIR:-$ROOT/dist/packages}"

die() { echo "error: $*" >&2; exit 1; }

need_cmd() { command -v "$1" >/dev/null 2>&1 || die "missing command: $1"; }

confirm() {
  local reply
  if [ "${GDR_YES:-}" = "1" ]; then return 0; fi
  read -r -p "$1 [y/N] " reply
  [[ "$reply" =~ ^[Yy]$ ]]
}

run_sudo() {
  # Run a command with sudo. Password via GDR_SUDO_PASSWORD → sudo -S (not argv).
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
    return
  fi
  if [ -n "${GDR_SUDO_PASSWORD:-}" ]; then
    printf '%s\n' "$GDR_SUDO_PASSWORD" | sudo -S -p '' "$@"
  else
    sudo "$@"
  fi
}

detect_pkg_manager() {
  if command -v apt-get >/dev/null 2>&1; then echo apt
  elif command -v dnf >/dev/null 2>&1; then echo dnf
  else echo unknown
  fi
}

install_build_deps() {
  local pm
  pm="$(detect_pkg_manager)"
  case "$pm" in
    apt)
      run_sudo apt-get update
      run_sudo env DEBIAN_FRONTEND=noninteractive apt-get install -y \
        build-essential pkg-config curl rsync openssl \
        libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libdbus-1-dev \
        gstreamer1.0-pipewire gstreamer1.0-plugins-good gstreamer1.0-plugins-base \
        nodejs npm
      ;;
    dnf)
      run_sudo dnf install -y \
        gcc make pkgconf-pkg-config curl rsync openssl \
        gstreamer1-devel gstreamer1-plugins-base-devel dbus-devel \
        pipewire-gstreamer gstreamer1-plugins-good gstreamer1-plugins-base \
        nodejs npm
      ;;
    *)
      die "need apt or dnf to install system packages"
      ;;
  esac
}

ensure_cargo() {
  if command -v cargo >/dev/null 2>&1; then return 0; fi
  if [ -x "$HOME/.cargo/bin/cargo" ]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env" 2>/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
    return 0
  fi
  echo "Rust toolchain not found — installing rustup (user)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
}

build_binaries() {
  ensure_cargo
  echo "==> Building gdr + gdrd (release)..."
  (cd "$ROOT" && cargo build --release -p gdr -p gdrd)
  [ -x "$ROOT/target/release/gdr" ] || die "gdr binary missing"
  [ -x "$ROOT/target/release/gdrd" ] || die "gdrd binary missing"
}

build_mcp() {
  echo "==> Building mcp-server..."
  need_cmd node
  need_cmd npm
  (cd "$ROOT/mcp-server" && npm install && npm run build)
  [ -f "$ROOT/mcp-server/dist/index.js" ] || die "mcp-server dist missing"
}

stage_package_tree() {
  local stage="$1"
  rm -rf "$stage"
  mkdir -p \
    "$stage/usr/bin" \
    "$stage/usr/share/gdr/mcp-server" \
    "$stage/usr/lib/systemd/user" \
    "$stage/usr/share/doc/gdr"

  install -m 755 "$ROOT/target/release/gdr" "$stage/usr/bin/gdr"
  install -m 755 "$ROOT/target/release/gdrd" "$stage/usr/bin/gdrd"
  install -m 755 "$ROOT/packaging/gdr-mcp.sh" "$stage/usr/bin/gdr-mcp"
  install -m 644 "$ROOT/packaging/gdr.service" "$stage/usr/lib/systemd/user/gdr.service"

  # MCP runtime (dist + package.json + production node_modules)
  rsync -a --delete \
    --exclude node_modules \
    --exclude src \
    --exclude tsconfig.json \
    "$ROOT/mcp-server/dist/" "$stage/usr/share/gdr/mcp-server/dist/"
  install -m 644 "$ROOT/mcp-server/package.json" "$stage/usr/share/gdr/mcp-server/package.json"
  if [ -f "$ROOT/mcp-server/package-lock.json" ]; then
    install -m 644 "$ROOT/mcp-server/package-lock.json" "$stage/usr/share/gdr/mcp-server/package-lock.json"
  fi
  (cd "$stage/usr/share/gdr/mcp-server" && npm install --omit=dev --ignore-scripts >/dev/null)

  install -m 644 "$ROOT/README.md" "$stage/usr/share/doc/gdr/README.md"
  if [ -d "$ROOT/docs" ]; then
    mkdir -p "$stage/usr/share/doc/gdr/docs"
    rsync -a "$ROOT/docs/" "$stage/usr/share/doc/gdr/docs/"
  fi
}

build_deb() {
  local stage arch deb
  arch="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
  stage="$(mktemp -d /tmp/gdr-deb.XXXXXX)"
  trap 'rm -rf "$stage"' RETURN

  stage_package_tree "$stage"
  mkdir -p "$stage/DEBIAN"
  cat > "$stage/DEBIAN/control" <<EOF
Package: gdr
Version: ${VERSION}
Section: utils
Priority: optional
Architecture: ${arch}
Maintainer: gdr maintainers <gdr@localhost>
Depends: nodejs (>= 18), openssl, gstreamer1.0-pipewire | pipewire
Recommends: gstreamer1.0-plugins-good, gstreamer1.0-plugins-base
Description: GNOME desktop remote (gdr CLI + gdrd + MCP)
 Asymmetric remote control for GNOME/Wayland: gdr client, gdrd daemon,
 and gdr-mcp for AI coding tools (Cursor, etc.).
EOF
  cat > "$stage/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v systemctl >/dev/null 2>&1; then
  systemctl --user daemon-reload 2>/dev/null || true
fi
exit 0
EOF
  chmod 755 "$stage/DEBIAN/postinst"

  mkdir -p "$DIST_DIR"
  deb="$DIST_DIR/gdr_${VERSION}_${arch}.deb"
  need_cmd dpkg-deb
  dpkg-deb --build "$stage" "$deb" >/dev/null 2>&1
  [ -f "$deb" ] || die "dpkg-deb failed to create $deb"
  printf '%s\n' "$deb"
}

build_rpm() {
  local stage arch rpm top
  arch="$(uname -m)"
  stage="$(mktemp -d /tmp/gdr-rpm.XXXXXX)"
  top="$stage/rpmbuild"
  mkdir -p "$top"/{BUILD,RPMS,SOURCES,SPECS,SRPMS}
  trap 'rm -rf "$stage"' RETURN

  local rootfs="$stage/root"
  stage_package_tree "$rootfs"

  cat > "$top/SPECS/gdr.spec" <<EOF
Name:           gdr
Version:        ${VERSION}
Release:        1%{?dist}
Summary:        GNOME desktop remote (CLI + daemon + MCP)
License:        MIT
BuildArch:      ${arch}

Requires:       nodejs >= 18
Requires:       openssl

%description
Asymmetric remote control for GNOME/Wayland: gdr, gdrd, and gdr-mcp.

%install
mkdir -p %{buildroot}
cp -a ${rootfs}/. %{buildroot}/

%files
/usr/bin/gdr
/usr/bin/gdrd
/usr/bin/gdr-mcp
/usr/lib/systemd/user/gdr.service
/usr/share/gdr/
/usr/share/doc/gdr/

%post
if command -v systemctl >/dev/null 2>&1; then
  systemctl --user daemon-reload 2>/dev/null || true
fi

%changelog
* $(date '+%a %b %d %Y') gdr maintainers <gdr@localhost> - ${VERSION}-1
- Package build from source tree
EOF

  need_cmd rpmbuild
  rpmbuild -bb --define "_topdir $top" "$top/SPECS/gdr.spec" >/dev/null
  rpm="$(find "$top/RPMS" -name 'gdr-*.rpm' | head -1)"
  [ -n "$rpm" ] || die "rpmbuild produced no rpm"
  mkdir -p "$DIST_DIR"
  cp "$rpm" "$DIST_DIR/"
  printf '%s\n' "$DIST_DIR/$(basename "$rpm")"
}

install_package_file() {
  local pkg="$1"
  local pm
  pm="$(detect_pkg_manager)"
  case "$pkg" in
    *.deb)
      [ "$pm" = apt ] || die "got .deb but package manager is $pm"
      run_sudo dpkg -i "$pkg" || run_sudo apt-get install -f -y
      ;;
    *.rpm)
      [ "$pm" = dnf ] || die "got .rpm but package manager is $pm"
      run_sudo dnf install -y "$pkg"
      ;;
    *)
      die "unknown package type: $pkg"
      ;;
  esac
}

build_and_install_package() {
  local pm pkg
  pm="$(detect_pkg_manager)"
  case "$pm" in
    apt) pkg="$(build_deb)" ;;
    dnf) pkg="$(build_rpm)" ;;
    *) die "need apt or dnf" ;;
  esac
  echo "==> Installing $pkg"
  install_package_file "$pkg"
  echo "Installed system package: $pkg"
}

setup_host_daemon() {
  local bind="${GDR_BIND:-0.0.0.0:7337}"
  local token profile_name
  profile_name="${GDR_PROFILE_NAME:-local}"

  echo "==> Enabling gdrd host daemon (systemd --user)..."
  mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user" \
           "${XDG_DATA_HOME:-$HOME/.local/share}/gdr"

  # Override bind if requested (drop-in).
  if [ "$bind" != "0.0.0.0:7337" ]; then
    local dropin="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/gdr.service.d"
    mkdir -p "$dropin"
    cat > "$dropin/bind.conf" <<EOF
[Service]
ExecStart=
ExecStart=/usr/bin/gdrd --bind $bind
EOF
  fi

  token="$(openssl rand -hex 32)"
  # Prefer linger so the user service survives logout (needs sudo).
  if command -v loginctl >/dev/null 2>&1; then
    run_sudo loginctl enable-linger "$USER" 2>/dev/null || true
  fi

  GDR_TOKEN="$token" /usr/bin/gdrd --seed-token || \
    die "gdrd --seed-token failed"

  # Keep token in unit for legacy fallback + documentation of first seed.
  local unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  mkdir -p "$unit_dir/gdr.service.d"
  cat > "$unit_dir/gdr.service.d/token.conf" <<EOF
[Service]
Environment=GDR_TOKEN=$token
EOF

  systemctl --user daemon-reload
  systemctl --user enable --now gdr.service
  sleep 2

  local fp="" cfg
  if [ -f "${XDG_DATA_HOME:-$HOME/.local/share}/gdr/cert.pem" ]; then
    fp="$(openssl x509 -in "${XDG_DATA_HOME:-$HOME/.local/share}/gdr/cert.pem" -noout -fingerprint -sha256 2>/dev/null \
      | sed 's/.*=//;s/://g' | tr 'A-F' 'a-f' || true)"
  fi

  cfg="${XDG_CONFIG_HOME:-$HOME/.config}/gdr/config.json"
  mkdir -p "$(dirname "$cfg")"
  node -e '
    const fs = require("fs");
    const path = process.argv[1];
    const name = process.argv[2];
    const token = process.argv[3];
    const pin = process.argv[4] || null;
    const bind = process.argv[5] || "0.0.0.0:7337";
    let address = "localhost";
    if (bind.startsWith("127.0.0.1") || bind.startsWith("localhost")) address = "localhost";
    let cfg = { hosts: {} };
    if (fs.existsSync(path)) cfg = JSON.parse(fs.readFileSync(path, "utf8"));
    cfg.hosts = cfg.hosts || {};
    const prev = cfg.hosts[name] || {};
    cfg.hosts[name] = {
      address,
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
  ' "$cfg" "$profile_name" "$token" "$fp" "$bind"

  echo
  echo "Host daemon ready."
  echo "  Profile: $profile_name"
  echo "  Bind:    $bind"
  echo "  Token:   $token"
  echo "  Pin:     ${fp:-pending}"
  echo "  Try:     gdr --host $profile_name ping"
}

merge_cursor_mcp() {
  local mcp_cmd="${1:-/usr/bin/gdr-mcp}"
  local mcp_json="${HOME}/.cursor/mcp.json"
  mkdir -p "$(dirname "$mcp_json")"
  node -e '
    const fs = require("fs");
    const path = process.argv[1];
    const cmd = process.argv[2];
    let cfg = { mcpServers: {} };
    if (fs.existsSync(path)) cfg = JSON.parse(fs.readFileSync(path, "utf8"));
    cfg.mcpServers = cfg.mcpServers || {};
    // Wrapper binary vs node + absolute path to dist/index.js
    if (cmd.endsWith("gdr-mcp") || cmd === "gdr-mcp") {
      cfg.mcpServers.gdr = { command: cmd, args: [] };
    } else {
      cfg.mcpServers.gdr = { command: "node", args: [cmd] };
    }
    fs.writeFileSync(path, JSON.stringify(cfg, null, 2) + "\n", { mode: 0o600 });
    try { fs.chmodSync(path, 0o600); } catch (_) {}
    console.log("Updated", path, "→", JSON.stringify(cfg.mcpServers.gdr));
  ' "$mcp_json" "$mcp_cmd"
}

restart_cursor_mcp() {
  echo "==> Restarting Cursor gdr MCP process (Cursor will respawn on next use)..."
  pkill -f '/usr/share/gdr/mcp-server/dist/index.js' 2>/dev/null || true
  pkill -f '/usr/bin/gdr-mcp' 2>/dev/null || true
  # Also repo-path MCP used during development
  pkill -f "$ROOT/mcp-server/dist/index.js" 2>/dev/null || true
  sleep 0.3
  echo "Done. Reload MCP in Cursor if tools look stale (Command Palette → MCP: Restart / Reload Window)."
}
