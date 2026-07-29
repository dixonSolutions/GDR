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
5. Every auth/request is appended to `~/.local/share/gdr/audit.log`
   (JSON lines, self-rotates at 5 MiB).

## Controller resolution

Both `gdr` and `gdr-mcp` resolve a connection as:

1. Explicit flags / env (`--addr`, `--token`, `--pin` / `GDR_*`)
2. Named `--host <profile>` / tool arg `host`
3. `default_host` in config.json
4. If exactly one profile exists, that one

An agent can pick among **saved** profiles only — it cannot invent an IP
via a tool argument. That is intentional blast-radius control.

## Why JSON over the wire

Framing is `[u32 BE length][UTF-8 JSON]`. JSON (not a Rust-only binary
codec) lets `mcp-server/` reimplement the protocol in TypeScript with no
Rust dependency on the controller.
