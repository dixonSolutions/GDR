# Computer-use performance

How GDR is tuned for agent-driven desktop control: what actually costs time,
what we measured, what we changed, and what we deliberately did *not* build.

Companion to [ARCHITECTURE.md](ARCHITECTURE.md) (how it works) and
[MCP.md](MCP.md) (tool surface).

## The cost model

An agent controlling a desktop runs this loop:

```
screenshot → model inference → action → (wait) → screenshot → …
```

Wall-clock per iteration, measured on `local` (1920×1200 eDP panel):

| Stage | Cost | Share |
|---|---|---|
| gdrd capture + PNG encode + TLS | ~100 ms | ~2 % |
| MCP PNG decode + resize + re-encode | ~85 ms | ~2 % |
| **Model inference on the screenshot** | **3–10 s** | **~95 %** |

The daemon was never the bottleneck. **The loop is dominated by how many
round trips the agent needs and how many tokens each one costs.** Shaving
milliseconds off capture is close to worthless; removing a single wasted
screenshot is worth ~5 seconds.

That reframing drives every decision below. We optimise, in priority order:

1. **Correctness of grounding** — a mis-aimed click costs 2+ extra round
   trips to detect and recover. This is the most expensive failure there is.
2. **Round-trip count** — batching, act-then-observe, settle detection.
3. **Tokens per observation** — right-sized images, `unchanged` short circuit.
4. **Milliseconds** — encode-once, payload size. Last, because it barely matters.

## What went wrong before (session forensics)

Reviewed ~189 live `gdr_*` calls across seven past agent sessions. Ranked by
frequency:

| Rank | Failure | Evidence |
|---|---|---|
| 1 | Mis-aimed clicks on small targets, then screenshot-hunting to recover | One 95-action burst containing 47 screenshots; a hand-rolled "click matrix" script probing y=155…280 to find a chat row |
| 2 | Agents inventing workarounds for missing primitives | `ffmpeg` crops to zoom into the tray, PIL badge detection, raw RGB scans of the top bar |
| 3 | Stale frames / fixed `delay_ms` guesses | Repeated "didn't take — retrying" after acting on a pre-update frame |
| 4 | Capture stalls | Static desktop stops emitting damage; appsink drained then waited for a frame that never came |

Failure 1 and 2 are the same root cause: **at 1440×900 a 14 px tray icon is
10 px, and there was no way to look closer.** The agent's only recovery was
to guess again, or drop out of MCP entirely and script `ffmpeg`.

## Change 1 — Token-budget sizing (fixes silent click drift)

**This was a live correctness bug, not a tuning knob.**

Claude bills images in 28×28 patches: `⌈w/28⌉ × ⌈h/28⌉` visual tokens, capped
at 1568 for standard-tier models. Over the cap, the API **silently downscales
server-side** before the model sees the image.

The old code fit every screenshot inside a fixed 1440×900 pixel box. On this
machine's 16:10 panel that produced exactly 1440×900:

```
⌈1440/28⌉ × ⌈900/28⌉ = 52 × 33 = 1716 visual tokens   ← over the 1568 cap
```

So the model saw a *further* downscaled image while `toStreamCoords` remapped
clicks using 1440×900. Every click carried a systematic scale error. 16:9
panels squeaked under the cap (1440×810 = 1508) purely by luck of aspect
ratio, which is why this never reproduced consistently.

The fix budgets in **visual tokens instead of pixels**, and snaps both edges
to multiples of 28 so no partially-filled patch row is paid for. On this
machine's 1920×1200 panel that yields 1400×868 — 50 × 31 = 1550 tokens, using
99 % of the allowance without crossing it.

The fit runs **in gdrd, not the MCP server**, because it depends on the
native aspect ratio and no pixel box can express it. The controller would
have to know the screen size before its first capture to compute it, which is
exactly the information it is asking for. Sending the budget down removes
that circularity and keeps one implementation of the math.

Profiles are selectable per call because the vendors genuinely disagree:

| Profile | Target | Rationale |
|---|---|---|
| `claude` (default) | ≤1568 tokens, ≤1568 px long edge | Standard-tier patch cap |
| `claude-hires` | ≤4784 tokens, ≤2576 px long edge | Claude 4.7+ high-resolution tier |
| `openai` | fit 1440×900 | OpenAI CUA harness viewport; no patch cap |
| `raw` | native, 1:1 | Debugging and pixel work |

Sizing never silently lies: the returned metadata always states the exact
`image_width`/`image_height` the remap is based on.

## Change 2 — `gdr_zoom` (fixes the #1 failure mode)

Crop an arbitrary native-pixel region and return it **at full native
resolution**. A 200×120 region of the tray comes back as 200×120 real pixels
rather than 10 px of mush.

The obvious hazard is that a zoom creates a second coordinate system — the
single most likely way a feature like this introduces the very drift it is
meant to cure. Two things prevent that:

- **One rule, always.** Coordinates are read off *the most recent image for
  this device*, zoom or not. The crop origin and scale live in the frame
  geometry and `toStreamCoords` composes them, so nothing changes for the
  caller. Clicking the centre of a 400×300 zoom taken at +1000,+700 lands at
  native (1200, 850).
- **Out-of-frame coordinates are rejected, not extrapolated.** An agent that
  zooms in and then sends full-desktop coordinates gets a hard error naming
  the zoom's region and telling it to re-screenshot. Extrapolating would put
  the click somewhere plausible but wrong, which is precisely the silent
  failure this work exists to eliminate.

This replaces the `ffmpeg`-crop and click-matrix workarounds agents were
already improvising, and it fixes small-target accuracy *without* raising the
base resolution of every screenshot in the transcript. A 420×32 slice of the
GNOME panel costs 1.9 KB and 30 visual tokens.

## Change 3 — Damage-driven settle instead of fixed sleeps

Mutter negotiates its ScreenCast PipeWire stream at `framerate=0/1`, meaning
**emit-on-damage**: frames arrive when the screen changes and stop when it
doesn't. The old capture path treated the silence as a problem to work around
(drain → wait 800 ms → fall back to `last-sample`).

Silence *is* the settle signal. `settle(quiet_ms, timeout_ms)` returns as soon
as no new buffer has arrived for `quiet_ms`. No screenshots, no encoding, no
hashing — just a timer on buffer arrival.

### Why it is off by default

The first version of this made settling the default for every capture. Live
measurement killed that idea. On an ordinary working desktop — an editor and a
few terminals open — the frame changed in **11 of 11 samples taken 200 ms
apart**. There is no 120 ms quiet window to find, so every capture waited out
its entire budget and then reported `settled: false` anyway:

| capture (profile=claude) | latency | outcome |
| --- | --- | --- |
| no settle | **9 ms** | newest frame |
| settle(120 / 400) | ~420 ms | `settled: false` |
| settle(120 / 1500) | **1532 ms** | `settled: false` |

That is up to a 170x latency multiplier in exchange for nothing. Damage is
tracked per *stream*, not per region, so a single blinking terminal caret
anywhere on the desktop keeps the whole screen permanently "busy" — including a
zoom into a completely static corner. Worse, several animators beating out of
phase (two terminals, an editor caret, a clock) leave no gap at all, which is
why the earlier "a caret only damages twice a second, so there is plenty of
room" reasoning did not survive contact with a real desktop.

So the policy follows where the risk actually is:

- **`gdr_screenshot` / `gdr_zoom`: settle off.** These just look at the screen.
  The newest frame is returned either way, so waiting buys at most one frame of
  freshness (~16 ms) for up to 400 ms of latency.
- **`gdr_act`: settle on.** Here the capture deliberately races a transition the
  caller just started, which is the one case where a half-drawn window is a
  genuine risk worth paying for.

`timeout_ms` defaults to 400 ms, bounded by the Doherty threshold: past roughly
that point the loop stops feeling interactive, and on a never-quiet screen extra
patience changes nothing but the bill. Timing out is not a failure — the newest
frame is returned regardless, just flagged — which is also why `settled: false`
is worded as information rather than a warning. It fires for benign reasons
often enough that dressing it up as an error would only train agents to ignore
it.

### The cursor is in the frame

Mutter's `CursorMode` is set to `1` = **embedded**, not metadata. An earlier
draft of this document claimed the opposite; a direct experiment settled it.
Capturing a 120x120 region with the pointer inside it, then outside, then back
inside produced hashes `fd8c…` / `786c…` / `fd8c…` — the pointer is painted into
the video, and the hash returns to exactly its earlier value.

This is the right trade, because it lets an agent see where it is about to click
and verify its own aim from the screenshot alone — which past sessions relied on
to debug misses. It has two consequences worth knowing:

- Pointer motion counts as damage, so moving the mouse defeats settling.
- Pointer motion changes the frame hash, so a bare mouse move makes an otherwise
  identical screen count as changed for `skip_unchanged`.

One related fix fell out of the same damage insight: `last-sample` is now
consulted **before** any timed pull. The old order waited 800 ms for a "new"
frame that a static, damage-driven stream was never going to send, on every
single capture of an idle screen.

## Change 4 — `unchanged` short circuit

Every frame is hashed (post-crop, post-resize, geometry- and format-salted).
Pass the previous hash as `if_none_match` and an identical screen returns
**no image at all** — just `unchanged: true`.

Two wins, and the second is the bigger one:

- ~1500 visual tokens saved on a no-op step.
- The agent is *told* its action had no visible effect, instead of having to
  infer it by comparing two images it may not reliably diff.

The message prefix stays byte-identical when nothing changed, so prompt
caching keeps hitting — unlike region-cropping schemes, which change the
payload every turn and quietly invalidate the cache.

## Change 5 — Encode once

The old path encoded a full-resolution PNG in gdrd, base64'd 205 KB across
TLS, then decoded and re-encoded it in `sharp`. The decode alone was ~85 ms of
pure waste — undoing work that had just been done.

gdrd now crops, resizes and encodes **once**, from the raw RGBA frame it
already holds. `sharp` is gone from the dependency list entirely — nothing in
the MCP server touches pixels any more.

Alpha is dropped during the crop: ScreenCast frames are opaque, and three
channels halve the work for both the resizer and the encoder.

### The resizer was the real bottleneck

Switching the resize into Rust made it *slower* at first — 150–200 ms per
capture, more than everything else combined. `image::imageops::resize` has no
vectorised path, and the filter choice barely matters because the cost is in
the scalar inner loop:

| Resizer (1920×1200 → 1400×868) | Time |
|---|---|
| `image` Nearest | 82 ms |
| `image` Triangle | 81 ms |
| `image` CatmullRom | 73 ms |
| `image` Lanczos3 | 127 ms |
| **`fast_image_resize` CatmullRom (SIMD)** | **11 ms** |
| `fast_image_resize` Lanczos3 | 12 ms |

Hence the `fast_image_resize` dependency. CatmullRom keeps UI text sharp
without the edge ringing Lanczos3 leaves behind for JPEG to spend bits on.

Format is JPEG q85 by default. Token cost is **identical** — billing is on
decoded pixel dimensions, so anyone claiming JPEG saves tokens is wrong about
the mechanism. The win is payload and encode time. PNG stays available for
`raw` layout and debug captures, where lossless matters and latency doesn't.

We use 4:4:4 chroma subsampling. It costs ~15 % more bytes and removes the
colour bleed around sharp text edges that 4:2:0 introduces — text legibility
is the whole point of a screenshot.

WebP was measured and **rejected**: 31 KB (best-in-class) but 231 ms to
encode, a 2.3× latency regression against JPEG for bytes we don't need.

## Change 6 — `gdr_act` (batch + settle + observe)

One round trip for a sequence of actions plus the screenshot that shows the
result, replacing N round trips.

The known hazard is compounding error: if step 2 depends on visual state that
step 1 changed, and step 1 misses, the rest of the batch runs on false
assumptions and the agent drifts without ever seeing real state. Mitigations:

- **Fail-fast with partial progress.** On error, return `completed: n`,
  `failed_at`, the reason, *and* a screenshot of the actual state at the point
  of divergence. The agent sees where reality diverged rather than a blind
  failure.
- **`expect_change`** per step asserts via frame hash that an action actually
  did something, converting a silent miss into a reported error.
- **Settle between steps**, so step 2 acts on a finished UI.
- The single-action tools stay. Batching is for self-contained sequences
  (form fills, keyboard chains, click-a-known-target); exploratory navigation
  and error recovery should still observe between steps.

**Security:** scopes are validated **per sub-action** on the daemon side, not
once for the batch. A `screenshot`-scoped token cannot smuggle a click through
`gdr_act`, and every sub-action is audit-logged individually. Batching is a
transport optimisation and must never become a privilege escalation.

## Change 7 — recovering from a dead capture stream

Found while testing the rest of this: the first screenshot after an idle
teardown failed outright a large fraction of the time, taking 9.2 s to do it.
Two of three attempts failed in one run. This matches the "capture pipeline
stall" failures in earlier sessions.

The cause is a startup race, not slowness. Mutter sometimes hands out a PipeWire
node that negotiates caps correctly and then never produces a buffer. The
distribution is bimodal and tells the whole story: a healthy stream delivers its
first frame in 28–368 ms, and an unhealthy one delivers nothing, ever. There is
no middle. So waiting longer cannot help — the old code waited 3 s at startup
and another 5.8 s at capture time, and still failed.

Three things follow:

- **Preroll is a health check, not a wait.** `PREROLL_WAIT` is 1500 ms — roughly
  4x headroom over the slowest healthy start observed — and its result is
  recorded as `prerolled`.
- **Recovery replaces the whole session.** Reattaching to the same node does not
  revive it, and dropping the last consumer can invalidate the node outright.
  An early attempt to rebuild just the GStreamer pipeline turned a clear "no
  frame" error into a confusing `keepalive Playing` failure — a worse bug than
  the one it was fixing. `DisplayProvider::start` now tears the Mutter session
  down and opens a new one, which yields a fresh node.
- **The retry is fault-injected in tests.** The race cannot be provoked on
  demand, so `GDR_FAULT_PREROLL=n` reports the next *n* stream startups as dead.
  Without a seam this recovery path would ship untested — and untested recovery
  code is usually broken recovery code, as the pipeline-rebuild attempt above
  demonstrated.

Verified both ways: with the fault injected, the daemon logs `capture stream came
up dead — restarting Mutter session`, completes recovery in ~90 ms, and returns a
frame in 775 ms. Without it, five consecutive cold starts across idle teardowns
succeeded at a 731 ms median (max 753 ms), against 9.2 s failures before.

One caveat on that second number: the retry path did not fire during those five
runs. A daemon restart had moved the stream to a fresh node, and the healthy node
never needed recovery. So the five-for-five result shows the cold path is fast
when healthy; it is the injected-fault test, not that run, that demonstrates the
recovery actually works.

## Deliberately not built

Recorded because each looks like an obvious omission until you know why.

### Accessibility-tree grounding (AT-SPI Set-of-Mark)

Tempting: enumerate clickable elements with bounding boxes, overlay numbered
marks, let the model pick a label instead of a pixel.

Rejected on three independent grounds:

1. **It doesn't work on GNOME Wayland.** There is no global coordinate space,
   so toolkits report `GetExtents(screen)` with the window at (0,0). GTK4 is
   broken this way today; the known workaround uses X11 `translate_coordinates`
   and therefore only works under XWayland. GTK popovers and menus are separate
   `xdg_popup` surfaces with their own origins — and menus are exactly what you
   most want to click. GNOME's Newton protocol exists to fix this and is still
   prototype-stage; by design it exposes no screen coordinates at all.
2. **The uplift has evaporated.** Set-of-Mark was a GPT-4V-era result whose
   authors noted it didn't generalise beyond that model. Current
   state-of-the-art OSWorld results are explicitly screenshot-only, no
   accessibility tree, no set-of-marks. Anthropic tested grid overlays and
   image tiling internally and reported no gain from either.
3. **It's slow.** Querying a full UIA/AT-SPI tree takes seconds on complex
   screens — the same order as the model inference we're trying to save.

Building it in 2026 would be optimising for a 2024 model against a broken
coordinate API.

Where AT-SPI *would* earn its place is verification and settle detection —
subtree hashing to confirm a click changed something, reading focus state that
is genuinely hard to see in pixels. That's a much smaller surface, and the
damage-based settle in Change 3 already covers most of it. Left as future work.

### Region-diff screenshots

Sending only changed crops saves 5–33 % on scroll-heavy work and can go
*negative* (two PNGs exceeding one), while introducing per-message crop origins
— a fresh source of exactly the coordinate drift Change 1 just fixed. The
`unchanged` short circuit in Change 4 captures the high-confidence part of the
idea with none of the coordinate risk.

### Lower JPEG quality

q85 is a practitioner default, not a measured threshold — we found no published
benchmark isolating JPEG quality against GUI-agent click accuracy. Going lower
trades legibility of small text for bytes we've established don't matter. If
this is ever revisited it should be measured, not guessed.

## Measured results

Screenshot round trip on `local` (1920×1200 eDP), mean of 10, measured
through the MCP server's own client so the numbers include TLS and framing.

| Path | Latency | Payload | Visual tokens |
|---|---|---|---|
| Before: native PNG + `sharp` resize | ~185 ms | 206 KB b64 | 1716 — **over cap** |
| Legacy `Screenshot` (native PNG, no resize) | 11 ms | 485 KB | 2967 — **over cap** |
| `profile=claude`, JPEG q85, 1400×868 | 26 ms | 177 KB | 1550 |
| `profile=openai`, JPEG q85, 1440×900 | 32 ms | 187 KB | 1716 |
| `profile=claude-hires`, JPEG q85, native | 39 ms | 300 KB | 2967 |
| `gdr_zoom` 400×300 native crop, PNG | 42 ms | 3 KB | 165 |

The headline is the token column, not the latency one: the default path costs
**48 % fewer visual tokens than the legacy capture and lands under the 1568
cap**, which is what stops the model API from silently downscaling and offsetting
every click. Latency rises from 11 ms to 26 ms because we now actually resize —
an irrelevant trade against a ~5 s inference step.

Through the real MCP server over stdio (`e2e.mjs`), the same paths land at 16–69
ms per call, all inside the Doherty threshold.

Payload varies with screen content — a text-dense IDE compresses far worse
than a flat desktop — so treat the ratio, not the absolute KB, as the result.

Two honest caveats from the same measurements:

- **`skip_unchanged` rarely fires on a working desktop.** With something always
  animating, consecutive frames differ, so the short circuit misses. It pays off
  on quiet screens and on zooms into static regions, not on a busy one.
- **Settling costs 1532 ms and achieves nothing on a busy desktop**, which is why
  it is off by default outside `gdr_act`. See Change 3.

Round trips for "click a small target and confirm it worked":

| | Before | After |
|---|---|---|
| Typical | 4–6: screenshot, click, screenshot, re-aim, click, screenshot | 2: `gdr_zoom` to locate, `gdr_act` to click + settle + observe |

At ~5 s of model inference per round trip, that is the change that actually
matters. Everything else in this document is rounding error by comparison.

### Verifying after a change

Two scripts in `mcp-server/`, both pointed at device `local`:

```bash
node bench.mjs   # latency/payload/token table per capture path
node e2e.mjs     # drives the real MCP server over stdio against the desktop
```

`e2e.mjs` is the one that matters: it exercises the sizing profiles, zoom,
the out-of-frame click rejection, the `unchanged` short circuit, and
`gdr_act` success *and* failure reporting, all against the live desktop.

**Redeploy gotcha:** the systemd unit runs `~/.local/bin/gdrd`, and
`cargo test` does **not** refresh `target/release/gdrd`. Always
`cargo build --release` before copying, or you will test a stale daemon
that silently ignores unknown request fields — serde drops them, so a
too-old binary looks like a working one that just declines to downscale.

## Tuning reference

| Setting | Default | Notes |
|---|---|---|
| `GDR_MCP_IDLE_MS` | 15000 | MCP→gdrd TLS idle close |
| `GDR_DISPLAY_IDLE_SECS` | 45 | gdrd ScreenCast teardown |
| `profile` (tool arg) | `claude` | Sizing target; see Change 1 |
| `format` / `quality` | `jpeg` / 85 | `png` for lossless |
| `settle.quiet_ms` | 120 | Damage silence before "settled" |
| `settle.timeout_ms` | 600 | Cap for never-quiet screens; see Change 3 |

## Guidance for harness authors

Client-side, so not enforceable here, but it dominates long-session cost:

- **Prune screenshots in batches, not one per turn.** Dropping one image per
  turn changes the prompt prefix every turn and continuously invalidates the
  cache. Keep the last ~3, prune every ~25 turns.
- **Put instruction text before the image** in the content array; image-first
  ordering measurably degrades click accuracy.
- **Thinking effort `high`** for computer use; `max` costs more with no
  accuracy gain, because UI tasks are perceptual rather than deeply logical.
