# The window plane

Everything else in gdr deals in **pixels**: a composited screenshot and
synthetic input. The window plane deals in **windows** — what is open, where
it is, which workspace it lives on, and how to bring one to the front so the
pixel plane can see it.

- Enumerate windows, with geometry in both coordinate spaces.
- Act on a specific window: activate, minimize, maximize, move, resize, send
  to another workspace, close.
- Reach an app that is not open at all, by launching it.
- Poll for windows opening and closing — or subscribe, and be told instead
  (with geometry, and the owning process): [HOOKS.md](./HOOKS.md).
- Pin one window as the default target, so you stop repeating the selector.

---

## Why this needs a GNOME Shell extension

Short version: **install `shell-extension/gdr-windows@…` and log out once.**

Mutter's `RemoteDesktop` and `ScreenCast` APIs — the ones gdrd already speaks
— hand over a video stream and inject input. They have no concept of a window
list, and no way to raise a window.

The API that does is `org.gnome.Shell.Introspect.GetWindows`. On GNOME 50 it
answers:

```console
$ gdbus call --session -d org.gnome.Shell.Introspect \
    -o /org/gnome/Shell/Introspect -m org.gnome.Shell.Introspect.GetWindows
Error: GDBus.Error:org.freedesktop.DBus.Error.AccessDenied: GetWindows is not allowed
```

Only the desktop portal is allowed to call it, and the `org.gnome.shell
introspect` GSetting that used to open it up no longer exists on GNOME 50.
There is no X11 escape hatch either — `xdotool` and `wmctrl` are blind on
Wayland — and `org.gnome.Shell.Eval` has been disabled in release builds since
GNOME 41.

That leaves running code *inside* gnome-shell, which is what an extension is.
`shell-extension/gdr-windows@gdr.dixonsolutions.github.io` is a thin
accessor: it exports `org.gdr.Windows` on the session bus with `List`, `Act`,
`Launch`, `ListApps` and `Events`, and nothing else. It reads no window
*contents* — pixels still go through ScreenCast, with its own token scope.

### Install

```bash
./scripts/install-window-extension.sh                 # this machine
./scripts/install-window-extension.sh user@host       # over SSH
./scripts/install-window-extension.sh --dev desktop   # a configured device
```

Then **log out and back in.** This is not optional and not a bug in the
script: on Wayland gnome-shell scans the extension directories only at session
start, and cannot be restarted in place. Until then `gnome-extensions enable`
reports *“Extension does not exist”* — the shell has genuinely never seen the
directory. gdrd says the same thing, with the same fix, on every window
request:

```
the gdr-windows GNOME Shell extension is not running on the target.
Install it with `scripts/install-window-extension.sh`, then log out and back
in — GNOME/Wayland only scans for new extensions at session start, so
enabling it is not enough.
```

Verify after logging back in:

```bash
gdbus call --session -d org.gdr.Windows -o /org/gdr/Windows -m org.gdr.Windows.List | head -c 300
gdr --host <device> windows
```

Uninstall with `./scripts/install-window-extension.sh --uninstall`.

---

## Two coordinate spaces

This is the part that silently produces clicks tens of pixels off, so it is
worth being explicit.

| Space | Who uses it | On a 1920×1200 panel at 125% |
|---|---|---|
| **logical** (stage) | the compositor, `frame_rect`, `gdr_window_control` move/resize | 1536 × 960 |
| **stream** (native) | `Region`, `MouseMove`, every existing gdr coordinate | 1920 × 1200 |

`WindowInfo` carries both: `frame_rect` is logical, `stream_region` is stream
and can be handed straight to `CaptureFrame` as a crop. gdrd does the
conversion in `server/src/windows.rs`, once, and derives the factor by
**measuring** — `stream width ÷ monitor logical width` — rather than trusting
the compositor's declared scale. That is what makes fractional scaling come
out right instead of nearly right.

`stream_region` is `null` when the window is on a monitor gdrd is not
streaming. That is a real answer, not missing data: a crop computed against
the wrong monitor would return a confidently wrong image.

---

## Viewing a window that is not on screen

Wayland composites. The capture stream carries *what is on the screen*, not a
per-window buffer, so a minimized window, one on another workspace, or one
buried behind others simply is not in the pixels.

So "screenshot this window" means "put it on screen, then crop". That is why
`gdr_window_screenshot` and `gdr_window_act` activate the window first by
default, and why the result always reports `activated: true|false` — this is
visible to whoever is sitting at the machine.

```text
gdr_windows                                    → find it
gdr_window_screenshot({ title: "Inbox" })      → activate + crop + capture
gdr_window_act({ title: "Inbox", steps: […] }) → activate + drive + capture
```

`activate: false` captures without disturbing anything, but then a window that
is not already visible cannot be captured at all, and you get an error saying
which of the three reasons applied.

For an app that is **not running at all**, `gdr_app_launch` starts it (or
raises it if it turns out to be running already):

```text
gdr_app_launch({ list: true, filter: "text" })     → discover app ids
gdr_app_launch({ app_id: "org.gnome.TextEditor" }) → launch or raise
```

---

## Selecting a window

Every window tool takes the same selector. Fields combine with **AND**:

| field | matching |
|---|---|
| `id` | exact. Fastest, but ids die with the window |
| `app_id` | exact, case-insensitive, `.desktop` suffix optional |
| `wm_class` | substring, case-insensitive |
| `title` | substring, case-insensitive |
| `pid` | exact |
| `focused` | whatever has keyboard focus |

Two or more matches is an **error carrying the candidate list**, not a coin
flip — the one exception being a focused match, which is a deliberate
tie-break. Acting on the wrong window is much worse than one more round trip:

```json
{
  "error": "2 windows match app_id=firefox.desktop — pass id=, or narrow the selector.",
  "candidates": [{"id": 2, "title": "gdr — Mozilla Firefox"}, {"id": 3, "title": "docs — …"}]
}
```

---

## Pinning

Driving one app across a long session means repeating the same selector on
every call, and every repetition is a chance to typo it onto someone else's
editor. A pin says *"unless told otherwise, this window"* once.

```text
gdr_window_pin({ title: "Inbox", note: "the mail window I'm driving" })
gdr_window_screenshot({})            → uses the pin, reports source: "pin"
gdr_window_control({ action: "maximize" })
gdr_window_screenshot({ id: 91 })    → explicit selector overrides the pin
gdr_window_pin({})                   → show the pin and what it resolves to
gdr_window_pin({ clear: true })      → wipe it
```

**Where it lives.** In `~/.config/gdr/config.json` beside the device it
belongs to — so it is per-device (pinning an editor on the desktop must not
aim at the laptop), survives restarts, and is visible to both front-ends. Both
`gdr device add` and `gdr_device_add` rebuild the whole profile, so both
explicitly carry the pin through; there are tests for exactly that, because a
device rename silently unpinning your window is a nasty thing to debug.

**What is stored.** The window id *and* its app/title:

```json
"pinned_window": {
  "id": 91,
  "app_id": "org.gnome.TextEditor.desktop",
  "title": "notes",
  "label": "notes editor",
  "note": "the editor I'm driving",
  "pinned_at": "2026-09-07T10:00:00.000Z",
  "pinned_to": { "id": 91, "title": "notes", "app_id": "org.gnome.TextEditor.desktop" }
}
```

The id is the exact fast path while the window lives. When the app restarts
and the id dies, the app/title selector still finds it — and the recorded id
is rewritten in place, so the pin heals itself instead of degrading. `pid` is
deliberately never a fallback: it gets recycled, and a stale one can match a
completely unrelated process.

Pinning **resolves before storing**. A selector that does not currently name
exactly one window is an error at pin time, not a surprise on the next action.

---

## Watching windows open and close

The extension keeps a 512-entry journal of `opened` / `closed` / `focused` /
`minimized` / `unminimized`, each with a monotonic `seq`. gdrd proxies it.

```text
gdr_windows                                  → note `seq`
gdr_window_events({ since: seq, wait_ms: 5000 })
gdr_window_events({ since: next_seq, … })    → continue without gaps
```

`wait_ms > 0` blocks until something happens (gdrd waits on the extension's
`Changed` signal), so watching costs one call rather than a spin loop. It does
occupy that device's connection for the duration — the wire protocol is one
in-flight request per connection.

Two honest failure reports rather than silent gaps:

- `dropped: true` — you polled later than 512 events ago and lost some.
- `reset: true` — gnome-shell or the extension restarted, so the sequence
  began again and pre-restart events are gone.

`closed` events carry the title and app of the window that closed, snapshotted
when it was created: by the time the compositor fires `unmanaged` the window
is already torn down and every getter returns null, which would make every
close event indistinguishable from every other.

---

## Logging

Two logs, on the two sides of the connection:

- **Target** — gdrd's existing audit log (`~/.local/share/gdr/audit.log`) gains
  a `detail` field on window requests, naming the action and the selector, so
  "what closed my editor?" has an answer:
  `{"event":"request","request":"WindowAction","detail":"close target[title~notes] destructive"}`
- **Controller** — `~/.local/share/gdr/window-log.jsonl` records pin changes,
  window actions you performed, and window events observed by
  `gdr_window_events` (on by default). Read it with `gdr_window_log`. This is
  the one that can answer *"what was open at 14:30"* after the windows are
  gone — window ids and titles do not survive the window.

---

## Scope

Window requests need the `window` token scope, separate from `screenshot`:
this plane reveals **titles**, not pixels, and a token can hold either alone.
Tokens minted as `all` — which is every token `deploy.sh` creates — pick it up
automatically. A token minted with an explicit scope list before this existed
needs `window` added:

```bash
gdr token create --host desktop --scopes screenshot,mouse,keyboard,type,window --label agent
```

Closing a window is destructive, but it is not a *new* capability: any token
with `keyboard` could already send `Alt+F4`. Folding window control under one
scope reflects that, rather than implying a boundary that is not there.

---

## Tools and commands

| MCP tool | CLI | What |
|---|---|---|
| `gdr_windows` | `gdr windows [--filter …] [--all]` | List windows, monitors, capture connector |
| `gdr_window_info` | — | Resolve one window (pin-aware) and report its state |
| `gdr_window_control` | `gdr window <action> [--id …]` | activate / minimize / maximize / move / resize / workspace / close |
| `gdr_window_screenshot` | — | Activate + capture just that window |
| `gdr_window_act` | — | Activate + run input steps + capture the window |
| `gdr_window_events` | `gdr window-events [--since N] [--wait-ms …]` | Poll open/close/focus |
| `gdr_window_pin` | — | Set / show / clear the per-device default window |
| `gdr_window_log` | — | Read the controller-side window log |
| `gdr_app_launch` | `gdr app [app_id] [--filter …]` | List installed apps, launch or raise one |

---

## Testing

```bash
cargo test                                   # selector, scope, coordinate mapping
cd mcp-server && npm test                    # selection, pin store, log
cd mcp-server && node e2e-windows.mjs <dev>  # live, over real MCP stdio
```

`e2e-windows.mjs` passes in both states and says which it saw: with the
extension active it lists, pins, captures and acts for real; without it, it
asserts that every window tool fails with the install hint and nothing else —
which is the state every target is in before its first logout, and therefore
worth testing rather than skipping.
