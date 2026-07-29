#!/usr/bin/env bash
# Wire gdr into Cursor's global MCP config (~/.cursor/mcp.json).
#
# Usage:
#   ./scripts/setup-mcp-cursor.sh
#   ./scripts/setup-mcp-cursor.sh --dev "home computer"
#   ./scripts/setup-mcp-cursor.sh --per-device          # gdr + gdr-<id> per device
#   ./scripts/setup-mcp-cursor.sh --system|--repo
#   ./scripts/setup-mcp-cursor.sh --restart
#
# Chat usage that works with agents:
#   @gdr -dev="home computer"   → pass tool arg dev="home computer"
#   Or @ the per-device server: @gdr-home
#
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/common.sh"

MODE=auto
RESTART=0
DEV=""
PER_DEVICE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --system) MODE=system ;;
    --repo|--dev-tree) MODE=repo ;;
    --restart|--restart-cursor) RESTART=1 ;;
    --per-device) PER_DEVICE=1 ;;
    --dev)
      DEV="${2:?--dev requires a value}"
      shift
      ;;
    --dev=*) DEV="${1#--dev=}" ;;
    -h|--help)
      sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) die "unknown arg: $1" ;;
  esac
  shift
done

TARGET=""
case "$MODE" in
  system)
    [ -x /usr/bin/gdr-mcp ] || die "/usr/bin/gdr-mcp not found — run ./scripts/install.sh first"
    TARGET=/usr/bin/gdr-mcp
    USE_WRAPPER=1
    ;;
  repo)
    build_mcp
    TARGET="$ROOT/mcp-server/dist/index.js"
    USE_WRAPPER=0
    ;;
  auto)
    if [ -x /usr/bin/gdr-mcp ]; then
      TARGET=/usr/bin/gdr-mcp
      USE_WRAPPER=1
    else
      echo "No system gdr-mcp — building repo MCP..."
      build_mcp
      TARGET="$ROOT/mcp-server/dist/index.js"
      USE_WRAPPER=0
    fi
    ;;
esac

MCP_JSON="${HOME}/.cursor/mcp.json"
mkdir -p "$(dirname "$MCP_JSON")"

node -e '
const fs = require("fs");
const path = process.argv[1];
const target = process.argv[2];
const useWrapper = process.argv[3] === "1";
const dev = process.argv[4] || "";
const perDevice = process.argv[5] === "1";
const cfgPath = require("os").homedir() + "/.config/gdr/config.json";

function entry(extraArgs) {
  const args = extraArgs || [];
  if (useWrapper) return { command: target, args };
  return { command: "node", args: [target, ...args] };
}

let cfg = { mcpServers: {} };
if (fs.existsSync(path)) cfg = JSON.parse(fs.readFileSync(path, "utf8"));
cfg.mcpServers = cfg.mcpServers || {};

const baseArgs = [];
if (dev) baseArgs.push("--dev", dev);
cfg.mcpServers.gdr = entry(baseArgs);

if (perDevice && fs.existsSync(cfgPath)) {
  const gdr = JSON.parse(fs.readFileSync(cfgPath, "utf8"));
  for (const id of Object.keys(gdr.hosts || {})) {
    const slug = id.replace(/[^a-zA-Z0-9]+/g, "-").replace(/^-|-$/g, "");
    cfg.mcpServers["gdr-" + slug] = entry(["--dev", id]);
  }
}

fs.writeFileSync(path, JSON.stringify(cfg, null, 2) + "\n", { mode: 0o600 });
try { fs.chmodSync(path, 0o600); } catch (_) {}
console.log("Updated", path);
console.log(JSON.stringify(cfg.mcpServers.gdr, null, 2));
if (perDevice) {
  const keys = Object.keys(cfg.mcpServers).filter((k) => k.startsWith("gdr-"));
  console.log("Per-device servers:", keys.join(", ") || "(none)");
}
' "$MCP_JSON" "$TARGET" "$USE_WRAPPER" "$DEV" "$PER_DEVICE"

if [ "$RESTART" = 1 ]; then
  restart_cursor_mcp
elif [ "${GDR_YES:-}" != "1" ]; then
  if confirm "Restart running gdr MCP process now?"; then
    restart_cursor_mcp
  fi
fi

echo
echo "Cursor MCP ready."
echo "  Config:  ~/.cursor/mcp.json"
echo "  Devices: ~/.config/gdr/config.json  (tokens + sudo per device)"
if [ -n "$DEV" ]; then
  echo "  Default: --dev \"$DEV\" (all tools without host/dev use this device)"
fi
echo "  Chat:    @gdr -dev=\"home computer\"  → tool arg dev=\"home computer\""
echo "  Or:      gdr device set-label home \"home computer\""
