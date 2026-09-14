# Computer-use benchmarks

`PERFORMANCE.md` measures how fast the stack can hand an agent a screenshot.
This measures something harder and more useful: whether an agent can actually
*get anything done* with it.

The two answers turned out to be very different, and the second one reorders the
priorities in the first.

## The suite

Five self-scoring task pages in `bench/tasks/`, driven through the real MCP tool
surface against a real GNOME desktop.

| Task | What it tests | Scored on |
| --- | --- | --- |
| `shapes` | Aim on targets from 26 to 80 px, mixed with already-red decoys | Clicks used against the board's minimum |
| `cycle` | Tracking state through a four-colour cycle | Overshoots past the target colour |
| `quiz` | Small body text; answers exist only in an invented article | Correct answers out of five |
| `slides` | Keyboard paging plus small monospace codes | Codes transcribed correctly |
| `news` | Scrolling, fact gathering, composing and submitting | Required facts present in the summary |

Three design choices matter:

- **The page scores itself** and posts the verdict to the harness. An agent is
  never asked whether it succeeded, because self-reported success is exactly the
  thing a benchmark cannot afford to trust.
- **Boards are seeded**, so two runs face an identical task. Otherwise a
  comparison measures which board was easier.
- **The content is invented.** The quiz article describes a city that does not
  exist, so world knowledge cannot substitute for reading the screen.

## Running it

```bash
node bench/server.mjs                                    # harness: tasks, scoring, tool bridge
node bench/runner.mjs open shapes --toolset full --seed 11
#   … agent drives the desktop via bench/gdrb.mjs …
node bench/runner.mjs end
node bench/analyse.mjs
```

`bench/AGENT_BRIEF.md` is what the driving agent reads. The harness can withhold
tools (`--toolset basic`) to compare the full surface against a
screenshot-and-click-only baseline.

Every tool call is recorded with its latency, payload and visual-token cost into
`bench/results/`, alongside the score the page awarded. That is what makes cost
and outcome comparable afterwards.

## Result: aim is the ceiling

The `shapes` task, same board, three ways:

| Run | Finished | Clicks | Hit | Miss | Accuracy | Round trips | Visual tokens |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Manual, one click by hand | — | 1 | 1 | 0 | 100 % | 3 | 1,702 |
| Composer 2.5, pixel analysis | yes | 28 | 13 | 15 | 46 % | 23 | 10,998 |
| Composer 2.5, vision only | no | 44 | 2 | 42 | **4.5 %** | 56 | 38,358 |

**42 of 44 clicks missed.** Not a coordinate bug: a hand-placed click lands
pixel-perfect, the remap has unit tests, and the daemon reports exactly where
each click went. The failure is the model turning "that purple circle" into a
pixel coordinate.

This reframes the whole performance effort. Sizing screenshots to the token
budget, cutting capture latency from 650 ms to 9 ms, batching actions — all real,
all worth having, and all downstream of a step that succeeds 4.5 % of the time.

Three details sharpen it:

- **The misses are biased, not scattered.** Vision-only clicks landed between
  y=8 and y=376 on a board 620 px tall — nothing at all in the bottom 40 %. A
  separate run independently reported aiming about 35 px too high. The model
  compresses vertical position toward the top, most plausibly by not accounting
  for the browser chrome and page header above the content.
- **Zoom made targets legible without making them locatable.** The vision-only
  run zoomed 8 times and still missed. Zoom answers *what is this*; it does not
  answer *where is it in the full frame*, and moving between the two coordinate
  spaces was itself reported as confusing.
- **Offered a way out, the agent stopped looking.** Unprompted, the first run
  wrote code to flood-fill a raw PNG and compute shape centroids, taking accuracy
  from 4.5 % to 46 %. Rational, and a warning: an agent will route around a weak
  grounding path rather than persevere with it. `AGENT_BRIEF.md` now forbids it,
  or the suite would be benchmarking image processing.

## What this changed

Two capability gaps surfaced while building the suite, both fixed:

- **`gdr_scroll`.** The daemon has supported `MouseScroll` on the wire since
  forever, but no MCP tool exposed it — an agent driving through MCP could not
  scroll a page at all. Added the tool and a `scroll` step for `gdr_act`, with
  optional pointer positioning, since the compositor delivers wheel events to
  whatever sits under the cursor.
- **`profile=compact`.** Caps images at 1024 px on the long edge. Image viewers
  silently resize anything larger, so a 1400 px screenshot reached the agent as
  1024 px while the click space stayed 1400 — every coordinate read off it would
  be wrong by 37 %. This is the same silent-downscale trap fixed on the server
  side in `PERFORMANCE.md`, arriving from the client side instead.

## Open: coordinate grounding

`PERFORMANCE.md` records a decision *not* to build accessibility-tree grounding:
broken on GNOME Wayland, published uplift evaporated, slow. The measurement above
does not overturn those objections — it overturns the priority. A 4.5 % hit rate
caps everything else in the stack.

Worth testing next, cheapest first, all measurable with this harness:

1. **A coordinate ruler or crosshair drawn into the capture.** Anthropic reported
   no gain from grid overlays, but that was tested against models sizing their
   own screenshots, not against a measured vertical bias like this one.
2. **Confirm before committing.** `gdr_move` then a cheap zoom around the pointer
   costs far less than a wrong click plus the screenshot that discovers it. The
   cursor is drawn into the frame, so this is already observable.
3. **Report the pointer's position back** after every move, so the agent can
   close the loop on its own aim without a full capture.

Each is a small change with a number attached to it afterwards, which is the
point of having the harness.

## Caveats

- One task of five has been run. `cycle`, `quiz`, `slides` and `news` exist and
  are self-testing, but have no agent results yet.
- One model (`composer-2.5-fast`), one board, no repeats. The 4.5 % versus 46 %
  gap is far too large to be noise, but the exact figures are not stable
  estimates.
- The `full` versus `basic` toolset comparison the harness supports has not been
  run, so the round-trip saving from `gdr_act` and `gdr_zoom` remains measured
  only at the capture level, not at the task level.
