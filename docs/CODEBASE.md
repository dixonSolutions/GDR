# Codebase map

```
GDR/
├── Cargo.toml              workspace (common, server, client)
├── README.md               quick start + design summary
├── deploy.sh               interactive install / MCP / shell entrypoint
├── scripts/                update, status, rotate-token, uninstall
├── docs/                   ← you are here
├── common/                 gdr-common — protocol, framing, scopes, keymap
├── server/                 gdrd — daemon for the GNOME target
├── client/                 gdr — controller CLI
└── mcp-server/             gdr-mcp — Node/TypeScript MCP tools
```

## Crates

### `common` (`gdr-common`)

| File | Role |
|---|---|
| `src/lib.rs` | `Request` / `Response`, framing, keymap, tests |
| `src/scopes.rs` | `Scope`, `ScopeSet`, parse `"all"` / `"screenshot,mouse"` |

No GStreamer/D-Bus. Builds on any host. Heavy unit-test coverage for the wire format.

### `server` (`gdrd`)

| File | Role |
|---|---|
| `src/main.rs` | bind TLS, auth, scope checks, request dispatch |
| `src/tokens.rs` | hashed multi-token store + legacy `GDR_TOKEN` |
| `src/audit.rs` | JSON-lines audit log + size rotation |
| `src/tls.rs` | self-signed cert load/generate + fingerprint |
| `src/mutter_dbus.rs` | zbus proxies for Mutter RD / ScreenCast |
| `src/capture.rs` | GStreamer `pipewiresrc` → PNG |

Must build **on the target** (or a matching Linux userspace) because of
GStreamer/PipeWire/D-Bus native deps.

### `client` (`gdr`)

| File | Role |
|---|---|
| `src/main.rs` | clap CLI: control cmds + host/token/audit/password |
| `src/config.rs` | load/save/resolve `~/.config/gdr/config.json` |
| `src/pinning_verifier.rs` | rustls cert fingerprint pin (TOFU) |
| `src/remote_admin.rs` | SSH helpers for token create/list/revoke + audit |

Builds anywhere with a Rust toolchain (no GStreamer).

### `mcp-server` (`gdr-mcp`)

| File | Role |
|---|---|
| `src/index.ts` | MCP tool registration |
| `src/gdrClient.ts` | TLS client + framing (TS mirror of common) |
| `src/config.ts` | shared config.json reader + password messages |

## Where to change what

| Want to… | Touch |
|---|---|
| Add a new mouse/key action | `common` Request/Response + server `handle_request` + MCP tool + PROTOCOL.md |
| Change auth / scopes | `common/scopes.rs`, `server/tokens.rs`, `server/main.rs` |
| Change remembered hosts | `client/config.rs`, `mcp-server/src/config.ts`, CONFIG.md |
| Change install flow | `deploy.sh`, DEPLOYMENT.md |
| Change Mutter integration | `server/mutter_dbus.rs` (re-introspect after GNOME upgrades) |

## Tests layout

- `common`: framing roundtrip, keymap, scope parse, JSON shape guards
- `server`: token create/auth/revoke/expiry, audit write, TLS fingerprint persistence
- `client`: config resolve order, password messages, chmod 600, expires parse

Run: `cargo test --workspace` (server native tests that need GStreamer only
run `gst::init` in the binary path; unit tests in tokens/audit/tls do not).
