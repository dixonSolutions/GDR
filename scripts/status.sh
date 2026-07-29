#!/usr/bin/env bash
# Quick health check: service state, recent logs, and the current cert
# fingerprint (so you can confirm what you should be pinning client-side).
#
# Usage: ./scripts/status.sh user@target-ip

set -euo pipefail
TARGET="${1:?usage: status.sh user@host}"

echo "== systemd unit status =="
ssh "$TARGET" 'systemctl --user status gdr.service --no-pager -n 10' || true

echo
echo "== current cert fingerprint (pin this with --pin / GDR_PIN) =="
ssh "$TARGET" 'openssl x509 -in ~/.local/share/gdr/cert.pem -noout -fingerprint -sha256 2>/dev/null | sed "s/.*=//;s/://g" | tr A-F a-f' \
  || echo "(no cert found - has gdrd run at least once?)"

echo
echo "== recent logs =="
ssh "$TARGET" 'journalctl --user -u gdr.service -n 30 --no-pager'
