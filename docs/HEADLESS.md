# Headless / no-monitor targets

## What we saw on `borys@100.118.238.2` (2026-07-29)

- GNOME Shell 50.2, Wayland session active, linger enabled.
- **All DRM connectors disconnected** (`card0-DP-*` / `HDMI-A-*` = disconnected).
- `org.gnome.Mutter.DisplayConfig.GetCurrentState` returns **zero** monitors
  until a platform virtual monitor is created.
- Physical `RecordMonitor("eDP-1"|…)` → `Unknown monitor`.

## How gdrd adapts

When no connector is usable, `MutterSession::open` falls back to:

```
RecordVirtual({ is-platform: true, width: 1920, height: 1080, cursor-mode: 1 })
```

That is the same headless approach gnome-remote-desktop uses: Mutter creates
a `Meta-N` “Virtual remote monitor”. Input injection (move/click/type) works
against that stream.

## Capture quality caveat

On a machine with **no real framebuffer** (no physical panel and no prior
virtual monitor already driving a desktop), the PipeWire node can still
negotiate and deliver frames that are effectively **1×1**. The protocol,
auth, scopes, and PNG pipeline are fine — there is simply nothing useful to
photograph until a display exists.

### Make screenshots useful

1. **Plug in a monitor** (or enable a dock/display), or  
2. Drive a persistent virtual monitor (e.g. run gnome-remote-desktop headless,
   or a dummy DRM driver), then point gdrd at that connector (`--connector Meta-0`
   / `eDP-1` / …).

After a real connector appears, `deploy`/`gdrd` will prefer `RecordMonitor`
automatically.

## Related bugs we fixed while debugging this

1. zbus `ProxyBuilder` required `.destination(...)` when binding session/
   stream object paths — missing it produced  
   `Parameter \`destination\` was not specified but it is required`.
2. Start order with a linked RD+SC session: **RemoteDesktop.Start first**;
   do not call `ScreenCast.Session.Start` (Mutter: “Must be started from
   remote desktop session”). Stream often auto-starts with RD.
