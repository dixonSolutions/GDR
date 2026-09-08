# Progress log

Living checklist. Update this when a milestone lands or a decision flips.

## 2026-07-29 — complete implementation + live deploy

### Done

- [x] Workspace: `common` / `server` (`gdrd`) / `client` (`gdr`) / `mcp-server`
- [x] JSON length-prefixed TLS protocol + TypeScript mirror
- [x] Mutter RemoteDesktop + ScreenCast + PipeWire capture (GStreamer)
- [x] Interactive `deploy.sh` (apt/dnf, prebuilt vs source-on-target, MCP merge)
- [x] Shared `~/.config/gdr/config.json` — resolve order, `gdr host *`
- [x] Multi-token store (`tokens.json`, hashed, scopes, expiry, revoke)
- [x] Legacy `GDR_TOKEN` full-scope fallback + `gdrd --seed-token`
- [x] Audit log (JSON lines, 5 MiB self-rotate)
- [x] Scope enforcement on every post-auth request
- [x] CLI: `token create|list|revoke`, `audit`, `get-password`
- [x] MCP: multi-host, `gdr_get_password`, `gdr_list_hosts`, gnome_* aliases
- [x] Non-interactive deploy via `GDR_SUDO_PASSWORD` + `GDR_YES`
- [x] Unit tests: common 12, client 7, server 8 — all green
- [x] docs/ tree (architecture, protocol, tokens, MCP, deploy, security, headless)
- [x] **Live deploy** to `borys@100.118.238.2` (source build on target)
- [x] **E2E verified** (controller → target over Tailscale):
  - `ping` → pong
  - `move` / `click` / `type` → ok
  - `screenshot` → PNG returned (1×1 — host has no physical display; see HEADLESS.md)
  - `get-password sudo` → plaintext; `user` → clear “not set”
  - scoped token `screenshot` → ping ok, move **denied**
  - `token list` + `audit` over SSH admin plane

### Bugs fixed during E2E

1. zbus ProxyBuilder missing `.destination(...)` → fake “destination” parameter error  
2. RD/SC start order (RD first; don’t Start ScreenCast session when linked)  
3. Headless fallback: `RecordVirtual { is-platform, 1920×1080 }`  
4. PipeWire signal subscribe-before-start race  

### Decisions locked

| Question | Choice |
|---|---|
| Live revocation | No new connections after revoke; existing sessions drain |
| Deploy token default | `all` + never-expire (`initial-install`) |
| Audit rotation | gdrd self-rotates at 5 MiB (+ `.1` backup) |
| Password tool | `gdr_get_password` as specified; safer alt documented |
| Token admin channel | SSH only |

### 2026-07-29 (later) — act as the display

- [x] Long-lived `DisplayProvider` with `RecordVirtual { is-platform }`
- [x] PipeWire negotiation via keepalive appsink at 1920×1080
- [x] Screenshots pull from same appsink (no second pipewiresrc)
- [x] E2E on headless host: **1920×1080 PNG**, `Meta-0` logical monitor up

### 2026-07-29 — no 24/7 screen broadcast

- [x] gdrd lazy Mutter ScreenCast (on first screenshot/input)
- [x] Physical idle-stop (`--display-idle-secs` / `GDR_DISPLAY_IDLE_SECS`, default 45)
- [x] Virtual/headless Meta-* kept once started; `--eager-display` for boot
- [x] MCP TLS idle disconnect (`GDR_MCP_IDLE_MS`, default 15s)
- [x] Docs: SECURITY / HEADLESS / MCP privacy notes

### 2026-07-29 — `@gdr -dev=` agent convention

- [x] `.cursor/rules/gdr-device.mdc` — parse chat `-dev=`, status + screenshot
- [x] MCP `gdr_status`, `gdr_device_add` / `remove` / `default`

### 2026-09-08 — locked screen reads as a lock, not a permissions bug

- [x] gdrd probes `org.gnome.ScreenSaver.GetActive` + logind `LockedHint`
      on the Mutter session-creation error path
- [x] `Session creation inhibited` translated to a message naming the lock
      and the session's own `loginctl unlock-session <id>`
- [x] Same translation on `RemoteDesktop.Session.Start` (screen can lock
      between CreateSession and Start)
- [x] MCP: `screen_locked` / `retryable: false` / `remedy` on every tool
      error, via `mcp-server/src/lockHint.ts`
- [x] Tests: 5 Rust (message classification), 4 node (`lockHint.test.ts`)
- [ ] Not verified against a live lock — the host was unlocked and in use
      by another agent, so the locked branch is covered by unit tests only

### Still open / next

- [ ] Optional: `gdr_run_privileged` MCP tool
- [ ] Optional: immediate live-session kill on revoke
- [ ] Optional: xkbcommon typing; normalized 0..1 coordinates
- [ ] Optional: public git remote + clone-based install (today: rsync)
- [ ] Mutter consent dialog behavior on this GNOME 50 host when a panel is attached
- [ ] Optionally prune Cursor `--per-device` MCP entries to a single `gdr`

### Reference deployment

| Role | Machine |
|---|---|
| Target | `borys@100.118.238.2` — Debian, GNOME 50.2 Wayland, linger on |
| Controller | local `eva` — `gdr` CLI + profiles `desktop` (remote) and `local` (loopback) |
| Same-machine | `scripts/install-local.sh` + address aliases `local`/`localhost`; Cursor MCP `~/.cursor/mcp.json` → `gdr` |
| Physical display | Fixed `GetCurrentState` connector parse — local `gdrd` records primary monitor (e.g. DP-6), not only Meta virtual |
| MCP input v2 | `gdr_hotkey`, `gdr_input` (chords/sequences), `gdr_double_click` / `clicks=`, `gdr_cursor` + protocol `GetCursor` |
| Packaging | `scripts/install.sh` → apt/dnf `.deb`/`.rpm`; `update.sh` (local+remotes+Cursor), `update-mcp.sh --restart-cursor`, `setup-mcp-cursor.sh` |
| Devices + mgmt CLI | labels/aliases; MCP `--dev` / tool `dev=`; `gdr device|service|mcp|pkg` |

Secrets (token, sudo password, cert pin) live only in
`~/.config/gdr/config.json` on the controller and the target’s unit /
`tokens.json` — **never in git**.
