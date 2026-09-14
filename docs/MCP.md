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
| `gdr_screenshot` / `gnome_screenshot` | `host?`, `profile?`, `settle?`, `skip_unchanged?`, `layout?` | Sizes for the model's token budget, returns geometry JSON + image. `settle` off by default |
| `gdr_zoom` | `host?`, `x`, `y`, `width`, `height`, `settle?` | Crop at **native** resolution. Use for anything under ~20px |
| `gdr_act` | `host?`, `steps[]`, `expect_change?`, `profile?`, `settle?`, `screenshot?` | Actions **and** the resulting screenshot in one round trip. Settles by default |
| `gdr_click` / `gnome_click` | `host?`, `x`, `y`, `button?`, `clicks?` | `x,y` in latest screenshot image space; remapped to stream pixels. Requires a prior screenshot. `clicks=2` = double-click |
| `gdr_double_click` | `host?`, `x`, `y`, `button?` | shorthand |
| `gdr_move` / `gnome_move` | `host?`, `x`, `y` | same image-space remap as click; updates tracked cursor |
| `gdr_cursor` | `host?` | last known `{x,y,known}` from gdr moves |
| `gdr_key` / `gnome_key` | `host?`, `key`/`keycode`, `modifiers?` | names or evdev; mods held for tap |
| `gdr_hotkey` | `host?`, `keys` | `"Alt+F4"`, `"Super+PageDown"` |
| `gdr_input` | `host?`, `steps[]` | flexible ordered sequence, no screenshot (see below) |
| `gdr_type` / `gnome_type` | `host?`, `text` | ASCII MVP |
| `gdr_ping` / `gnome_ping` | `host?` | |
| `gdr_status` | `host?` / `dev?` | Resolve device + Ping; `auth: valid\|failed` (no secrets) |
| `gdr_list_devices` / `gdr_list_hosts` | — | id, label, aliases, flags (no secrets) |
| `gdr_device_add` | `id`, `token?`, `local?` / `address?`, … | Add/update `~/.config/gdr/config.json` |
| `gdr_device_remove` | `dev` | Remove profile by id/label/alias |
| `gdr_device_default` | `dev` | Set `default_host` |
| `gdr_get_password` | `host?`/`dev?`, `kind: sudo\|user` | see below |

### Window tools

These need the `gdr-windows` GNOME Shell extension on the target and the
`window` token scope — see [WINDOWS.md](./WINDOWS.md), which also explains why
an extension is unavoidable on GNOME 50 and why a window screenshot activates
the window first.

Every tool below takes the same selector (`id`, `app_id`, `wm_class`, `title`,
`pid`, `focused`), combined with AND. With no selector they fall back to the
device's **pinned window**, then to whatever has focus, and the result says
which (`source: "explicit" | "pin" | "focused"`).

| Tool | Key args | Notes |
|---|---|---|
| `gdr_windows` | `filter?`, `include_skip_taskbar?`, `log?` | Start here. Lists windows, monitors, the captured connector, and the event `seq` |
| `gdr_window_info` | selector | Full state of one window; use it to check a pin still resolves |
| `gdr_window_control` | selector, `action`, `x/y/width/height/index?` | activate, minimize, maximize, move, resize, workspace, close |
| `gdr_window_screenshot` | selector, `activate?`, `profile?` | Crops to one window. Activates first by default — Wayland has no per-window buffer |
| `gdr_window_act` | selector, `steps[]`, `activate?` | Activate + drive + capture the window, in one round trip |
| `gdr_window_events` | `since`, `limit?`, `wait_ms?`, `log?` | Open/close/focus journal. `wait_ms>0` blocks instead of polling |
| `gdr_window_pin` | selector, `clear?`, `label?`, `note?` | Set / show / wipe the per-device default window |
| `gdr_window_log` | `tail?`, `kind?`, `since?` | Controller-side log of pins, actions and observed events |
| `gdr_app_launch` | `app_id?`, `list?`, `filter?` | Launch or raise an app — how to reach a window that is not open at all |

A selector matching several windows is an error carrying the candidates, not
an arbitrary pick.

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

### Driving the desktop efficiently

The loop is dominated by model inference (~5 s per round trip), not by gdrd
(~50 ms). Optimise for **fewer observations**, not faster ones. See
[PERFORMANCE.md](PERFORMANCE.md) for the full reasoning.

1. **`gdr_act` instead of act-then-screenshot.** One call runs a sequence and
   returns the resulting screenshot. Best for self-contained sequences (form
   fills, keyboard chains, clicking a known target). For exploratory
   navigation, keep observing between steps.
2. **`gdr_zoom` for small targets.** Below ~20px, a full screenshot does not
   have the pixels to aim with. Zoom rather than guessing and retrying — the
   guess-and-retry loop is the single most expensive failure mode there is.
3. **Don't add `delay_ms` before capturing.** Screenshots wait for the
   compositor to stop repainting by default, which is both faster and more
   reliable than a guessed delay.
4. **`skip_unchanged: true`** when you only need to know whether something
   happened. An identical screen returns a short note and no image.

```json
{
  "steps": [
    { "hotkey": "Super" },
    { "type": "text editor" },
    { "tap": "Enter" }
  ],
  "expect_change": true
}
```

### Sizing profiles

`profile` picks the sizing target; `layout` is the older, coarser knob
(`agent` → `claude`, `raw` → `raw`).

| Profile | Fit | Use when |
|---|---|---|
| `claude` (default) | ≤1568 visual tokens | Claude Sonnet 4.6 / standard tier |
| `claude-hires` | ≤4784 visual tokens | Claude 4.7+ high-resolution tier |
| `openai` | 1440×900 | OpenAI computer-use models |
| `raw` | native, 1:1 | Pixel work, debugging |

Sizing over a model's token budget makes its API silently downscale the image
*again*, after which every coordinate it returns is in a space the remap
knows nothing about. `visual_tokens` in the returned metadata is the billed
cost, so it is easy to confirm you are under the cap.

### Frame geometry → click remap

MCP keeps per-device geometry from the last capture. Click/move coords are
mapped from image pixels back to Mutter stream pixels, clamped to
`[0, native-1]`.

**One rule: coordinates are always read off the most recent image for that
device.** That holds for zooms too — the crop origin and scale are folded
into the remap, so clicking the centre of a 400×300 zoom taken at +1000,+700
lands at native (1200, 850). Nothing special to track.

Two failures are made loud rather than silent:

- Clicking before any screenshot → `no screenshot geometry…`.
- Coordinates outside the last image (classically: zooming in, then sending
  full-desktop coords) → an error naming the zoom region. Extrapolating would
  click somewhere plausible but wrong.

Geometry is only replaced on the next capture — re-screenshot after a resize,
workspace switch, monitor change, or after a zoom if you want to click
elsewhere.

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

## Locked screen

Mutter refuses ScreenCast and RemoteDesktop to unprivileged clients while
the lock shield is up. It reports this as a bare
`org.freedesktop.DBus.Error.Failed: Session creation inhibited`, which names
neither the lock nor a fix, so it reads like a permissions bug — it is not,
and no portal or policy change reaches it.

gdrd probes `org.gnome.ScreenSaver.GetActive` and logind's `LockedHint` on
that error path and returns a message naming the lock and the exact
`loginctl unlock-session <id>` for the session. MCP tools tag the payload
`screen_locked: true`, `retryable: false`, plus a `remedy` string:

```json
{ "error": "RemoteDesktop.CreateSession: the GNOME session is locked. …",
  "screen_locked": true, "retryable": false, "remedy": "…" }
```

Ask the user to unlock rather than retrying. Nothing needs restarting —
gdrd opens the display lazily, so the next screenshot or input succeeds.

## Multi-host

One MCP server process can address many saved profiles:

```
gdr_screenshot({ host: "laptop" })
gdr_screenshot({ host: "desktop" })
```

Resolution mirrors the CLI (`config.ts` ↔ `client/src/config.rs`).
