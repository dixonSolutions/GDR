# gdr

A minimal remote-control protocol for GNOME/Wayland, primarily meant to be
driven by an AI agent: screenshots + mouse + keyboard input, over its own
TLS+token protocol, deployed via SSH+sudo. Two ways in: a terminal CLI, or
an MCP server for agent hosts (Claude Desktop/Code, Cursor, etc.).

**Deep docs** (architecture, protocol, tokens, MCP, deploy, progress): see [`docs/`](./docs/README.md).

## Who installs what — asymmetric, like SSH

- **Target** (the GNOME desktop) runs `server/` → `gdrd`. Must be Linux+GNOME on Wayland.
- **Controller** (agent host) only needs a thin client:
  - `client/` → `gdr` Rust CLI, or
  - `mcp-server/` → Node MCP server (no Rust required on the controller).

## Quick start

```bash
# On the controller, with SSH to the target:
GDR_YES=1 GDR_INSTALL_METHOD=2 \
  GDR_SUDO_PASSWORD='…' GDR_PROFILE_NAME=desktop GDR_SAVE_SUDO=1 \
  ./deploy.sh user@host full

cargo build --release -p gdr
./target/release/gdr --host desktop ping
./target/release/gdr --host desktop screenshot
```

Interactive menu (no env vars): `./deploy.sh user@host`

## Remembered devices

After deploy (or `gdr device add`), both CLI and MCP read
`~/.config/gdr/config.json` (chmod 600) — per-device token, pin, optional sudo.

```bash
gdr device add home --address … --token "$T" --label "home computer" --ask-sudo
gdr --dev "home computer" ping
gdr screenshot                    # default_host
gdr get-password sudo --host home
gdr service status                # systemd --user gdrd
gdr mcp setup-cursor --dev "home computer" --per-device
gdr pkg info
```

See [docs/CONFIG.md](./docs/CONFIG.md) and [docs/TOKENS.md](./docs/TOKENS.md).

## Two trust boundaries

| Plane | Channel | For |
|---|---|---|
| Admin | SSH (+ optional stored sudo via `sudo -S`) | install, tokens, audit |
| Data | TLS + bearer token | screenshot / mouse / keyboard |

`gdrd` never prompts at runtime. Token mint/revoke stays on SSH so a leaked
scoped token cannot escalate. Details: [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md),
[docs/SECURITY.md](./docs/SECURITY.md).

## MCP

```bash
cd mcp-server && npm install && npm run build
```

Point your MCP host at `mcp-server/dist/index.js`. Prefer the no-env form so
secrets stay in `~/.config/gdr/config.json`. Tools include `gdr_screenshot`,
`gdr_click`, … and `gdr_get_password`. See [docs/MCP.md](./docs/MCP.md).

## Build

**Target** (for `gdrd`):

```bash
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  gstreamer1.0-plugins-good gstreamer1.0-pipewire libdbus-1-dev pkg-config
cargo build --release -p gdrd
```

**Controller** (for `gdr`): `cargo build --release -p gdr` — no GStreamer needed.

## System package + Cursor MCP

```bash
# Full apt/dnf package (gdr + gdrd + gdr-mcp); --host enables systemd daemon
GDR_YES=1 GDR_SUDO_PASSWORD='…' ./scripts/install.sh --host --mcp-cursor

# After code changes
./scripts/update-package.sh
./scripts/update-mcp.sh --restart-cursor

# Coding-tool MCP only (Cursor global ~/.cursor/mcp.json)
./scripts/setup-mcp-cursor.sh
```

See [docs/DEPLOYMENT.md](./docs/DEPLOYMENT.md).

## Day-2 ops

| Task | Command |
|---|---|
| Install system package | `./scripts/install.sh [--host] [--mcp-cursor]` |
| Update package (code changed) | `./scripts/update-package.sh` |
| Update MCP (+ optional Cursor restart) | `./scripts/update-mcp.sh --restart-cursor` |
| Cursor MCP setup | `./scripts/setup-mcp-cursor.sh` |
| Update remote binary | `./scripts/update.sh user@host --source` |
| Status | `./scripts/status.sh user@host` |
| Rotate token | `./scripts/rotate-token.sh user@host` |
| Uninstall remote | `./scripts/uninstall.sh user@host` |

## Progress & known edges

Tracked in [docs/PROGRESS.md](./docs/PROGRESS.md). Highlights still open:
Mutter consent dialogs, ASCII-only typing, no live video yet, private Mutter
API compatibility across GNOME versions.
