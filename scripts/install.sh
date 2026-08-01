#!/usr/bin/env bash
# Install gdr as a system package (apt .deb or dnf .rpm).
#
# Builds from the current tree, packages gdr + gdrd + gdr-mcp, installs via
# the distro package manager, and optionally enables the host daemon.
#
# Usage:
#   ./scripts/install.sh                 # CLI + daemon binary + MCP (no systemd enable)
#   ./scripts/install.sh --host          # also enable systemd --user gdrd + local profile
#   ./scripts/install.sh --host --mcp-cursor
#   GDR_YES=1 GDR_SUDO_PASSWORD=… ./scripts/install.sh --host
#
# Env:
#   GDR_YES=1              non-interactive yes
#   GDR_SUDO_PASSWORD=…    sudo -S (not argv)
#   GDR_BIND=0.0.0.0:7337  daemon bind (--host)
#   GDR_PROFILE_NAME=local controller profile name (--host)
#   GDR_SKIP_DEPS=1        skip apt/dnf dependency install
#
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/common.sh"

WITH_HOST=0
WITH_CURSOR_MCP=0
SKIP_DEPS="${GDR_SKIP_DEPS:-0}"

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --host|--daemon) WITH_HOST=1 ;;
    --mcp-cursor|--cursor-mcp) WITH_CURSOR_MCP=1 ;;
    --skip-deps) SKIP_DEPS=1 ;;
    -h|--help) usage 0 ;;
    *) die "unknown arg: $1 (see --help)" ;;
  esac
  shift
done

echo "gdr system install (version $VERSION)"
echo "  package manager: $(detect_pkg_manager)"
if [ "$WITH_HOST" = 1 ]; then echo "  host daemon:     yes"; else echo "  host daemon:     no"; fi
if [ "$WITH_CURSOR_MCP" = 1 ]; then echo "  Cursor MCP:      yes"; else echo "  Cursor MCP:      no"; fi
echo

if [ "$SKIP_DEPS" != "1" ]; then
  echo "==> Installing build/runtime dependencies..."
  install_build_deps
else
  echo "==> Skipping dependency install (GDR_SKIP_DEPS=1)"
fi

build_binaries
build_mcp
build_and_install_package

if [ "$WITH_HOST" = 1 ]; then
  setup_host_daemon
elif [ "${GDR_YES:-}" != "1" ]; then
  if confirm "Also enable gdrd as a systemd --user host daemon on this machine?"; then
    setup_host_daemon
  fi
fi

if [ "$WITH_CURSOR_MCP" = 1 ]; then
  merge_cursor_mcp /usr/bin/gdr-mcp
elif [ "${GDR_YES:-}" != "1" ]; then
  if confirm "Configure Cursor global MCP (~/.cursor/mcp.json) for gdr?"; then
    merge_cursor_mcp /usr/bin/gdr-mcp
  fi
fi

echo
echo "Install complete."
echo "  gdr      → $(command -v gdr || echo /usr/bin/gdr)"
echo "  gdrd     → $(command -v gdrd || echo /usr/bin/gdrd)"
echo "  gdr-mcp  → $(command -v gdr-mcp || echo /usr/bin/gdr-mcp)"
echo
echo "Day-2:"
echo "  ./scripts/update.sh             # rebuild + reinstall (+ remotes / Cursor)"
echo "  ./scripts/update-mcp.sh         # rebuild MCP only (+ optional Cursor restart)"
echo "  ./scripts/setup-mcp-cursor.sh   # Cursor global MCP wiring"
