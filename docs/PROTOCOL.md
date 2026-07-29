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
| `MouseMove` | `x, y: f64` (pixels in stream space) | `mouse` |
| `MouseButton` | `button: i32` (evdev), `pressed: bool` | `mouse` |
| `MouseScroll` | `dx, dy: f64` | `mouse` |
| `KeyEvent` | `keycode: u32` (evdev), `pressed: bool` | `keyboard` |
| `TypeText` | `text: string` | `type` |
| `Ping` | — | none (auth only) |

Button constants: `0x110` left, `0x111` right, `0x112` middle.

## Responses

| type | meaning |
|---|---|
| `AuthOk` | authenticated (current clients match this) |
| `AuthOkScoped` | authenticated + `scopes: string[]` (optional future) |
| `AuthFailed` | bad/expired/revoked token |
| `Ok` | mutation succeeded |
| `Pong` | ping reply |
| `Screenshot` | `png_base64: string` |
| `Error` | `message: string` (including permission denied) |

## Auth + scopes

After `AuthOk`, the server remembers that connection’s `ScopeSet`.
Each later request calls `Request::required_scope()`; missing scope →
`Error { message: "permission denied: ..." }` and an audit `denied` event.

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
