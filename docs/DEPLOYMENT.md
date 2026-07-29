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

## Day-2 scripts

| Script | Purpose |
|---|---|
| `scripts/update.sh user@host [--source]` | Replace binary, restart |
| `scripts/status.sh user@host` | unit status, fingerprint, logs |
| `scripts/rotate-token.sh user@host` | new GDR_TOKEN + seed |
| `scripts/uninstall.sh user@host` | stop unit, remove binary + `~/.local/share/gdr` |

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
