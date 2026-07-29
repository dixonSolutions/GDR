# gdr documentation

This folder is the living design + ops record for **gdr** (GNOME Desktop Remote).
The root [`README.md`](../README.md) is the quick-start; everything here goes deeper.

| Doc | What it covers |
|---|---|
| [ARCHITECTURE.md](./ARCHITECTURE.md) | Trust boundaries, planes (SSH admin vs TLS data), components |
| [PROTOCOL.md](./PROTOCOL.md) | Wire framing, request/response types, scopes |
| [CODEBASE.md](./CODEBASE.md) | Crate/file map, where to change what |
| [CONFIG.md](./CONFIG.md) | `~/.config/gdr/config.json`, tokens.json, env vars |
| [TOKENS.md](./TOKENS.md) | Multi-token lifecycle, scopes, expiry, revocation |
| [MCP.md](./MCP.md) | MCP tools, password tool tradeoff, host resolution |
| [DEPLOYMENT.md](./DEPLOYMENT.md) | `deploy.sh`, system `.deb`/`.rpm`, MCP Cursor setup, systemd |
| [TESTING.md](./TESTING.md) | Unit tests, how we E2E against a real GNOME host |
| [PROGRESS.md](./PROGRESS.md) | What is built, what is next, open decisions |
| [SECURITY.md](./SECURITY.md) | Secrets handling, what never goes in git |
| [HEADLESS.md](./HEADLESS.md) | No-monitor targets, virtual Meta-* fallback, 1×1 caveat |
| [data/captures/](./data/captures/) | Runtime gdr screenshots (gitignored PNGs) |

**Audience split**

- **Target** (GNOME/Wayland machine): runs `gdrd` as `systemd --user`.
- **Controller** (laptop / agent host): runs `gdr` CLI and/or `gdr-mcp`.

Same shape as SSH: the daemon lives on the machine you control; the client is thin.
