# Configuration

## Controller: `~/.config/gdr/config.json`

Single source of remembered **devices** for **both** `gdr` and `gdr-mcp`.
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
      "label": "office tower",
      "aliases": ["office"],
      "sudo_password": null,
      "user_password": null
    },
    "local": {
      "address": "localhost",
      "port": 7337,
      "token": "<…>",
      "pin": "<…>",
      "label": "home computer",
      "aliases": ["home"]
    }
  }
}
```

| Field | Required | Notes |
|---|---|---|
| `address` | yes | Host/IP for the TLS data plane. Same-machine: `local` / `localhost` / `.` |
| `port` | no (7337) | gdrd listen port |
| `token` | yes | Plaintext bearer used at Auth (per device) |
| `pin` | recommended | Cert fingerprint |
| `ssh` | recommended | `user@host` for admin-plane CLI ops |
| `label` | opt | Friendly name; matched by `--host` / MCP `dev=` |
| `aliases` | opt | Extra names that resolve to this device |
| `sudo_password` | opt-in | Plaintext; deploy + `gdr_get_password` |
| `user_password` | opt-in | Plaintext; same exposure model |

Lookups accept **id**, **label**, or **alias** (case-insensitive), e.g. `home computer`.

### Device management CLI

```bash
gdr device add home --address 100.x.x.x --token "$TOKEN" --pin "$PIN" \
  --label "home computer" --alias home --ask-sudo --default

gdr device list
gdr device show "home computer"
gdr device set-label local "home computer"
gdr device add-alias local home
gdr device set-sudo home --ask
gdr device set-token home --ask
gdr device default home
gdr device remove office

# Still supported:
gdr host add … / list / show / remove / default
```

### System package management

```bash
gdr service status|start|stop|restart|enable|disable|logs
gdr mcp setup-cursor [--dev "home computer"] [--per-device] [--restart]
gdr mcp status
gdr mcp update --restart-cursor
gdr pkg info|version|paths|update
```

## Cursor MCP + device selection

Devices/tokens/sudo stay in `config.json`. Cursor only needs how to start `gdr-mcp`:

```bash
# Default all tools to one device:
gdr mcp setup-cursor --dev "home computer" --restart

# Or one MCP server entry per device (mention @gdr-local / @gdr-desktop):
gdr mcp setup-cursor --per-device --restart
```

`~/.cursor/mcp.json` example:

```json
{
  "mcpServers": {
    "gdr": {
      "command": "node",
      "args": [
        "/path/to/gdr/mcp-server/dist/index.js",
        "--dev",
        "home computer"
      ]
    },
    "gdr-local": {
      "command": "node",
      "args": ["…/dist/index.js", "--dev", "local"]
    }
  }
}
```

### What actually works for `@gdr -dev="home computer"`

Cursor does **not** parse `-dev=` into MCP argv by itself. These do work:

1. **Tool argument** — agent passes `dev: "home computer"` (or `host:`) on any `gdr_*` tool.  
   Project rule `.cursor/rules/gdr-device.mdc` tells the agent to treat that chat
   shorthand as the active device, run `gdr_status`, and screenshot when visual.
2. **Server default** — `gdr-mcp --dev "home computer"` in mcp.json `args` (no tool arg needed).
3. **Per-device MCP server** — `@gdr-local` / `@gdr-home` after `--per-device` setup.

```text
gdr_status({ "dev": "home computer" })   # token valid?
gdr_screenshot({ "dev": "home computer" })
gdr_list_devices
gdr_device_add({ "id": "local", "local": true, "token": "…", "label": "home computer" })
```

## Resolution order

1. Explicit `--addr` + `--token` (CLI)  
2. Tool `dev=` / `host=` / CLI `--host` / `--dev` (id|label|alias)  
3. MCP process `--dev` / env `GDR_DEV`  
4. Legacy env `GDR_HOST`+`GDR_TOKEN`  
5. `default_host`  
6. Sole configured device  

## Target: `~/.local/share/gdr/` (or package paths)

| Path | Purpose |
|---|---|
| `cert.pem` / `key.pem` | Self-signed TLS |
| `tokens.json` | Hashed multi-token store |
| `audit.log` | JSON-lines audit |

System package also installs `/usr/bin/gdr`, `/usr/bin/gdrd`, `/usr/bin/gdr-mcp`,
`/usr/lib/systemd/user/gdr.service`, `/usr/share/gdr/mcp-server/`.
