# Testing

## Unit tests (no remote host)

```bash
# From repo root — common + client always; server unit tests that avoid gst
cargo test -p gdr-common
cargo test -p gdr --lib   # if lib tests; else:
cargo test -p gdr
cargo test -p gdrd --lib  # tokens/audit/tls modules
# Full workspace (server binary link needs gstreamer dev packages):
cargo test --workspace
```

Covered today:

- Framing roundtrip + oversized frame rejection  
- Keymap ASCII / shift  
- Scope parse (`all`, subsets, unknown)  
- Config resolve order, password messages, chmod 600  
- Token hash/create/auth/revoke/expiry + legacy env token  
- Audit JSON-lines write  
- TLS generate → reload same fingerprint  
- Expires parser (`never` / `30d`)  
- Capture geometry: region clamping (and rejection of off-screen origins),
  aspect-preserving fit, patch/token budget across aspect ratios, row-stride
  and BGRA handling, frame hashing  

## MCP build + tests

```bash
cd mcp-server && npm install && npm test
```

Covers sizing profiles, the visual-token budget, coordinate remapping
(including zoom crops and out-of-frame rejection), frame-state tracking,
window selection (ambiguity, pin fallback across an app restart), and the pin
store — the last of which redirects `$HOME` to a temp dir, so it never touches
your real `~/.config/gdr/config.json`.

## Capture performance + live MCP exercise

Both target device `local` and need a running `gdrd`:

```bash
cd mcp-server
npm run bench   # latency / payload / visual-token table per capture path
npm run e2e     # drives the real MCP server over stdio against the desktop

# Window plane, over real MCP stdio. Passes in both states and says which:
#   extension active  — lists, pins, captures and acts for real
#   extension absent  — asserts every window tool fails with the install hint
node e2e-windows.mjs local
```

### Subscription hooks

Window hooks need a GNOME session whose shell has the `gdr-windows` extension
*loaded*, and GNOME/Wayland scans for extensions only at session start. So a
change under `shell-extension/` — or anything that depends on it — cannot be
tested on the desktop you are sitting in front of without logging out. Bring
up a session of your own instead:

```bash
./scripts/hook-test-rig.sh start      # nested headless gnome-shell + gdrd on :7339
./scripts/hook-test-rig.sh status     # prints the HOME and the eval line

# the whole hook surface, against real chromium windows nobody can see
HOME=<rig>/home node mcp-server/e2e-hooks.mjs rig --drive

./scripts/hook-test-rig.sh stop       # takes the session, gdrd and chromium with it
```

The rig is a `gnome-shell --headless --virtual-monitor 1920x1080` on its own
D-Bus session, with the extension copied out of the working tree — so it picks
up your edits on every start. Chromium launched into it (`hook-test-rig.sh
chromium`) is a real managed window on a real compositor, which is what makes
open/resize/close events worth asserting.

Against any other device the same script runs the safe half — create, toggle,
reconfigure, drain, remove — and touches no windows:

```bash
node mcp-server/e2e-hooks.mjs local
```

On a target without the shell extension it asserts that a window hook is
*refused at creation* with the install hint, and runs the lifecycle against an
activity hook instead. A subscription that is accepted and then silently never
fires is the failure worth guarding against.

`e2e.mjs` is the useful one after any change to capture or coordinates: it
exercises every sizing profile, zoom, the out-of-frame click rejection, the
`unchanged` short circuit, and `gdr_act` on both success and failure.

> **Redeploy gotchas — both sides.** Neither script runs from your working
> tree, and both fail silently rather than loudly.
>
> - **Daemon.** The systemd unit runs `~/.local/bin/gdrd`, and `cargo test` does
>   *not* refresh `target/release/gdrd`. Always `cargo build --release` before
>   copying, then `systemctl --user restart gdr.service`. A stale daemon looks
>   healthy: serde drops request fields it doesn't know, so an old binary just
>   quietly declines to downscale.
> - **MCP.** `e2e.mjs` runs `/usr/share/gdr/mcp-server/dist/index.js`, the
>   *installed* copy — `npm run build` alone changes nothing it sees. Sync with
>   `GDR_SUDO_PASSWORD='…' GDR_YES=1 ./scripts/update-mcp.sh --system`. This one
>   cost real time: a settle-default change measured as having no effect at all,
>   because e2e was still exercising the previous build.
> - **Cursor's own MCP connection.** Cursor caches tool schemas at connect time,
>   so new or changed tools stay invisible until it reconnects. `--restart-mcp`
>   kills the process Cursor owns, which drops the live connection without
>   re-establishing it — the tools then report *Not connected* until you reload
>   from Cursor (Command Palette → MCP: Restart, or Reload Window). Prefer
>   syncing with `--system` alone and reloading from Cursor when convenient;
>   `e2e.mjs` verifies the same code path without touching the IDE session.

### Forcing the dead-stream recovery path

Mutter occasionally hands out a PipeWire node that never produces frames, and
the race cannot be provoked on demand. `GDR_FAULT_PREROLL=n` reports the next
*n* stream startups as dead so the session-restart recovery is actually
exercised:

```bash
systemctl --user stop gdr.service
GDR_FAULT_PREROLL=1 RUST_LOG=info XDG_RUNTIME_DIR=/run/user/$(id -u) \
  ./target/release/gdrd --bind 127.0.0.1:7337
```

Expect `capture stream came up dead — restarting Mutter session` in the log,
followed by a successful capture. Unset, the variable defaults to 0 and the
seam is inert.

### Checking scope enforcement

Security is enforced per request on the daemon, so batching through `gdr_act`
cannot escalate privilege. To confirm, create a `screenshot`-only token and send
input primitives directly — each must come back denied:

```
CaptureFrame  → allowed
MouseMove     → permission denied: token lacks scope 'mouse'
MouseButton   → permission denied: token lacks scope 'mouse'
KeyEvent      → permission denied: token lacks scope 'keyboard'
TypeText      → permission denied: token lacks scope 'type'
```

## End-to-end against a real GNOME host

Requires: SSH to target, graphical Wayland session for that user, port 7337
reachable from the controller.

```bash
# 1) Deploy (source build on target is the portable path)
GDR_YES=1 GDR_INSTALL_METHOD=2 \
  GDR_SUDO_PASSWORD='…' GDR_PROFILE_NAME=desktop GDR_SAVE_SUDO=1 \
  ./deploy.sh borys@HOST full

# 2) From controller — build CLI
cargo build --release -p gdr

# 3) Smoke
./target/release/gdr --host desktop ping
./target/release/gdr --host desktop --json screenshot -o /tmp/gdr-e2e.png

# 4) Token scopes
./target/release/gdr token create desktop --label e2e-shot --scope screenshot --expires 1h
# (use printed token once)
./target/release/gdr --addr HOST:7337 --token NEW --pin PIN --json ping
# mouse should fail with permission denied if scope is screenshot-only:
./target/release/gdr --addr HOST:7337 --token NEW --pin PIN move 10 10

# 5) Password tool messaging
./target/release/gdr get-password sudo --host desktop
./target/release/gdr get-password user --host desktop   # expect "not set" if unset

# 6) Audit
./target/release/gdr audit desktop --lines 20
```

## Known E2E pitfalls

1. **Consent dialog** — some GNOME versions prompt for RemoteDesktop/
   ScreenCast even for session-bus callers; headless hang if nobody clicks Allow.  
2. **Wrong session bus** — `gdrd` must see the graphical user’s
   `XDG_RUNTIME_DIR` / session bus (linger + user unit handles this).  
3. **RecordVirtual vs real monitor** — we try DisplayConfig connectors first;
   fallback is RecordVirtual (may not match the visible desktop).  
4. **Firewall** — ensure 7337/tcp is open on the Tailscale/LAN path.  
5. **Fingerprint mismatch** after cert regen — update `pin` in config.json.
