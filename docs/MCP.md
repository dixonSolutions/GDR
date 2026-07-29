# MCP server (`gdr-mcp`)

Built with `@modelcontextprotocol/sdk`. Speaks the same TLS+JSON protocol
as the Rust CLI — no Rust toolchain required on the controller.

## Setup

```bash
cd mcp-server
npm install && npm run build
```

**Recommended host config** (secrets stay in `~/.config/gdr/config.json`):

```json
{
  "mcpServers": {
    "gdr": {
      "command": "node",
      "args": ["/absolute/path/to/gdr/mcp-server/dist/index.js"]
    }
  }
}
```

Legacy env form (still supported for single-host Claude Desktop setups):

```json
{
  "mcpServers": {
    "gdr": {
      "command": "node",
      "args": ["/absolute/path/to/gdr/mcp-server/dist/index.js"],
      "env": {
        "GDR_HOST": "100.118.238.2",
        "GDR_PORT": "7337",
        "GDR_TOKEN": "...",
        "GDR_PIN": "..."
      }
    }
  }
}
```

`deploy.sh` menu option 3 / full setup can merge the recommended entry into
Claude Desktop’s config when found.

## Tools

| Tool | Args | Notes |
|---|---|---|
| `gdr_screenshot` / `gnome_screenshot` | `host?` | Returns image content |
| `gdr_click` / `gnome_click` | `host?`, `x`, `y`, `button?` | |
| `gdr_move` / `gnome_move` | `host?`, `x`, `y` | |
| `gdr_key` / `gnome_key` | `host?`, `keycode` | evdev |
| `gdr_type` / `gnome_type` | `host?`, `text` | ASCII MVP |
| `gdr_ping` / `gnome_ping` | `host?` | |
| `gdr_list_hosts` | — | names + flags, no secrets |
| `gdr_get_password` | `host?`, `kind: sudo\|user` | see below |

`host` selects a **saved** profile only. Unknown names error; arbitrary IPs
via tool args are rejected by design.

Connections are pooled per profile and serialized (one in-flight request
per client) so screenshot→click loops stay on one Mutter session.

## `gdr_get_password` — intentional tradeoff

```
gdr_get_password(host?, kind: "sudo" | "user")
  → plaintext password
  → or "No {kind} password is set for host '{host}'."
```

Whatever the tool returns enters the **model context / transcript**, with
whatever retention the MCP host and provider apply. That is a different
exposure than “sits in a chmod-600 local file.”

Built as specified because agents sometimes need to type a password into
a GUI or terminal they are driving.

**Safer alternative (not yet implemented):** `gdr_run_privileged(host, command)`
that uses the stored password *inside the MCP process* with `sudo -S` over
SSH and only returns command output — password never enters the model.
Documented here so we can add it without redesigning config.

## Multi-host

One MCP server process can address many saved profiles:

```
gdr_screenshot({ host: "laptop" })
gdr_screenshot({ host: "desktop" })
```

Resolution mirrors the CLI (`config.ts` ↔ `client/src/config.rs`).
