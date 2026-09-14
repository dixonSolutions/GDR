# Architecture

## One-sentence summary

`gdrd` runs inside a GNOME Wayland session, talks to Mutter’s private
RemoteDesktop + ScreenCast D-Bus APIs and PipeWire, and exposes a
TLS+token JSON control channel. Controllers (`gdr`, `gdr-mcp`) speak that
channel; install and token admin happen over SSH, never over the live protocol.

## Component diagram

```
┌─────────────────────────────────────────────────────────────┐
│ Controller (eva / agent host)                               │
│  ~/.config/gdr/config.json  ← remembered hosts + optional   │
│                               sudo/user passwords (chmod 600)│
│                                                             │
│  ┌──────────┐  TLS :7337 + bearer token   ┌──────────────┐ │
│  │ gdr CLI  │ ──────────────────────────► │ Target       │ │
│  └──────────┘                             │  gdrd        │ │
│  ┌──────────┐  same protocol              │  (systemd    │ │
│  │ gdr-mcp  │ ──────────────────────────► │   --user)    │ │
│  └──────────┘                             │              │ │
│  ┌──────────┐  SSH + optional sudo -S     │  Mutter RD   │ │
│  │ deploy.sh│ ──────────────────────────► │  ScreenCast  │ │
│  │ gdr token│  tokens.json / audit.log    │  PipeWire    │ │
│  │ gdr audit│                             └──────────────┘ │
│  └──────────┘                                              │
└─────────────────────────────────────────────────────────────┘
```

## Two trust boundaries (on purpose)

| Plane | Channel | Credentials | Used for |
|---|---|---|---|
| **Admin** | SSH (+ `sudo -S` when password stored) | SSH keys, optional stored sudo password | install, update, token create/revoke, audit read |
| **Data** | TCP + TLS + bearer token | token (+ cert fingerprint pin) | screenshot, mouse, keyboard, type, ping |

Piping the sudo password into the live protocol would mean every click
carries a root credential. That is deliberately never done.

`gdrd` itself never prompts and never needs sudo at runtime — it is a
`systemd --user` unit talking to the session bus.

## Runtime on the target

1. `gdrd` binds `0.0.0.0:7337` (configurable), TLS with a self-signed cert
   at `~/.local/share/gdr/{cert,key}.pem`.
2. First message on each connection must be `Auth { token }`.
3. Server hashes the token, looks up `~/.local/share/gdr/tokens.json`
   (and falls back to legacy `GDR_TOKEN` env), checks expiry/revocation,
   attaches scopes to the connection.
4. On first screenshot/input request, opens a Mutter RemoteDesktop session
   linked to a ScreenCast stream, waits for `PipeWireStreamAdded`, then
   pulls frames via GStreamer `pipewiresrc`.
   Crop, downscale and encode all happen here, in one pass over the raw
   PipeWire buffer — see [PERFORMANCE.md](PERFORMANCE.md).
5. Every auth/request is appended to `~/.local/share/gdr/audit.log`
   (JSON lines, self-rotates at 5 MiB). Window requests carry a `detail`
   naming the action and the selector, so a destructive one is attributable.

## The window plane sits beside the pixel plane

Mutter's RemoteDesktop/ScreenCast APIs deal only in pixels and input events —
they have no notion of a window list, and no way to raise one. The API that
would, `org.gnome.Shell.Introspect.GetWindows`, is refused to unprivileged
callers on GNOME 50. So window metadata comes from a **third component**: a
GNOME Shell extension running inside gnome-shell, exporting `org.gdr.Windows`
on the session bus, which gdrd talks to as a client.

```
  controller ──TLS──▶ gdrd ──D-Bus──▶ Mutter RemoteDesktop/ScreenCast   (pixels, input)
                        └────D-Bus──▶ org.gdr.Windows (shell extension)  (windows)
```

Consequences worth knowing before reading the code:

- The extension is a **separate install with its own lifecycle** — it is
  versioned apart from gdrd, so `server/src/windows.rs` parses its JSON
  tolerantly and every window request degrades to one actionable error when
  it is absent.
- The extension is deliberately a thin accessor. Selector resolution,
  coordinate conversion and pinning live in gdrd and the MCP server, where
  they are unit-testable; code inside gnome-shell can only be exercised by
  logging in.
- It reports **logical** (stage) coordinates while everything else in gdr is
  in **stream** (native) pixels. gdrd converts, once, by measuring the live
  stream rather than trusting the declared scale factor.

Full rationale: [WINDOWS.md](WINDOWS.md).

## Controller resolution

Both `gdr` and `gdr-mcp` resolve a connection as:

1. Explicit flags / env (`--addr`, `--token`, `--pin` / `GDR_*`)
2. Named `--host <profile>` / tool arg `host`
3. `default_host` in config.json
4. If exactly one profile exists, that one

An agent can pick among **saved** profiles only — it cannot invent an IP
via a tool argument. That is intentional blast-radius control.

## Why the daemon owns image sizing

`CaptureFrame` carries the sizing budget (including the vision-model patch
budget) rather than the controller resizing what it receives. Two reasons,
both non-obvious:

- **The controller cannot compute the fit.** A patch budget is
  `ceil(w/p) * ceil(h/p) <= N`, which depends on the native aspect ratio —
  the very thing it is asking the daemon for. Sending the budget down removes
  the circularity.
- **Encode once.** The old path encoded a full-resolution PNG, base64'd it
  over TLS, then decoded and re-encoded it in `sharp`. The decode alone was
  ~85 ms of undoing work that had just been done.

Getting this wrong is not a slow screenshot, it is a *wrong* one: an image
over the model's token budget gets silently downscaled by the model API, and
every coordinate it returns is then in a space the click remap knows nothing
about.

## Two things deliberately not built

Both look like omissions until you know why. Full reasoning in
[PERFORMANCE.md](PERFORMANCE.md).

- **Accessibility-tree grounding (AT-SPI Set-of-Mark).** There is no global
  coordinate space on Wayland, so GTK4 reports screen extents with the window
  at (0,0) and menus live in separate `xdg_popup` surfaces. The known
  workaround is XWayland-only. Meanwhile the grounding uplift that motivated
  Set-of-Mark has largely been absorbed by current models. AT-SPI may still
  earn a place for *verification and settle detection* — not click targets.
- **Region-diff screenshots.** Per-message crop origins are a fresh source of
  the coordinate drift this design works hard to eliminate, for savings that
  can go negative on scroll-heavy work. The `unchanged` short circuit takes
  the high-confidence part of the idea with none of the coordinate risk.

## Why JSON over the wire

Framing is `[u32 BE length][UTF-8 JSON]`. JSON (not a Rust-only binary
codec) lets `mcp-server/` reimplement the protocol in TypeScript with no
Rust dependency on the controller.
