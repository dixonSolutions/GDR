#!/usr/bin/env bash
# Rebuild the MCP server from the current tree (assumes code changes).
# Syncs into the system package path when installed, and can restart the
# Cursor (or other host) MCP process so agents pick up new tools.
#
# Usage:
#   ./scripts/update-mcp.sh
#   ./scripts/update-mcp.sh --restart-cursor
#   ./scripts/update-mcp.sh --restart-mcp          # same as --restart-cursor
#   ./scripts/update-mcp.sh --system               # force sync into /usr/share/gdr
#   GDR_YES=1 ./scripts/update-mcp.sh --restart-cursor
#
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/common.sh"

RESTART_MCP=0
FORCE_SYSTEM=0
ASK_RESTART=1

while [ $# -gt 0 ]; do
  case "$1" in
    --restart-cursor|--restart-mcp|--restart)
      RESTART_MCP=1
      ASK_RESTART=0
      ;;
    --no-restart)
      RESTART_MCP=0
      ASK_RESTART=0
      ;;
    --system) FORCE_SYSTEM=1 ;;
    -h|--help)
      sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "unknown arg: $1" ;;
  esac
  shift
done

echo "gdr update-mcp — assumes mcp-server/ code changes"
echo

build_mcp

SYSTEM_MCP="/usr/share/gdr/mcp-server"
sync_system_mcp() {
  echo "==> Syncing MCP into system path $SYSTEM_MCP ..."
  run_sudo mkdir -p "$SYSTEM_MCP/dist"
  run_sudo rsync -a --delete "$ROOT/mcp-server/dist/" "$SYSTEM_MCP/dist/"
  run_sudo install -m 644 "$ROOT/mcp-server/package.json" "$SYSTEM_MCP/package.json"
  if [ -f "$ROOT/mcp-server/package-lock.json" ]; then
    run_sudo install -m 644 "$ROOT/mcp-server/package-lock.json" "$SYSTEM_MCP/package-lock.json"
  fi
  run_sudo bash -lc "cd '$SYSTEM_MCP' && npm install --omit=dev --ignore-scripts >/dev/null"
  run_sudo install -m 755 "$ROOT/packaging/gdr-mcp.sh" /usr/bin/gdr-mcp
  echo "System MCP updated."
}

if [ "$FORCE_SYSTEM" = 1 ]; then
  sync_system_mcp
elif [ -d "$SYSTEM_MCP" ]; then
  sync_system_mcp
else
  echo "No system MCP install at $SYSTEM_MCP — using repo build only:"
  echo "  $ROOT/mcp-server/dist/index.js"
  echo "Tip: ./scripts/setup-mcp-cursor.sh   # point Cursor at repo or system gdr-mcp"
fi

if [ "$ASK_RESTART" = 1 ] && [ "$RESTART_MCP" = 0 ]; then
  if [ "${GDR_YES:-}" = "1" ]; then
    RESTART_MCP=0
  elif confirm "Restart MCP server process for AI tools (Cursor)?"; then
    RESTART_MCP=1
  fi
fi

if [ "$RESTART_MCP" = 1 ]; then
  restart_cursor_mcp
fi

echo
echo "MCP update complete."
echo "  Repo dist: $ROOT/mcp-server/dist/index.js"
[ -x /usr/bin/gdr-mcp ] && echo "  System:    /usr/bin/gdr-mcp"
