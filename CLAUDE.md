# gdr — working notes for Claude

A remote-control protocol for GNOME/Wayland meant to be driven by an agent:
screenshots, mouse, keyboard, windows and subscriptions, over its own
TLS+token protocol. Two ways in — a Rust CLI (`client/`) and a Node MCP server
(`mcp-server/`) — over one wire protocol defined in `common/`.

Deep docs live in [`docs/`](./docs/README.md) and are the source of truth;
this file is only what an agent needs before touching anything.

@.claude/rules/agent-locks.md

@.cursor/rules/gdr-device.mdc

## Shape of the thing

| Where | What |
|---|---|
| `common/` | the wire protocol — `Request`/`Response`, scopes, window types, hook types |
| `server/` (`gdrd`) | runs **on the GNOME target**, inside the graphical session |
| `client/` (`gdr`) | thin CLI, runs anywhere |
| `mcp-server/` | Node MCP server — a hand-written TypeScript mirror of `common/` |
| `shell-extension/` | GJS, runs inside gnome-shell; the only way to see windows on GNOME 50 |

**Three mirrors are maintained by hand.** Changing one side without the other
fails quietly, not loudly:

- `common/src/lib.rs` ↔ `mcp-server/src/gdrClient.ts` — a renamed field means
  gdrd drops the connection rather than returning an error.
- `shell-extension/…/extension.js` ↔ `server/src/windows.rs` — serde silently
  ignores fields it does not know.
- A new capability usually needs all of: `common/`, `server/`, a CLI command,
  an MCP tool, and a doc. `docs/CODEBASE.md` has the "where to change what"
  table.

## Build and test

```bash
cargo test --workspace                 # common + client + server
cd mcp-server && npm test              # tsc + node --test
```

If `cargo build` fails with *“The system library `gstreamer-1.0` was not
found”* while `/usr/lib/x86_64-linux-gnu/pkgconfig/gstreamer-1.0.pc` clearly
exists, a Homebrew `pkg-config` is shadowing the system one with its own
search path:

```bash
export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig
```

## Testing against a real desktop

`docs/TESTING.md` is the full story; the traps are worth repeating:

- **A stale daemon looks healthy.** `cargo test` does not refresh
  `target/release/gdrd`, and the systemd unit runs `~/.local/bin/gdrd`. Build,
  copy, `systemctl --user restart gdr.service`. Serde drops unknown request
  fields, so an old binary just quietly declines to do the new thing.
- **The binary is busy.** Copying over a running `gdrd` fails with *Text file
  busy*; `cp` to `gdrd.new` and `mv -f` over it, which leaves running
  processes on the old inode.
- **A stale MCP looks healthy too.** `e2e.mjs` runs the *installed*
  `/usr/share/gdr/mcp-server/dist/index.js`, not your build. Sync it, and
  remember the agent host caches tool schemas at connect time — new tools stay
  invisible until it reconnects.
- **The shell extension cannot be reloaded.** GNOME/Wayland scans extension
  directories only at session start, so an edit under `shell-extension/` is
  invisible to the session you are sitting in until you log out. Use
  `./scripts/hook-test-rig.sh start` instead: a nested headless gnome-shell
  with its own D-Bus session, which picks up the working tree on every start
  and where chromium windows can be opened, resized and closed without anyone
  seeing them.

## Things that are deliberate, and read like bugs

- **`settled: false` is information.** Damage is tracked per stream, not per
  region, so one blinking caret keeps every capture "busy". The frame is still
  the newest one.
- **An ambiguous window selector is an error carrying candidates**, not a
  best guess. Acting on the wrong window is worse than a message.
- **Window metadata and pixels are separate scopes.** `window` reveals titles,
  `screenshot` reveals pixels; a token can hold either alone. Hooks inherit
  the scope of what they watch.
- **An enabled activity hook holds the capture open** for as long as it is
  switched on, defeating the idle teardown on purpose — see `docs/HOOKS.md`.
  Switch it off when the watching is done.
- **Lazy display.** Listing windows must never be the thing that starts
  broadcasting the desktop.
- **A hook that reports nothing is usually not a detection bug.** The capture
  stream carries the composited screen, so a background browser tab, an
  occluded window or a locked session produces no activity at any
  sensitivity. Check the thing is being drawn before touching `threshold`.

## House style

The code here explains *why*, not *what*, and the comments are load-bearing —
they record the failures that produced the current shape (a measured 73 ms
resize, a Mutter race, a GNOME 50 API rename). Match that: comment the
decision and the evidence, not the syntax. Same for docs — every doc in
`docs/` argues for its design rather than listing its API.

Never echo tokens or sudo passwords into chat or into a commit.
