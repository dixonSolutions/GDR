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

## MCP build

```bash
cd mcp-server && npm install && npm run build
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
