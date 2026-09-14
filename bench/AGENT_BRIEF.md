# Driving the desktop for a benchmark run

You are controlling a real GNOME desktop. A browser window is already open on
your task. Everything you do goes through one command.

## The only tool you need

```bash
cd /home/eva/Projects/SideProjects/GDR
node bench/gdrb.mjs <command>
```

| Command | What it does |
| --- | --- |
| `shot` | Capture the screen. Prints an image path — **read that image file to see the desktop**. |
| `zoom <x> <y> <w> <h>` | Native-resolution crop, for anything too small to read. |
| `click <x> <y>` | Click. Add `--clicks 2` to double-click. |
| `move <x> <y>` | Move the pointer without clicking. |
| `scroll <dy>` | Wheel scroll; positive scrolls down. `--at <x> <y>` points first. |
| `key <Key>` | Tap a key: `Return`, `Escape`, `Tab`, `Page_Down`, `Right`, `Left`. |
| `hotkey <Combo>` | e.g. `Ctrl+a`. |
| `type <text>` | Type literal text. |
| `act '<json>'` | Batch several steps, then screenshot. |
| `status` | Your running totals so far. |

## Coordinates

**Read coordinates straight off the screenshot image.** It is saved at the same
size as the click space (1024 wide), so a button at (296, 207) in the picture is
`click 296 207`. Do not rescale anything.

Every command prints JSON. After `shot`, open the file at `"image"` to look at
the screen.

## Batching with `act`

`act` runs several steps and then screenshots, in one go. Steps:

```bash
node bench/gdrb.mjs act '[{"click":{"x":296,"y":207}},{"click":{"x":415,"y":337}}]'
node bench/gdrb.mjs act '[{"tap":"Page_Down"},{"delay_ms":200},{"tap":"Page_Down"}]'
node bench/gdrb.mjs act '[{"click":{"x":500,"y":600}},{"type":"some text"},{"tap":"Return"}]'
```

Use it when you already know the next few actions from the current screenshot —
that is the whole saving. Do not batch steps whose coordinates depend on what
the earlier steps will reveal, because the later ones would be aiming at a
screen you have not seen.

## What counts as playing fairly

This measures how well an agent drives a desktop **by looking at it**. So:

- Decide where to click by **looking at the screenshot image**. Do not write
  scripts that decode the image and compute pixel positions, and do not read the
  page's HTML, source or the harness's log files. Solving it by image processing
  measures a different skill and makes the run incomparable to the others.
- `bench/gdrb.mjs` is the only way to touch the desktop. No `xdotool`, no
  browser devtools, no editing files.

## How to work well

- **Look before you act, but do not look twice for the same information.** Every
  screenshot is a full round trip. Plan several actions from one picture where
  the picture already tells you what you need.
- **Zoom instead of guessing.** If text or a target is too small to be sure
  about, `zoom` into it. One zoom is far cheaper than a wrong click plus the
  screenshot that discovers the mistake.
- **Check the header.** Each task shows its own progress (for example
  `6 / 18 red`). That is ground truth — use it instead of guessing whether an
  action worked.
- **Stop when the page says the task is complete.** A green banner appears. Once
  you see it, stop: further actions only cost round trips.
- **If a click misses, do not repeat it blindly.** Take a screenshot, look at
  where the pointer actually is (it is drawn into the picture), and correct.

## When you are finished

Say so, and report in your final message:

1. Whether the task reached its completion banner.
2. Roughly how many round trips you used (`node bench/gdrb.mjs status` tells you).
3. Anything about the tooling that slowed you down or misled you.

Do not run `bench/runner.mjs` — starting and ending runs is handled for you.
