# Token lifecycle

## Goals

- Multiple named tokens per host (you / agent / script).
- Server stores **hashes only** (SHA-256; tokens are high-entropy, not passwords).
- Optional expiry (`never` / `30d` / `12h` / `15m`).
- Permission scopes: `screenshot`, `mouse`, `keyboard`, `type`, or `all`.
- Management over **SSH admin plane**, not the live TLS protocol.

## Why admin-plane management?

If “create token” were a wire message, a leaked scoped data-plane token
could mint a wider token. Keeping mint/revoke on SSH means a leaked
`screenshot`-only token can only take screenshots until it expires or is
revoked — it cannot escalate.

## CLI

Tokens are stored on the **target** running `gdrd`
(`~/.local/share/gdr/tokens.json`) — not in your controller `config.json`.
The controller profile only holds the plaintext you present when connecting.

```bash
# This machine — omit HOST, or use me / local / --host me (no SSH)
gdr token list
gdr token list --host me
gdr token list me
gdr token create --label agent --scope all --expires never
gdr token revoke tok_ab12cd34...                 # defaults to me
gdr token revoke --host me tok_ab12cd34...
gdr token revoke tok_ab12cd34... desktop         # remote by positional HOST

# Remote host (uses profile `ssh:` over SSH)
gdr token create desktop --label "me" --scope all --expires never
gdr token create desktop --label "cursor-agent" \
  --scope screenshot,mouse,keyboard,type --expires 30d
gdr token list desktop
gdr token revoke desktop tok_ab12cd34...
```

Plaintext is printed **once** at creation (GitHub PAT pattern). Save it into
the matching device profile:

```bash
gdr device set-token local --token '<plaintext>'
# or: gdr device add local --local --token '<plaintext>' --label "home computer"
```

## Auth path inside gdrd

1. Reload `tokens.json` (so SSH revoke/create applies without restart).
2. SHA-256 the presented bearer; find matching non-revoked entry.
3. Reject if `expires_at < now`.
4. Attach `ScopeSet` to the connection; update `last_used_at`.
5. If no file match, accept legacy `GDR_TOKEN` env as `id=env`, scope `all`.

## Deploy bootstrap

`deploy.sh` generates `openssl rand -hex 32`, puts it in the unit’s
`Environment=GDR_TOKEN=...`, then runs:

```bash
GDR_TOKEN=... gdrd --seed-token
```

That creates the first `tokens.json` entry labeled `initial-install`,
scope `all`, `expires: null`. Behavior matches the old single-token world
until you issue more tokens.

## Open decisions (also in PROGRESS.md)

1. **Live revocation:** today = no new connections. Immediate kill of
   existing sessions needs a connection map keyed by token id.
2. **Default deploy scope:** currently `all` + never-expire (simplest).
3. **Audit rotation policy:** gdrd self-rotates at 5 MiB; logrotate drop-in
   is optional later.
