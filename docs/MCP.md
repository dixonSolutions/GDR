# MCP server (`gdr-mcp`)

Built with `@modelcontextprotocol/sdk`. Speaks the same TLS+JSON protocol
as the Rust CLI — no Rust toolchain required on the controller.

## Setup

```bash
cd mcp-server
npm install && npm run build
```

**Recommended Cursor global config** — prefer the helper:

```bash
gdr mcp setup-cursor --dev "home computer" --restart
gdr mcp setup-cursor --per-device --restart   # @gdr-local, @gdr-desktop, …
./scripts/setup-mcp-cursor.sh --dev "home computer"
./scripts/update-mcp.sh --restart-cursor
```

Devices (token + optional sudo) live in `~/.config/gdr/config.json`.
Select with tool arg `dev=` / `host=` (id, label, or alias), or bind a
default via `gdr-mcp --dev "…"`. Chat shorthand for agents:
`@gdr -dev="home computer"` → pass `dev: "home computer"` on tools.

Manual `~/.cursor/mcp.json` (secrets stay in `~/.config/gdr/config.json`;
multiple machines/tokens are chosen per tool call):

```json
{
  "mcpServers": {
    "gdr": {
      "command": "gdr-mcp",
      "args": []
    }
  }
}
```

Or repo/dev form:

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

Add as many host profiles as you need under `~/.config/gdr/config.json`
(each with its own `token` / optional `pin`). MCP tools take `host=<name>`
(e.g. `local`, `desktop`). Call `gdr_list_hosts` first when unsure.

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
| `gdr_screenshot` / `gnome_screenshot` | `host?`, `layout?` | Default `layout=agent` downscales to ≤1440×900 and returns geometry JSON + PNG; `raw` is 1:1 native |
| `gdr_click` / `gnome_click` | `host?`, `x`, `y`, `button?`, `clicks?` | `x,y` in latest screenshot image space; remapped to stream pixels. Requires a prior screenshot. `clicks=2` = double-click |
| `gdr_double_click` | `host?`, `x`, `y`, `button?` | shorthand |
| `gdr_move` / `gnome_move` | `host?`, `x`, `y` | same image-space remap as click; updates tracked cursor |
| `gdr_cursor` | `host?` | last known `{x,y,known}` from gdr moves |
| `gdr_key` / `gnome_key` | `host?`, `key`/`keycode`, `modifiers?` | names or evdev; mods held for tap |
| `gdr_hotkey` | `host?`, `keys` | `"Alt+F4"`, `"Super+PageDown"` |
| `gdr_input` | `host?`, `steps[]` | flexible ordered sequence (see below) |
| `gdr_type` / `gnome_type` | `host?`, `text` | ASCII MVP |
| `gdr_ping` / `gnome_ping` | `host?` | |
| `gdr_status` | `host?` / `dev?` | Resolve device + Ping; `auth: valid\|failed` (no secrets) |
| `gdr_list_devices` / `gdr_list_hosts` | — | id, label, aliases, flags (no secrets) |
| `gdr_device_add` | `id`, `token?`, `local?` / `address?`, … | Add/update `~/.config/gdr/config.json` |
| `gdr_device_remove` | `dev` | Remove profile by id/label/alias |
| `gdr_device_default` | `dev` | Set `default_host` |
| `gdr_get_password` | `host?`/`dev?`, `kind: sudo\|user` | see below |

Every control tool accepts **`host`** or **`dev`** (same meaning): device id,
label (`"home computer"`), or alias.

### Flexible keyboard: `gdr_hotkey` + `gdr_input`

Chords need held modifiers. Prefer these over tapping keys one-by-one:

```json
{ "keys": "Super+PageDown" }
```

```json
{
  "steps": [
    { "hotkey": "Super+PageDown" },
    { "delay_ms": 400 },
    { "chord": ["Super"] },
    { "type": "lutris" },
    { "tap": "Enter" },
    { "delay_ms": 2000 },
    { "click": { "x": 380, "y": 200, "clicks": 2 } },
    { "hotkey": "Alt+F4" }
  ]
}
```

Step kinds: `tap`, `down`, `up`, `chord`, `hotkey`, `type`, `delay_ms`, `move`, `click`.

### Cursor position

`gdr_cursor` returns the last absolute position injected via `MouseMove`
(from `gdr_move` / `gdr_click`). Mutter RemoteDesktop does not expose a live
OS pointer query — `known: false` until the first gdr move.

`host` selects a **saved** profile only. Unknown names error; arbitrary IPs
via tool args are rejected by design.

Connections are pooled per profile and serialized (one in-flight request
per client) so screenshot→click loops stay on one Mutter session.

### Screenshot layout → click remap

MCP keeps per-device frame geometry from the last `gdr_screenshot`. Click/move
coords are mapped from image pixels back to Mutter stream pixels (clamped to
`[0, native-1]`). Calling `gdr_click` / `gdr_move` before any screenshot for
that device errors loudly (`no screenshot geometry…`) instead of silently
treating coords as native.

Geometry is only replaced on the next screenshot — re-screenshot after
resize, workspace switch, or monitor change. Native PNG size must match the
PipeWire buffer for the ScreenCast stream node (true by construction in
gdrd today: same keepalive appsink).

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

## Privacy / idle disconnect

Cursor keeps the `gdr-mcp` process up (health + tool listing). That does
**not** mean your screen is shared. The MCP client closes its TLS link to
gdrd after `GDR_MCP_IDLE_MS` (default `15000`). Set `0` to keep the
socket open. gdrd itself tears down physical ScreenCast after
`GDR_DISPLAY_IDLE_SECS` (default 45) with no screenshot/input.

Prefer a **single** `gdr` MCP entry (not `--per-device`) so you do not
run three idle Node processes.

## Multi-host

One MCP server process can address many saved profiles:

```
gdr_screenshot({ host: "laptop" })
gdr_screenshot({ host: "desktop" })
```

Resolution mirrors the CLI (`config.ts` ↔ `client/src/config.rs`).
