# Wire protocol

Canonical types live in [`common/src/lib.rs`](../common/src/lib.rs).
The TypeScript mirror is [`mcp-server/src/gdrClient.ts`](../mcp-server/src/gdrClient.ts).
**Keep them in sync by hand** — there is no codegen yet.

## Framing

```
[ 4 bytes: payload length, big-endian u32 ]
[ N bytes: UTF-8 JSON ]
```

- Max frame: 64 MiB (`MAX_FRAME_BYTES`) — screenshots as base64 need headroom.
- One request → one response per connection; no pipelining.
- Transport: TLS 1.2/1.3 (rustls). Server name advertised/expected: `gdrd`.
- Cert trust: fingerprint pin (TOFU), not a public CA.

## Requests (`type` tag)

| type | fields | required scope |
|---|---|---|
| `Auth` | `token: string` | (must be first message) |
| `Screenshot` | `connector: string \| null` | `screenshot` |
| `CaptureFrame` | see below (all optional) | `screenshot` |
| `MouseMove` | `x, y: f64` (pixels in stream space) | `mouse` |
| `MouseButton` | `button: i32` (evdev), `pressed: bool` | `mouse` |
| `MouseScroll` | `dx, dy: f64` | `mouse` |
| `KeyEvent` | `keycode: u32` (evdev), `pressed: bool` | `keyboard` |
| `TypeText` | `text: string` | `type` |
| `GetCursor` | — | `mouse` |
| `ListWindows` | `include_skip_taskbar: bool` | `window` |
| `WindowAction` | `target: WindowTarget`, `action` + its args (flattened) | `window` |
| `LaunchApp` | `app_id: string` | `window` |
| `ListApps` | `filter: string \| null` | `window` |
| `WindowEvents` | `since: u64`, `limit: u32`, `wait_ms: u64` | `window` |
| `HookCreate` | `kind: activity\|window` + that kind's fields (flattened), `label`, `enabled` | `screenshot` (activity) / `window` (window) |
| `HookUpdate` | `id`, `enabled?`, `label?`, `spec?` | per hook (see below) |
| `HookRemove` | `id` | per hook |
| `HookList` | — | per hook |
| `HookPoll` | `id?`, `since: u64`, `limit: u32`, `wait_ms: u64` | per hook |
| `Ping` | — | none (auth only) |

The `Window*` requests need the `gdr-windows` GNOME Shell extension on the
target — see [WINDOWS.md](./WINDOWS.md) for why, and for the two coordinate
spaces `WindowInfo` reports.

Button constants: `0x110` left, `0x111` right, `0x112` middle.

### `CaptureFrame`

Screenshot with server-side crop, downscale, encode and change detection.
Every field is optional; with none set it is equivalent to `Screenshot`.
Doing the work here means the controller never decodes and re-encodes an
image gdrd just produced.

| field | type | meaning |
|---|---|---|
| `region` | `{x,y,width,height}` | Crop in native pixels, applied before downscale |
| `max_width` / `max_height` | `u32` | Fit inside this box, aspect preserved |
| `max_long_edge` | `u32` | Cap the longer edge |
| `max_patches` | `u32` | Vision-model tiling budget — fit within N cells |
| `patch_size` | `u32` | Cell size for `max_patches` (default 28) |
| `format` | `"png" \| "jpeg"` | Default `png` |
| `quality` | `u8` | JPEG quality 1–100 (default 85) |
| `settle` | `{quiet_ms, timeout_ms}` | Wait for damage to stop before capturing |
| `if_none_match` | `string` | Return `unchanged` if the frame hashes to this |

Replies with `Frame`:

| field | meaning |
|---|---|
| `data_base64` | Image bytes; **empty when `unchanged`** |
| `format` | `"png"` or `"jpeg"` |
| `native_width` / `native_height` | Full desktop size, regardless of crop |
| `region` | Portion of the desktop this image covers, in native pixels |
| `image_width` / `image_height` | Encoded size (`region` after downscale) |
| `hash` | Content hash; pass back as `if_none_match` |
| `unchanged` | Screen matched `if_none_match`; no image sent |
| `settled` | `false` when a requested settle hit its timeout |
| `capture_ms` | Server-side time for the whole capture |

Sizing notes:

- **`max_patches` is applied server-side on purpose.** The constraint is
  `ceil(w/p) * ceil(h/p) <= max_patches`, which depends on the native aspect
  ratio — no pixel box can express it, and the controller does not know the
  screen size before its first capture.
- Downscaling never upscales, and a source already within every limit is
  passed through untouched rather than snapped to a cell boundary.
- A `region` whose origin is off-screen is an **error**, not a clamp.
  Clamping would return a 1px sliver that reads as a successful capture of
  the wrong thing.
- `hash` is salted with output geometry and format, so changing any capture
  parameter can never be mistaken for "screen unchanged". It is a fast
  non-cryptographic hash for change detection — not an integrity check.

## Responses

| type | meaning |
|---|---|
| `AuthOk` | authenticated (current clients match this) |
| `AuthOkScoped` | authenticated + `scopes: string[]` (optional future) |
| `AuthFailed` | bad/expired/revoked token |
| `Ok` | mutation succeeded |
| `Pong` | ping reply |
| `Screenshot` | `png_base64: string` |
| `Frame` | sized/encoded capture + geometry — see `CaptureFrame` above |
| `CursorPosition` | `x, y: f64`, `known: bool` (last `MouseMove`; false if none yet) |
| `Windows` | `windows[]`, `monitors[]`, `capture_connector`, `focus_window`, `seq`, workspace counts |
| `WindowActed` | `action`, `window` (state **after** the compositor had its say), `detail` |
| `AppLaunched` | `app_id`, `name`, `was_running` (false = we started it) |
| `Apps` | `apps[]` — `app_id`, `name`, `windows`, `running` |
| `WindowEvents` | `events[]`, `next_seq`, `dropped` (fell behind the ring), `reset` (shell restarted) |
| `Hook` | `hook: HookStatus` — reply to create/update/remove (for a removal, as it stood just before it went) |
| `Hooks` | `hooks[]` — only those the token's scopes cover |
| `HookEvents` | `events[]`, `next_seq`, `dropped`, `hooks[]` (live state, so an empty drain can say which kind of empty) |
| `Error` | `message: string` (including permission denied) |

## Auth + scopes

After `AuthOk`, the server remembers that connection’s `ScopeSet`.
Each later request calls `Request::required_scope()`; missing scope →
`Error { message: "permission denied: ..." }` and an audit `denied` event.

**Hooks are the exception to one-scope-per-request.** `HookUpdate`,
`HookRemove`, `HookList` and `HookPoll` can name subscriptions of either kind,
so a single `required_scope()` would be either too strict or too loose. They
return `None` from that check and the daemon filters *per hook* instead:
listing and polling return only what the token's scopes cover, and naming a
hook it may not see is a permission error. See [HOOKS.md](./HOOKS.md).

**Revocation semantics (current):** revoke blocks *new* connections.
Already-open sessions keep working until disconnect (SSH-like).
Immediate kill of live sessions is an open enhancement — see PROGRESS.md.

## Batch mode (CLI only)

`gdr batch` reads one JSON action per stdin line on a **single** TLS
connection and writes one `JsonResult` per stdout line. Actions:

```json
{"action":"screenshot"}
{"action":"click","x":640,"y":400,"button":"left"}
{"action":"move","x":10,"y":10}
{"action":"key","keycode":28}
{"action":"type","text":"hello"}
{"action":"ping"}
```
