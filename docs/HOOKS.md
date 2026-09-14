# Subscription hooks

Everything else in gdr is a question: the agent asks, the daemon answers,
nothing happens in between. Hooks are the other shape — **standing
subscriptions** that gdrd keeps running on its own, and that the agent drains
when it wants to, or blocks on until something lands.

```text
gdr_hook_window({ events: ["opened","closed","resized"], app_id: "chromium" })
gdr_hook_screen({ title: "Inbox", buffer_ms: 600 })
gdr_hook_events({ wait_ms: 10000 })     → both hooks, one cursor
gdr_hooks({ action: "disable", id: "activity-2" })
```

Two kinds, because there are two genuinely different questions:

| Kind | Answers | Scope | Needs |
|---|---|---|---|
| **activity** | *pixels changed, roughly here* — reported as a circle | `screenshot` | a capture stream |
| **window** | *a window opened / closed / resized / moved / was retitled* | `window` | the `gdr-windows` extension |

The split is the same one the scopes already draw: activity is derived from
the capture stream and is (very coarse) screen content, so a token that
cannot screenshot cannot watch the screen move either. Window events are
metadata — titles, geometry, process owners — and never pixels.

---

## Buffer time, and why the event is late on purpose

Every hook takes `buffer_ms`: how long the thing being watched has to hold
still before the event is sent.

This is the whole point. A drag-resize emits a geometry change per frame; a
loading page repaints continuously; chromium's window exists at 0×0 for the
best part of a second before it is placed. Reporting each of those the instant
it is observed produces hundreds of events, and every one of them describes a
state that was already gone by the time it arrived.

So the watcher accumulates instead, and emits **one** event describing the
whole burst — where it started, where it ended, how long it ran — once the
subject has been quiet for `buffer_ms`. By construction the state in that
event has held for at least that long, which is what makes it safe to act on:
the size in a `resized` event is a size you can click against.

For something that repaints on a *timer* — a clock, a spinner, a blinking
panel — `buffer_ms` cannot help: every repaint is its own settled burst, so a
2 Hz flash is two identical events a second. `min_interval_ms` is the floor on
the reporting rate for that case, and it coalesces rather than drops — the
burst keeps accumulating while it is held back, so the event that eventually
comes out covers everything since the last one.

A burst that never goes quiet would otherwise never be reported at all, so
`max_burst_ms` (5 s by default) cuts it off and sends what it has with
`settled: false`. That flag is the one case where the report may already be
out of date — treat it as "look again", not as ground truth.

Two events skip the buffer deliberately:

- **`closed`** — there is nothing left to settle, and a buffered close would
  arrive after whatever the agent decided to do next. If a change was still
  buffering when the window went, that pending report is flushed first, so the
  history reads in the order it happened.
- **`focused` / `minimized` / `unminimized`** — discrete state flips, not
  motion.

An `opened` event additionally waits for the window to *have* a geometry: a
window the compositor is managing but has not placed yet reports a 0×0 frame,
and "chromium opened, 0×0" is true and useless.

---

## Activity: why a circle

An activity hook samples the capture stream on a cadence (`poll_ms`, 120 ms by
default), reduces each frame to a coarse luma grid — 64 cells along the long
edge, so roughly 30×30 native pixels per cell — and diffs successive grids.
Cells that moved by more than `threshold` are the change; their bounding box,
unioned across the burst, is the affected area.

That area comes back as a **circle** in capture-stream pixels:

```json
{
  "circle": { "x": 1465, "y": 612, "radius": 93, "space": "stream" },
  "bbox": { "x": 1399, "y": 546, "width": 132, "height": 132 },
  "changed_fraction": 0.013,
  "settled": true
}
```

A circle rather than a rectangle because a circle is what the measurement
supports. The grid says "something around here moved"; a rectangle with exact
edges would claim a precision the cells do not have. The radius is the box's
half-diagonal, so the circle contains everything that changed. The `bbox` is
there too, for callers that want to crop rather than aim.

Through MCP the circle also arrives in **your last screenshot's image space**,
which is the space `gdr_click` takes:

```json
"circle_in_last_screenshot": { "x": 1068, "y": 446, "radius": 68,
                               "note": "gdr_click takes these coordinates." }
```

When no screenshot has been taken for that device, or the circle falls outside
the frame that was taken, it says so instead of offering a coordinate that
would click the wrong thing.

### When it reports nothing

Check that the thing you are watching is *being drawn* before you touch
`threshold` or `grid`. This hook sees the composited screen, so anything the
compositor is not painting produces no activity at any sensitivity:

- a **background browser tab** — Chromium does not repaint an occluded tab, so
  a page animating in the background is genuinely static on screen
- a minimized or fully covered window
- a window on another workspace or another monitor

This cost two full test runs to find, both times misread as the detector being
too blunt. It is not subtle once you look: a `gdr_screenshot` shows the thing
not changing either.

The tunables that *do* matter interact, and cannot be set independently. A cell
is the **mean** brightness of its area, so the coarser the grid, the more a
small change is diluted before it is compared. At the default 64-cell grid a
cell covers about 30×30 native pixels; a few characters changing inside one
move its mean by single digits. Watching small text means raising `grid`
(128–256) and lowering `threshold` (2–4) **together**.

There is no noise floor to clear on a static desktop, whatever earlier versions
of this document said. Measured: an idle headless GNOME session at `grid: 256,
threshold: 1` produced **zero** events in 20 seconds. The stream is
damage-driven — a screen that is not repainting sends nothing at all. The floor
that does exist belongs to whatever is animating on that particular desktop,
which no default can predict.

### Following the circle: `look_here`

Every activity report carries the exact `gdr_zoom` call that looks at what it
is pointing at:

```json
"look_here": {
  "tool": "gdr_zoom",
  "args": { "space": "stream", "x": 964, "y": 694, "width": 272, "height": 172 }
}
```

It is built from the **bbox**, not the circle. The circle is a lossy
re-encoding of that rectangle — its radius is the box's half-diagonal — so
squaring it back up would hand over roughly 2.4× the area the measurement
actually covers, which for small text is resolution spent on nothing.

This exists because the obvious next step was, for a while, impossible.
`gdr_zoom` took coordinates in the last screenshot's image space and refused
to run without one — and the entire point of a hook is that you have *not*
taken a screenshot. `space: "stream"` accepts the coordinate space hooks
report in and needs no prior capture, so "the hook told me where, now let me
look" is one call.

### Watching one window

`target` scopes an activity hook to a single window, and the region is
re-derived from that window's live geometry on **every** sample — so a watch
on "the terminal" follows the terminal when it is moved, rather than watching
the patch of desktop it used to occupy. If the window moves, the grid geometry
changes, and that sample re-baselines instead of reporting the move as a
screen-sized change.

A window that is minimized, or on a monitor gdrd is not streaming, has no
pixels to watch. The hook says exactly that in `last_error` and goes to
`waiting` — it recovers on its own when the window comes back.

### Locked screens

A lock does not stop the capture stream — the lock shield is composited like
anything else, so a hook keeps reporting, and what it reports is the lock
clock ticking over once a minute. An agent that is not told this will chase
that circle, tune its hook against a shield, and conclude the tool is broken.
So every activity report carries `session_locked`, and the MCP layer turns it
into an explicit warning:

```json
"session_locked": true,
"warning": "The session is LOCKED. What is on the capture stream is the lock
            screen, not the desktop — activity here is the lock clock, not the
            app you are watching. Ask the user to unlock; ..."
```

gdrd asks the screensaver once per emitted event, not once per sample.

### The cost, which is not hidden

An enabled activity hook **holds the capture session open**. gdrd otherwise
tears ScreenCast down after 45 idle seconds precisely so the desktop is not
being broadcast around the clock; a hook that is meant to be watching would be
killed by that, so the watcher keeps it alive. Watching the screen means
streaming the screen, and on GNOME that is visible to whoever is sitting at
the machine. Toggle the hook off when you are done — `gdr_hooks({action:
"disable", id})` keeps the configuration and the buffered events, so turning
it back on costs one call.

---

## Window events: what comes with one

```jsonc
{
  "kind": "resized",
  "window": {
    "title": "gdr+hooks - Chromium",
    "size": { "width": 887, "height": 609 },      // logical pixels
    "position": { "x": 448, "y": 206 },
    "stream_region": { "x": 448, "y": 206, "width": 887, "height": 609 },
    "process": {
      "pid": 137377, "user": "eva", "uid": 1000,
      "command": "chromium",
      "exe": "/usr/lib/chromium/chromium",
      "cmdline": "/usr/lib/chromium/chromium --ozone-platform=wayland …",
      "ppid": 137373
    },
    "previous": { "frame_rect": { "width": 1024, "height": 700 },
                  "dw": -137, "dh": -91 },
    "samples": 4,
    "settled": true
  }
}
```

The process record is read from `/proc` and is **captured when the window is
first seen**, not when the event fires — so a `closed` event can still say who
owned the window, which by then is usually the only place that answer exists.
When it cannot be read the record carries an `error` saying why (`the
compositor did not report a pid for this window`, `process 4242 is gone`)
rather than quietly going missing.

Events available: `opened`, `closed`, `resized`, `moved`, `retitled`,
`focused`, `minimized`, `unminimized`, `workspace`. A name that is not on that
list is **refused at creation** — a hook asked to watch `resize` (the name
that is not `resized`) would sit there reporting nothing and look fine doing
it.

### Why it polls

The shell extension emits a `Changed` signal, and `WindowEvents` already uses
it — but it carries opens, closes and focus changes, not geometry, which is
most of what a subscription is for. The window watcher therefore re-lists
windows every `poll_ms` (250 ms by default) and diffs the listing, which gets
resize, move, retitle and workspace changes out of the extension that is
*already deployed* rather than one every target would have to install and log
out for. The cost is latency: an open is noticed within `poll_ms`, not
instantly.

---

## Draining

One journal, one sequence, shared by every hook on the daemon:

```text
gdr_hook_events({ since: 0 })                → everything buffered
gdr_hook_events({ since: 41, wait_ms: 10000 })  → blocks until something lands
gdr_hook_events({ id: "window-1", since: 41 })  → just that hook
```

Pass the reply's `next_seq` back as `since` to continue. The journal holds
1024 events; a caller that falls behind gets `dropped: true` rather than a
silently incomplete history.

**A cursor belongs to the filter that produced it.** Sequence numbers are
global across hooks — that is what lets one poll drain everything — so a
`next_seq` from a drain of one hook has walked past events belonging to the
others. Reuse it on an unfiltered poll and those are gone. Every reply
therefore says which filter its cursor is for, and how many events it stepped
over:

```json
"next_seq": 91,
"cursor_scope": "activity-4",
"skipped_other_hooks": 1,
"note": "next_seq is the cursor for hook activity-4 ONLY — 1 event(s) from other
         hooks sit inside the range it covers and are still waiting. ..."
```

This cost an agent a window-open event, which it reasonably read as a broken
subscription. The alternative — refusing to advance past events the filter
dropped — was tried and is worse: with two hooks emitting, a filtered cursor
never gets past the other hook's first event and every drain replays the whole
journal. A warned-about gap beats a livelock. **Keep one cursor per filter, or
just drain without `id`**, which is always correct.

`wait_ms` occupies the connection to that device for its duration — it is one
call instead of a spin loop, not a free background stream.

Every reply also carries the current state of the hooks the token may see, so
an empty drain can say *which* kind of empty it is:

```text
"No hooks exist yet. Create one with gdr_hook_screen or gdr_hook_window."
"Every hook is switched off. Turn one on with gdr_hooks({action:'enable', id})."
"Nothing is watching yet: activity-1 is waiting (the GNOME session is locked …)"
"Nothing happened yet. Pass wait_ms to block until something does."
```

---

## Scopes, and verifying them

A hook is checked against the connection's scopes on **every** operation, not
just at creation:

- creating an activity hook needs `screenshot`; a window hook needs `window`
- `HookList` / `HookPoll` return only hooks the token's scopes cover — a
  screenshot-only token is not told that window hooks exist
- naming a hook the token may not see is a permission error, not "no such
  hook": pretending it does not exist would send the caller off creating a
  duplicate

Each hook reports both sides of that so it can be audited after the fact:

```json
"scopes": {
  "required": "window",
  "created_with": ["screenshot", "mouse", "keyboard", "type", "window"],
  "created_by": "laptop"
}
```

`created_with` is the scope set the creating token held *at the time*, and
`created_by` its label. A subscription outlives the request that made it, so
"what is watching this desktop, and under whose authority" needs an answer
that does not depend on the token still existing. Creation, toggling and
removal are also written to the audit log with the hook's description.

---

## States

| State | Meaning |
|---|---|
| `watching` | enabled and sampling |
| `paused` | toggled off; config and buffered events kept |
| `waiting` | enabled, but there is nothing to observe **yet** — the watched window is not open, the stream has not produced a first frame. Clears on its own; nobody needs to do anything |
| `failing` | enabled, and the last attempt **errored** — the shell extension went away, ScreenCast was refused, the session is locked. Needs someone to act |

The split between the last two is the whole point of having both: a hook that
will never report again must not look like one that is simply having a quiet
minute. A healthy hook with nothing to say reads `watching`.

`waiting` is the interesting one, and it is usually self-explanatory:

```text
! the GNOME session is locked. Mutter refuses ScreenCast … (loginctl unlock-session 4)
! no open window matches that selector
! window chromium — Inbox is not on the monitor gdrd is streaming
```

A window hook cannot be created at all on a target whose shell extension is
not running: the creation fails with the install hint — and the hint
distinguishes *never installed* from *was answering and stopped*, because the
fixes are opposite and the symptom is identical. The commonest cause of the
second is the screen locking: gnome-shell unloads every extension whose
`session-modes` omits `unlock-dialog`, which this one did until the version
that added it. Being told to reinstall and log out, sixteen seconds after the
extension was working, wastes a whole trip. A subscription that is
accepted and then never fires is the worst possible way to find out the
extension was never installed.

---

## CLI

```bash
gdr hook window --events opened,closed,resized --title Chromium --buffer-ms 300
gdr hook screen --title Chromium --buffer-ms 400 --label "inside chromium"
gdr hook screen --region 0,0,960,540 --max-radius 200
gdr hooks                       # list, with state and scopes
gdr hook off activity-2         # toggle, keeping config and buffer
gdr hook on activity-2
gdr hook events --wait-ms 10000
gdr hook remove window-1
gdr --json hook events --since 12   # the full payload, for scripting
```

## Testing

Window hooks need a GNOME session whose shell has the extension loaded, and
GNOME only scans for extensions at session start — so testing a change means
logging out, unless you bring up a session of your own:

```bash
./scripts/hook-test-rig.sh start          # nested, headless GNOME + gdrd
./scripts/hook-test-rig.sh chromium       # a real window nobody can see
eval "$(./scripts/hook-test-rig.sh env)"  # point the CLI at it
HOME=$(./scripts/hook-test-rig.sh status 2>&1 | grep -o '/.*rig/home') \
  node mcp-server/e2e-hooks.mjs rig --drive
./scripts/hook-test-rig.sh stop
```

See [TESTING.md](./TESTING.md).
