#!/usr/bin/env bash
# Rotate the legacy GDR_TOKEN in the systemd unit AND seed a new
# tokens.json entry. Old env token stops working immediately once the
# unit restarts (unless it still exists as a non-revoked tokens.json entry).
#
# Usage: ./scripts/rotate-token.sh user@host
# Prints the new plaintext token once — update ~/.config/gdr/config.json.

set -euo pipefail
TARGET="${1:?usage: rotate-token.sh user@host}"
NEW=$(openssl rand -hex 32)

ssh -o ConnectTimeout=15 "$TARGET" bash -s <<EOF
set -euo pipefail
UNIT="\$HOME/.config/systemd/user/gdr.service"
[ -f "\$UNIT" ] || { echo "no gdr.service at \$UNIT"; exit 1; }
sed -i "s/^Environment=GDR_TOKEN=.*/Environment=GDR_TOKEN=$NEW/" "\$UNIT"
# Seed / add as initial-install replacement label
GDR_TOKEN=$NEW "\$HOME/.local/bin/gdrd" --seed-token || true
systemctl --user daemon-reload
systemctl --user restart gdr.service
echo "rotated"
EOF

echo
echo "New token (update every client / config.json profile):"
echo "$NEW"
echo
echo "Tip: gdr host add <name> --address ... --token $NEW --pin <fp> --ssh $TARGET"
