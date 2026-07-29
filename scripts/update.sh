#!/usr/bin/env bash
# Ship a new gdrd binary to an already-installed target.
# Token / cert / unit file are left untouched.
#
# Usage: ./scripts/update.sh user@host [--source]
#   default: copy local target/release/gdrd
#   --source: rsync + cargo build --release -p gdrd on the target

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="${1:?usage: update.sh user@host [--source]}"
MODE="${2:-binary}"

SUDO_PASS="${GDR_SUDO_PASSWORD:-}"

remote() { ssh -o ConnectTimeout=15 "$1" "${@:2}"; }

if [ "$MODE" = "--source" ] || [ "$MODE" = "source" ]; then
  remote "$TARGET" 'mkdir -p ~/gdr-src'
  rsync -az --delete \
    --exclude target --exclude node_modules --exclude dist --exclude .git \
    "$SCRIPT_DIR/" "$TARGET:~/gdr-src/"
  remote "$TARGET" 'source "$HOME/.cargo/env" 2>/dev/null; cd ~/gdr-src && cargo build --release -p gdrd'
  remote "$TARGET" 'cp ~/gdr-src/target/release/gdrd ~/.local/bin/gdrd'
else
  BIN="$SCRIPT_DIR/target/release/gdrd"
  [ -f "$BIN" ] || { echo "missing $BIN — build locally or pass --source"; exit 1; }
  scp "$BIN" "$TARGET:~/.local/bin/gdrd"
fi

remote "$TARGET" 'systemctl --user restart gdr.service && systemctl --user --no-pager status gdr.service | head -20'
echo "Updated gdrd on $TARGET"
