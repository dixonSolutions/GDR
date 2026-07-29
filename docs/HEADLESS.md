# Headless / no-monitor targets

## Goal

When the GNOME session has **no physical monitor**, gdrd should **act as
the display**: create a platform virtual monitor (`Meta-*`) the session
actually uses, negotiate a real resolution over PipeWire, and capture that.

## How it works (implemented)

By default gdrd starts **lazy** (no ScreenCast until the first
screenshot/input). On headless hosts use `--eager-display` (or
`GDR_EAGER_DISPLAY=1`) so Meta-* exists at boot.

When the display session starts, `DisplayProvider`:

1. Opens Mutter RemoteDesktop + ScreenCast (linked session).
2. If no connector is usable → `RecordVirtual { is-platform: true, width, height, cursor-mode }`.
3. Starts a long-lived GStreamer consumer:
   `pipewiresrc ! videoconvert ! appsink(caps=RGBA,1920x1080)`.
4. Mutter sizes the virtual monitor from that **PipeWire negotiation**
   (not from the D-Bus width/height hints alone).
5. Screenshots pull frames from the **same appsink** (a second
   `pipewiresrc` on the same node will stall).

Platform virtual monitors are **not** idle-stopped (removing them would
kill the session's only display). Physical monitors idle-stop after
`GDR_DISPLAY_IDLE_SECS` (default 45).

Logs you want to see:

```
no physical monitor — acting as display via platform virtual monitor, negotiating 1920x1080
keepalive negotiated 1920x1080 on pipewire node …
display provider ready (virtual=true, node=…, 1920x1080)
```

`DisplayConfig` should then show `Meta-0` with one logical monitor while
the display session is active.

## Verified (2026-07-29, `borys@100.118.238.2`)

- Host: all DRM connectors disconnected, GNOME 50.2 Wayland.
- After deploy: `Meta-0` present, `logical=1`.
- `gdr --host desktop screenshot` → **1920×1080 PNG** (~178 KiB).
- `move` / `click` / `ping` still ok on the virtual stream.

## Tunables

```bash
gdrd --width 1920 --height 1080   # default
gdrd --connector eDP-1            # force a physical connector when present
gdrd --eager-display              # open Mutter at boot (recommended headless)
gdrd --display-idle-secs 0        # never idle-stop once started
gdrd --no-display                 # control plane only (no Mutter session)
```

## Failure modes

| Symptom | Likely cause |
|---|---|
| 1×1 PNG | Consumer accepted placeholder size (old code); upgrade gdrd |
| `no frame from keepalive appsink` | Pipeline not playing / caps mismatch |
| `display provider not available` | Mutter open/negotiate failed at boot — check journal |
| Meta-0 disappears when gdrd stops | Expected — the virtual monitor is owned by gdrd’s session |
