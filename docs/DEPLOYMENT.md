# Deployment

## Prerequisites

**Target (GNOME/Wayland machine)**

- Linux with GNOME on Wayland (Mutter RemoteDesktop + ScreenCast)
- SSH access with sudo
- For source builds: `cargo`, or allow `deploy.sh` to install rustup
- Packages (apt example): `gstreamer1.0-pipewire`, plugins-good/base,
  and for builds: `libgstreamer1.0-dev`, `libgstreamer-plugins-base1.0-dev`,
  `libdbus-1-dev`, `pkg-config`, `build-essential`

**Controller**

- Rust (for `gdr` CLI) and/or Node 20+ (for `gdr-mcp`)
- SSH client
- Network path to target:7337 (Tailscale, LAN, etc.)

## Interactive entrypoint

```bash
./deploy.sh                         # ask target + menu
./deploy.sh user@host               # menu
./deploy.sh user@host shell         # ssh -t only
./deploy.sh user@host install       # install path
./deploy.sh user@host mcp           # local MCP merge
./deploy.sh user@host full          # install + profile + MCP offer
```

Menu:

1. Connect only  
2. Install gdrd (prebuilt **or** build-from-source on target)  
3. MCP setup  
4. Full setup  

## Non-interactive / agent-friendly

```bash
export GDR_YES=1
export GDR_INSTALL_METHOD=2          # source build on target
export GDR_SUDO_PASSWORD='...'       # piped to sudo -S, not argv
export GDR_PROFILE_NAME=desktop
export GDR_SAVE_SUDO=1
export GDR_SKIP_MCP=1                # optional
./deploy.sh borys@100.118.238.2 full
```

`sudo -S` reads the password from stdin on the remote side so it does not
appear in `ps`. First bootstrap still conceptually needs a password
*somewhere*; after it is stored in config.json, later admin ops can reuse it.

## What install does

1. Detect apt vs dnf  
2. Install runtime (+ build) packages  
3. Copy binary **or** rsync tree + `cargo build --release -p gdrd`  
4. `loginctl enable-linger`  
5. Write `~/.config/systemd/user/gdr.service`  
6. `gdrd --seed-token` → `tokens.json`  
7. `systemctl --user enable --now gdr.service`  
8. Print token + cert fingerprint; optionally save controller profile  

## System package install (apt / dnf)

Preferred on a machine you develop on or permanently control. Builds from the
current tree, produces a real `.deb` or `.rpm`, and installs via the distro
package manager (`dpkg` / `dnf`). Ships **full CLI** (`gdr`), **daemon**
(`gdrd`), **MCP** (`gdr-mcp` + `/usr/share/gdr/mcp-server`), and a systemd
**user** unit template.

```bash
# Controller + binaries + MCP package
GDR_YES=1 GDR_SUDO_PASSWORD='…' ./scripts/install.sh

# Also enable host daemon (systemd --user) + local profile + Cursor MCP
GDR_YES=1 GDR_SUDO_PASSWORD='…' ./scripts/install.sh --host --mcp-cursor
```

| Flag / env | Meaning |
|---|---|
| `--host` / `--daemon` | `systemctl --user enable --now gdr.service`, seed token, write profile |
| `--mcp-cursor` | merge `gdr` into `~/.cursor/mcp.json` |
| `GDR_BIND` | daemon bind (default `0.0.0.0:7337`) |
| `GDR_PROFILE_NAME` | profile name when `--host` (default `local`) |
| `GDR_SKIP_DEPS=1` | skip apt/dnf dependency install |

Packages land in `dist/packages/` (e.g. `gdr_0.1.0_amd64.deb`).

## Same-machine (user-local, no .deb)

Lightweight alternative without packaging:

```bash
./scripts/install-local.sh
# → binds 127.0.0.1:7337, seeds token, writes profile `local`
gdr --host local ping
```

## Coding-tool MCP (Cursor global)

```bash
./scripts/setup-mcp-cursor.sh           # /usr/bin/gdr-mcp or repo dist
./scripts/setup-mcp-cursor.sh --system  # force packaged gdr-mcp
./scripts/setup-mcp-cursor.sh --repo --restart
```

Multi-host tokens stay in `~/.config/gdr/config.json`; tools take `host=`.

## Day-2 scripts

| Script | Purpose |
|---|---|
| `scripts/install.sh` | Build + install system `.deb`/`.rpm` (full stack) |
| `scripts/update.sh` | Unified updater: local package, optional remotes from `config.json`, Cursor MCP restart; or one `user@host` |
| `scripts/update-mcp.sh [--restart-cursor]` | Rebuild MCP only; optional AI-host MCP restart |
| `scripts/setup-mcp-cursor.sh` | Cursor global `~/.cursor/mcp.json` |
| `scripts/install-local.sh` | User-local gdrd only (loopback + `local` profile) |
| `scripts/install-window-extension.sh [user@host\|--dev id]` | GNOME Shell extension for the window plane. **Requires a log out / log back in afterwards** |
| `scripts/status.sh user@host` | unit status, fingerprint, logs |
| `scripts/rotate-token.sh user@host` | new GDR_TOKEN + seed |
| `scripts/uninstall.sh user@host` | stop unit, remove binary + `~/.local/share/gdr` |

After local code changes:

```bash
./scripts/update.sh                            # interactive: machines + git pull + Cursor
./scripts/update.sh --yes                      # all machines + git pull + Cursor
./scripts/update.sh --yes --local-only         # this machine only
./scripts/update.sh --yes --machines local,desktop
./scripts/update.sh --yes --no-git-pull
./scripts/update.sh --yes --no-restart-cursor
./scripts/update.sh user@host                  # one remote (package / binary fallback)
./scripts/update.sh user@host --source         # rsync + cargo build on target
./scripts/update-mcp.sh --restart-cursor       # MCP-only rebuild
```

Interactive select mode: type a machine number to toggle, `s` to list
selection status, `f` to finish and continue.

Git sync (`fetch`, then `reset --hard` upstream **only when behind**) runs on
each selected machine when the worktree is clean. Skipped when there are
uncommitted changes, unpushed commits (ahead), or a diverged history — so
local work is never discarded. Remote paths tried: `~/SideProjects/GDR`,
`~/Projects/SideProjects/GDR`, `~/gdr-src` (override with `GDR_SRC`).

`update-package.sh` remains as a thin deprecated shim → `update.sh`.

Remotes come from `~/.config/gdr/config.json` (unique `ssh` targets; localhost
skipped). Matching distro packages install over SSH (stored `sudo_password`
when set); mismatched remotes get a binary fallback + user-unit restart.

SSH login prefers keys (`BatchMode`). If publickey fails, `update.sh` tries
`user_password` from the host profile, then `GDR_SSH_PASSWORD`, then prompts
on the TTY (cached for the rest of the run). Password auth needs `sshpass`.

## Window plane (one extra step, once per target)

Screenshots and input work with nothing beyond `gdrd`. Enumerating windows or
activating one does not: GNOME 50 refuses
`org.gnome.Shell.Introspect.GetWindows` to unprivileged callers and Mutter's
screencast API only knows about pixels, so a small GNOME Shell extension has
to run inside the session.

```bash
./scripts/install-window-extension.sh --dev desktop
# then, on the target: log out and back in
gdr --host desktop windows
```

The log-out is a hard requirement, not caution: on Wayland gnome-shell scans
the extension directories only at session start and cannot be restarted in
place, so until then `gnome-extensions enable` reports that the extension does
not exist. Full rationale in [WINDOWS.md](./WINDOWS.md).

Existing tokens minted with `--scope all` already cover the `window` scope.
Tokens with an explicit scope list need it added.

## Cert rotation

Rare. On the target:

```bash
systemctl --user stop gdr.service
rm -f ~/.local/share/gdr/cert.pem ~/.local/share/gdr/key.pem
systemctl --user start gdr.service
# re-pin clients with the new fingerprint from status.sh / journal
```

## Reference target used in development

- Host: `borys@100.118.238.2` (Tailscale)
- OS: Debian, GNOME Wayland session active
- Controller: this workstation (`eva`)

Do **not** commit real tokens, sudo passwords, or `config.json` with secrets.
See SECURITY.md.
