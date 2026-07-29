#!/usr/bin/env bash
# Fully removes gdrd from a target: stops+disables the service, deletes
# the unit file, the binary, and its generated cert/key. Does NOT remove
# the system packages (gstreamer/pipewire plugins) since other things on
# the machine may depend on them.
#
# Usage: ./scripts/uninstall.sh user@target-ip

set -euo pipefail
TARGET="${1:?usage: uninstall.sh user@host}"

read -p "This will remove gdrd, its systemd unit, and its cert/key from $TARGET. Continue? [y/N] " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
  echo "Aborted."
  exit 0
fi

ssh "$TARGET" bash -s <<'EOF'
set -euo pipefail
systemctl --user stop gdr.service 2>/dev/null || true
systemctl --user disable gdr.service 2>/dev/null || true
rm -f ~/.config/systemd/user/gdr.service
systemctl --user daemon-reload
rm -f ~/.local/bin/gdrd
rm -rf ~/.local/share/gdr
echo "Removed gdrd, unit file, and cert/key."
EOF

echo "Done. If you want to also disable linger for this user (only if"
echo "nothing else relies on it): ssh $TARGET sudo loginctl disable-linger \$USER"
