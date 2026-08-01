#!/usr/bin/env bash
# Compatibility shim — use ./scripts/update.sh instead.
# Forwards all args; --yes is implied when GDR_YES=1 (unchanged).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "note: update-package.sh is deprecated; use ./scripts/update.sh" >&2
exec "$SCRIPT_DIR/update.sh" "$@"
