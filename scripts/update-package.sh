#!/usr/bin/env bash
# Rebuild gdr from the current tree (assumes code changes) and reinstall the
# system package (apt .deb or dnf .rpm). Restarts gdrd if the user unit is active.
#
# Usage:
#   ./scripts/update-package.sh
#   ./scripts/update-package.sh --skip-deps
#   GDR_YES=1 GDR_SUDO_PASSWORD=… ./scripts/update-package.sh
#
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/common.sh"

SKIP_DEPS=0
while [ $# -gt 0 ]; do
  case "$1" in
    --skip-deps) SKIP_DEPS=1 ;;
    -h|--help)
      sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "unknown arg: $1" ;;
  esac
  shift
done

echo "gdr update-package (version $VERSION) — assumes local code changes"
echo

if [ "$SKIP_DEPS" != "1" ] && [ "${GDR_SKIP_DEPS:-}" != "1" ]; then
  if [ "${GDR_YES:-}" = "1" ] || confirm "Refresh apt/dnf build dependencies?"; then
    install_build_deps
  fi
fi

build_binaries
build_mcp
build_and_install_package

if systemctl --user is-active --quiet gdr.service 2>/dev/null; then
  echo "==> Restarting active gdr.service..."
  systemctl --user restart gdr.service
  systemctl --user --no-pager status gdr.service | head -15 || true
fi

echo
echo "Package updated."
echo "  Binaries: gdr / gdrd / gdr-mcp"
echo "  Tip: ./scripts/update-mcp.sh --restart-cursor   # if agents still see old MCP tools"
