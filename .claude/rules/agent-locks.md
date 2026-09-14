# Claiming work with agent-locks

This repo is worked on from several git worktrees at once, often by several
agents at once. [`agent-locks`](https://github.com/luohoa97/agent-locks) is the
MCP server that keeps those from silently overwriting each other: markdown lock
files under the repository's **shared** `.git` directory, visible from every
worktree of this repo, and structurally impossible to commit (a path under
`.git/` cannot enter the index — `git add` on one is a silent no-op).

No database, no daemon, no credentials. If the `mcp__agent-locks__*` tools are
not present, this section does not apply — carry on without them.

## The loop

1. **Look before you start.** `lock_query` (default view: active locks) to see
   what other agents are already doing, then `lock_check_conflict` with the
   globs you are about to touch. Conflict checking is *informational* — it
   never blocks you, it hands you what you need to make the call yourself.
2. **Claim it.** `lock_create` with a title, the globs you are touching, and a
   checklist of what you plan to do. Do this before the first edit, not after.
3. **Update as you go.** `lock_update` the moment each task is actually done —
   not batched at the end. The whole value is that another agent can see live
   state; a lock that only becomes accurate just before it is finished told
   nobody anything while the work was happening.
4. **Close it out.** `lock_finish` with a short summary. That moves the lock
   into the `done/` archive, where it doubles as a readable log of what changed
   and why — which outlives the session that did it.

## What is worth a lock here

Anything that spans more than one file or more than a few minutes, and in
particular the seams where this codebase is mirrored by hand:

- `common/src/*.rs` ↔ `mcp-server/src/gdrClient.ts` — the wire protocol exists
  twice and is kept in step manually.
- `shell-extension/…/extension.js` ↔ `server/src/windows.rs` — the JSON the
  extension emits is parsed by hand on the Rust side.
- `server/src/hooks.rs`, `common/src/hooks.rs`, `mcp-server/src/hooks.ts` — one
  feature across three languages.

A one-line fix in a single file does not need ceremony. Two agents editing the
protocol from different worktrees absolutely does.

## Honesty about identity

The server cannot detect your agent id or your parent's — no MCP transport
exposes that. Pass `agent_id` / `parent_agent_id` to `lock_create` **only** if
your own context already gave you explicit ids. Otherwise omit them and they
are recorded as null. Never guess or invent one; a lock attributed to a
fabricated agent is worse than an anonymous lock.
