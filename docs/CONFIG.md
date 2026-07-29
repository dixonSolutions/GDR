# Configuration

## Controller: `~/.config/gdr/config.json`

Single source of remembered connections for **both** `gdr` and `gdr-mcp`.
Always written `chmod 600`.

```json
{
  "default_host": "desktop",
  "hosts": {
    "desktop": {
      "address": "100.118.238.2",
      "port": 7337,
      "token": "<plaintext bearer>",
      "pin": "<sha256 hex of server cert, no colons>",
      "ssh": "borys@100.118.238.2",
      "sudo_password": null,
      "user_password": null
    }
  }
}
```

| Field | Required | Notes |
|---|---|---|
| `address` | yes | Host/IP for the TLS data plane |
| `port` | no (7337) | gdrd listen port |
| `token` | yes | Plaintext bearer used at Auth |
| `pin` | recommended | Cert fingerprint; omit = TOFU warning |
| `ssh` | recommended | `user@host` for admin-plane CLI ops |
| `sudo_password` | opt-in | Plaintext; used by deploy non-interactive + `gdr_get_password` |
| `user_password` | opt-in | Plaintext; same exposure model |

### CLI management

```bash
gdr host add desktop \
  --address 100.118.238.2 --token "$TOKEN" --pin "$PIN" \
  --ssh borys@100.118.238.2 \
  --ask-sudo          # prompts, not echoed
gdr host list
gdr host show desktop
gdr host default desktop
gdr host remove desktop

gdr get-password sudo --host desktop
gdr get-password user --host desktop   # → clear "not set" if missing
```

`deploy.sh` can write this profile automatically after install
(`GDR_PROFILE_NAME`, `GDR_SAVE_SUDO=1`, `GDR_SUDO_PASSWORD=...`).

### Resolution order

See ARCHITECTURE.md. Env vars still work for one-off use:

```bash
export GDR_ADDR=100.118.238.2:7337
export GDR_TOKEN=...
export GDR_PIN=...
gdr ping
```

## Target: `~/.local/share/gdr/`

| Path | Purpose |
|---|---|
| `cert.pem` / `key.pem` | Self-signed TLS material (key is 600) |
| `tokens.json` | Hashed multi-token store (600) |
| `audit.log` | JSON-lines audit trail (self-rotates) |

### `tokens.json` shape

```json
{
  "tokens": [
    {
      "id": "tok_ab12...",
      "token_hash": "<sha256 hex of plaintext>",
      "label": "initial-install",
      "created_at": "2026-07-29T07:00:00+00:00",
      "expires_at": null,
      "scopes": ["all"],
      "last_used_at": null,
      "revoked": false
    }
  ]
}
```

Managed via `gdr token create|list|revoke` (SSH) or `gdrd --seed-token`.

## systemd unit

`~/.config/systemd/user/gdr.service` — written by `deploy.sh`.

```
ExecStart=%h/.local/bin/gdrd --bind 0.0.0.0:7337
Environment=GDR_TOKEN=...
Environment=XDG_RUNTIME_DIR=/run/user/%U
```

`GDR_TOKEN` remains as a legacy full-scope fallback even when `tokens.json`
exists. Prefer issuing scoped tokens for agents and keeping the install
token for yourself.

## Environment variables

| Var | Where | Meaning |
|---|---|---|
| `GDR_TOKEN` | target unit / controller | Bearer (legacy or override) |
| `GDR_TOKENS_PATH` | gdrd | Override tokens.json path |
| `GDR_AUDIT_PATH` | gdrd | Override audit.log path |
| `GDR_ADDR` / `GDR_HOST`+`GDR_PORT` | controller | Connection target |
| `GDR_PIN` | controller | Cert fingerprint |
| `GDR_SUDO_PASSWORD` | deploy.sh | Non-interactive remote sudo |
| `GDR_YES=1` | deploy.sh | Skip confirms (agent/CI) |
| `GDR_INSTALL_METHOD=2` | deploy.sh | Force source build on target |
| `GDR_PROFILE_NAME` | deploy.sh | Name for saved config profile |
| `GDR_SAVE_SUDO=1` | deploy.sh | Persist sudo password into profile |
