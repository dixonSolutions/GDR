# Security notes

## What is sensitive

| Secret | Where it lives | Never commit |
|---|---|---|
| Data-plane bearer token | controller `config.json`, target unit env (legacy) | yes |
| Token hashes | target `tokens.json` | yes (still sensitive metadata) |
| TLS key | target `key.pem` | yes |
| Sudo / user passwords | controller `config.json` (opt-in, plaintext) | yes |
| SSH private keys | `~/.ssh` | yes |

`.gitignore` excludes `*.pem`, `*.key`, and local config paths. Review
`git status` before every commit.

## Design choices that limit blast radius

1. **Sudo password never on the data plane** — install/admin only, over SSH.  
2. **Token mint/revoke over SSH** — scoped token cannot escalate.  
3. **MCP cannot invent IPs** — only saved profiles (or fixed env).  
4. **Cert pinning** — MitM on the LAN/Tailscale path fails closed once pinned.  
5. **chmod 600** on config.json, tokens.json, key.pem, audit.log.  
6. **No 24/7 screen broadcast** — gdrd opens Mutter ScreenCast on demand;
   physical monitors idle-stop after `GDR_DISPLAY_IDLE_SECS` (default 45).
   MCP keeps its process up for Cursor health, but drops the TLS link to
   gdrd after `GDR_MCP_IDLE_MS` (default 15s). Headless virtual Meta-*
   stays up once started (see HEADLESS.md).  

## Explicit tradeoffs you opted into

- **Stored sudo/user passwords in plaintext** on the controller disk.  
- **`gdr_get_password`** returns plaintext into the model transcript.  
  Safer future alternative: `gdr_run_privileged` (password stays in-process).  

## Operational hygiene

- Prefer scoped, expiring tokens for agents; keep `initial-install` for you.  
- Rotate with `scripts/rotate-token.sh` or `gdr token create` + revoke old.  
- After a suspected leak: revoke token, rotate, check `gdr audit`.  
- Do not paste live tokens into chats; `token list` is safe (hashes/meta only).  

## Passwords in agent sessions

If an operator pastes a sudo password into a chat so the agent can deploy,
treat that transcript as compromised for that password and rotate when done.
Prefer `GDR_SUDO_PASSWORD` in the agent’s ephemeral env over putting it in
repo files or docs.
