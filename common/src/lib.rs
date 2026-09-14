//! Shared wire protocol between the `server` (runs on the GNOME/Wayland
//! target, inside the user's graphical session), the `client` (thin CLI,
//! runs anywhere), and any MCP server wrapping this for agent use (e.g.
//! the TypeScript one in `mcp-server/`).
//!
//! Framing: every message is `[u32 length (big-endian)][UTF-8 JSON payload]`.
//! JSON (not a Rust-specific binary format) is deliberate: it means an MCP
//! server in TypeScript/Python/whatever can speak this protocol directly
//! without depending on this Rust crate or replicating a binary encoding.
//! Transport: TLS (rustls) over TCP. Auth is a pre-shared token exchanged
//! as the very first message on a new connection — NOT the sudo password.
//! The sudo password is only ever used out-of-band (SSH) to install/start
//! the server binary; it never touches this protocol.

use serde::{Deserialize, Serialize};
use std::io;

pub mod hooks;
pub mod scopes;
pub mod windows;

pub use hooks::{
    ActivityReport, ActivityScope, ActivitySpec, Circle, HookEvent, HookKind, HookPollResult,
    HookSpec, HookState, HookStatus, ProcessInfo, WindowChange, WindowEventInfo, WindowHookSpec,
};
pub use scopes::{Scope, ScopeSet};
pub use windows::{
    AppInfo, LogicalRect, MonitorInfo, TargetError, WindowBackend, WindowEvent, WindowInfo,
    WindowOp, WindowTarget,
};

pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024; // 64 MiB, generous for a full screenshot as base64

/// Default TCP port for gdrd.
pub const DEFAULT_PORT: u16 = 7337;

/// Encoding for [`Request::CaptureFrame`].
///
/// JPEG is the default for agent traffic: identical visual-token cost (image
/// billing is on decoded pixel dimensions, not bytes) but a much smaller
/// payload. PNG stays for lossless/debug captures.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
}

impl ImageFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
        }
    }
}

/// Rectangle in native stream pixels (the coordinate space Mutter's
/// `NotifyPointerMotionAbsolute` uses).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Wait for the compositor to stop emitting damage before grabbing a frame.
///
/// Mutter's ScreenCast stream is emit-on-damage, so "no new buffer for
/// `quiet_ms`" means the UI has finished painting. `timeout_ms` bounds
/// screens that never go quiet (spinners, video, blinking carets).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settle {
    pub quiet_ms: u64,
    pub timeout_ms: u64,
}

impl Default for Settle {
    fn default() -> Self {
        Self {
            quiet_ms: 120,
            // Bounded by the Doherty threshold. Damage is tracked per stream,
            // not per region, so one blinking caret anywhere on the desktop
            // keeps every capture "busy" and no amount of patience will find a
            // quiet window — measured on a working desktop, the frame changed
            // in 11 of 11 samples 200 ms apart. Timing out is not a failure:
            // the newest frame is still returned, flagged `settled: false`.
            timeout_ms: 400,
        }
    }
}

/// A captured, already-sized, already-encoded frame.
///
/// gdrd crops, resizes and encodes in one pass from the raw PipeWire buffer
/// so controllers never decode-and-re-encode what the daemon just produced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Frame {
    /// Base64 image bytes. Empty when `unchanged` is true.
    pub data_base64: String,
    pub format: ImageFormat,
    /// Full desktop size, regardless of any crop.
    pub native_width: u32,
    pub native_height: u32,
    /// Portion of the desktop this image covers, in native pixels.
    pub region: Region,
    /// Encoded image size (`region` after downscale).
    pub image_width: u32,
    pub image_height: u32,
    /// Content hash; pass back as `if_none_match` to skip identical frames.
    pub hash: String,
    pub unchanged: bool,
    /// False when a requested settle hit its timeout (screen never went quiet).
    pub settled: bool,
    pub capture_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Request {
    /// Must be the first message on every connection.
    Auth { token: String },

    /// Take a single screenshot of the given monitor (or the primary one
    /// if `connector` is None) and return it as base64-encoded PNG.
    ///
    /// Legacy full-resolution path. Prefer [`Request::CaptureFrame`], which
    /// sizes and encodes server-side in a single pass.
    Screenshot { connector: Option<String> },

    /// Capture with server-side crop, downscale, encode and change detection.
    ///
    /// All of these are optional; the default is a native-size PNG, i.e.
    /// equivalent to `Screenshot`.
    CaptureFrame {
        /// Crop in native pixels, applied before any downscale. Clamped to
        /// the frame; an empty intersection is an error, not a silent full
        /// frame (a wrong crop must not look like a working screenshot).
        #[serde(default)]
        region: Option<Region>,
        /// Downscale to fit this box, aspect preserved. Never upscales.
        #[serde(default)]
        max_width: Option<u32>,
        #[serde(default)]
        max_height: Option<u32>,
        /// Cap on the longer edge, in pixels.
        #[serde(default)]
        max_long_edge: Option<u32>,
        /// Vision-model tiling budget: fit within this many `patch_size`
        /// cells, where an image costs `ceil(w/p) * ceil(h/p)`. Applied
        /// server-side because it depends on the native aspect ratio, which
        /// only the daemon knows before the first capture.
        #[serde(default)]
        max_patches: Option<u32>,
        /// Patch cell size for `max_patches`. Defaults to 28.
        #[serde(default)]
        patch_size: Option<u32>,
        #[serde(default)]
        format: ImageFormat,
        /// JPEG quality 1..=100. Ignored for PNG.
        #[serde(default)]
        quality: Option<u8>,
        #[serde(default)]
        settle: Option<Settle>,
        /// Return `unchanged: true` with no image when the frame hashes to
        /// this value.
        #[serde(default)]
        if_none_match: Option<String>,
    },

    /// Absolute mouse move, in real pixel coordinates of the target
    /// stream (server maps these directly to Mutter's coordinate space).
    MouseMove { x: f64, y: f64 },

    /// Mouse button click/press/release. `button` follows Linux evdev
    /// button codes: 0x110 = BTN_LEFT, 0x111 = BTN_RIGHT, 0x112 = BTN_MIDDLE.
    MouseButton { button: i32, pressed: bool },

    MouseScroll { dx: f64, dy: f64 },

    /// Key event by evdev keycode (NOT X keysym) - see `common::keymap`.
    KeyEvent { keycode: u32, pressed: bool },

    /// Convenience: type a UTF-8 string by translating each char through
    /// the server's loaded keymap (press+release per character).
    TypeText { text: String },

    /// Return the last absolute pointer position injected via `MouseMove`
    /// (Mutter RemoteDesktop does not expose a live query). Unknown until
    /// the first move/click from a controller.
    GetCursor,

    /// Enumerate every managed window, with logical *and* capture-stream
    /// geometry. Needs the `gdr-windows` shell extension on the target.
    ListWindows {
        /// Include dock/panel/notification windows, which are normally noise.
        #[serde(default)]
        include_skip_taskbar: bool,
    },

    /// Activate / minimize / move / close one window.
    ///
    /// `Activate` is the interesting one: it is what makes a window that is
    /// minimized, buried, or on another workspace visible to `CaptureFrame`,
    /// which streams the composited screen and cannot see a window that is
    /// not on it.
    WindowAction {
        target: WindowTarget,
        #[serde(flatten)]
        op: WindowOp,
    },

    /// Start an installed app, or raise it if it is already running. The
    /// path to "act on a window that is not currently open at all".
    LaunchApp { app_id: String },

    /// Installed apps, optionally filtered by a substring of id or name.
    ListApps {
        #[serde(default)]
        filter: Option<String>,
    },

    /// Poll the window open/close/focus journal.
    ///
    /// Pass the previous reply's `next_seq` as `since`. `wait_ms > 0` holds
    /// the request open until something happens or the wait elapses, so a
    /// watcher costs one connection instead of a busy loop.
    WindowEvents {
        #[serde(default)]
        since: u64,
        #[serde(default)]
        limit: u32,
        #[serde(default)]
        wait_ms: u64,
    },

    /// Start a standing subscription — a screen-activity or window watcher
    /// that keeps running between requests. See [`hooks`].
    HookCreate {
        #[serde(flatten)]
        spec: HookSpec,
        #[serde(default)]
        label: Option<String>,
        /// Create it switched off. Default is on: an agent that asked for a
        /// subscription wants one, not a form to fill in twice.
        #[serde(default = "yes")]
        enabled: bool,
    },

    /// Toggle a hook, or replace its configuration in place.
    ///
    /// Toggling keeps the id, the buffered events and the counters, which is
    /// what makes "watch while I do this, then stop" cheap.
    HookUpdate {
        id: String,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        spec: Option<HookSpec>,
    },

    /// Forget a hook and drop its buffered events.
    HookRemove { id: String },

    /// Every hook this token is allowed to see.
    HookList,

    /// Drain the hook journal, optionally blocking until something lands.
    ///
    /// `since` is the previous reply's `next_seq`. Sequence numbers are
    /// global across hooks, so one poll drains every subscription at once;
    /// `id` narrows it to one.
    HookPoll {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        since: u64,
        #[serde(default)]
        limit: u32,
        #[serde(default)]
        wait_ms: u64,
    },

    Ping,
}

fn yes() -> bool {
    true
}

impl Request {
    /// Which permission scope this request needs after Auth.
    /// `Auth` and `Ping` need no scope (Ping only requires a live session).
    pub fn required_scope(&self) -> Option<Scope> {
        match self {
            Request::Auth { .. } | Request::Ping => None,
            Request::Screenshot { .. } | Request::CaptureFrame { .. } => Some(Scope::Screenshot),
            Request::MouseMove { .. }
            | Request::MouseButton { .. }
            | Request::MouseScroll { .. }
            | Request::GetCursor => Some(Scope::Mouse),
            Request::KeyEvent { .. } => Some(Scope::Keyboard),
            Request::TypeText { .. } => Some(Scope::Type),
            Request::ListWindows { .. }
            | Request::WindowAction { .. }
            | Request::LaunchApp { .. }
            | Request::ListApps { .. }
            | Request::WindowEvents { .. } => Some(Scope::Window),
            // A hook needs the scope of the thing it watches: screen
            // activity is a (coarse) read of the screen, window events are
            // metadata.
            Request::HookCreate { spec, .. } => Some(spec.required_scope()),
            // These can name hooks of either kind, and a single scope here
            // would be either too strict or too loose. The daemon checks
            // each hook against the connection's scopes instead — see
            // `hook_scope_denial` in gdrd — so listing and polling return
            // only what the token may see rather than failing outright.
            Request::HookUpdate { .. }
            | Request::HookRemove { .. }
            | Request::HookList
            | Request::HookPoll { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Response {
    Ok,
    AuthOk,
    /// Auth succeeded and the server is returning the granted scopes
    /// (optional; older clients ignore unknown variants carefully via
    /// AuthOk-only matching, new clients may read this).
    AuthOkScoped { scopes: Vec<String> },
    AuthFailed,
    Screenshot { png_base64: String },
    /// Reply to [`Request::CaptureFrame`]. Boxed so the enum stays small.
    Frame(Box<Frame>),
    /// Last known absolute pointer position (see `Request::GetCursor`).
    CursorPosition { x: f64, y: f64, known: bool },
    /// Reply to [`Request::ListWindows`]. Boxed so the enum stays small.
    Windows(Box<WindowList>),
    /// Reply to [`Request::WindowAction`], carrying the window's state
    /// *after* the compositor had its say — a move can be clamped and a
    /// close can be refused, and the caller needs to see that.
    WindowActed {
        action: String,
        window: Option<Box<WindowInfo>>,
        #[serde(default)]
        detail: Option<String>,
    },
    AppLaunched {
        app_id: String,
        name: Option<String>,
        /// False means we started it; true means it was already running and
        /// we raised it.
        was_running: bool,
    },
    Apps { apps: Vec<AppInfo> },
    /// Reply to [`Request::HookCreate`], [`Request::HookUpdate`] and
    /// [`Request::HookRemove`] — the hook as it now stands (for a removal,
    /// as it stood just before it went).
    Hook { hook: Box<HookStatus> },
    Hooks { hooks: Vec<HookStatus> },
    /// Reply to [`Request::HookPoll`]. Boxed so the enum stays small.
    HookEvents(Box<HookPollResult>),
    WindowEvents {
        events: Vec<WindowEvent>,
        /// Pass back as `since` on the next poll.
        next_seq: u64,
        /// The caller fell behind the extension's ring and lost events.
        dropped: bool,
        /// The compositor (and its sequence numbers) restarted under us.
        reset: bool,
    },
    Pong,
    Error { message: String },
}

/// Payload of [`Response::Windows`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowList {
    pub backend: WindowBackend,
    pub windows: Vec<WindowInfo>,
    pub monitors: Vec<MonitorInfo>,
    pub focus_window: Option<u64>,
    pub active_workspace: i32,
    pub n_workspaces: i32,
    /// Connector gdrd is streaming. Only windows on this monitor have a
    /// `stream_region`, and only they can be screenshotted.
    pub capture_connector: Option<String>,
    /// Event sequence at the moment of the listing; a fine `since` for a
    /// first `WindowEvents` poll.
    pub seq: u64,
}

#[derive(thiserror::Error, Debug)]
pub enum ProtocolError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u32),
    #[error("json encode/decode error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Async framing helpers. Kept generic over tokio's AsyncRead/AsyncWrite so
/// they work identically over plain TCP (testing) or a TLS stream (prod).
pub mod framing {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn write_message<W, T>(writer: &mut W, msg: &T) -> Result<(), ProtocolError>
    where
        W: AsyncWrite + Unpin,
        T: Serialize,
    {
        let payload = serde_json::to_vec(msg)?;
        let len = payload.len() as u32;
        if len > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge(len));
        }
        writer.write_all(&len.to_be_bytes()).await?;
        writer.write_all(&payload).await?;
        writer.flush().await?;
        Ok(())
    }

    pub async fn read_message<R, T>(reader: &mut R) -> Result<T, ProtocolError>
    where
        R: AsyncRead + Unpin,
        T: for<'de> Deserialize<'de>,
    {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf);
        if len > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge(len));
        }
        let mut payload = vec![0u8; len as usize];
        reader.read_exact(&mut payload).await?;
        Ok(serde_json::from_slice(&payload)?)
    }
}

/// Minimal ASCII -> evdev keycode map for `TypeText`. Covers lowercase
/// letters, digits, space and a handful of punctuation marks -- enough for
/// an MVP. Extend as needed, or move to a proper xkbcommon-based layout
/// lookup on the server side for full Unicode/layout support.
pub mod keymap {
    /// Returns (keycode, needs_shift) for a given ASCII char, evdev codes.
    pub fn ascii_to_evdev(c: char) -> Option<(u32, bool)> {
        let lower = c.to_ascii_lowercase();
        let letter_row = "qwertyuiopasdfghjklzxcvbnm";
        if let Some(pos) = letter_row.find(lower) {
            let codes = [
                16, 17, 18, 19, 20, 21, 22, 23, 24, 25, // qwertyuiop
                30, 31, 32, 33, 34, 35, 36, 37, 38, // asdfghjkl
                44, 45, 46, 47, 48, 49, 50, // zxcvbnm
            ];
            return Some((codes[pos], c.is_ascii_uppercase()));
        }
        match lower {
            ' ' => Some((57, false)),
            '\n' => Some((28, false)),
            '\t' => Some((15, false)),
            '0' => Some((11, false)),
            '1'..='9' => Some((2 + (lower as u32 - '1' as u32), false)),
            '-' => Some((12, false)),
            '=' => Some((13, false)),
            '[' => Some((26, false)),
            ']' => Some((27, false)),
            ';' => Some((39, false)),
            '\'' => Some((40, false)),
            '`' => Some((41, false)),
            '\\' => Some((43, false)),
            ',' => Some((51, false)),
            '.' => Some((52, false)),
            '/' => Some((53, false)),
            // shifted punctuation
            '!' => Some((2, true)),
            '@' => Some((3, true)),
            '#' => Some((4, true)),
            '$' => Some((5, true)),
            '%' => Some((6, true)),
            '^' => Some((7, true)),
            '&' => Some((8, true)),
            '*' => Some((9, true)),
            '(' => Some((10, true)),
            ')' => Some((11, true)),
            '_' => Some((12, true)),
            '+' => Some((13, true)),
            '{' => Some((26, true)),
            '}' => Some((27, true)),
            ':' => Some((39, true)),
            '"' => Some((40, true)),
            '~' => Some((41, true)),
            '|' => Some((43, true)),
            '<' => Some((51, true)),
            '>' => Some((52, true)),
            '?' => Some((53, true)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    #[test]
    fn request_json_roundtrip_auth() {
        let req = Request::Auth {
            token: "abc".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""type":"Auth""#));
        let back: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn request_json_shape_matches_ts_client() {
        // The TypeScript client in mcp-server/src/gdrClient.ts serializes
        // the same tagged enum. Guard the wire shape so a rename here
        // breaks tests instead of silently breaking MCP.
        let req = Request::MouseMove { x: 1.5, y: 2.5 };
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(v["type"], "MouseMove");
        assert_eq!(v["x"], 1.5);
        assert_eq!(v["y"], 2.5);
    }

    #[test]
    fn response_auth_failed_deserializes() {
        let raw = r#"{"type":"AuthFailed"}"#;
        let r: Response = serde_json::from_str(raw).unwrap();
        assert_eq!(r, Response::AuthFailed);
    }

    #[test]
    fn required_scope_mapping() {
        assert_eq!(
            Request::Screenshot { connector: None }.required_scope(),
            Some(Scope::Screenshot)
        );
        assert_eq!(
            Request::MouseMove { x: 0.0, y: 0.0 }.required_scope(),
            Some(Scope::Mouse)
        );
        assert_eq!(
            Request::KeyEvent {
                keycode: 30,
                pressed: true
            }
            .required_scope(),
            Some(Scope::Keyboard)
        );
        assert_eq!(
            Request::TypeText {
                text: "hi".into()
            }
            .required_scope(),
            Some(Scope::Type)
        );
        assert_eq!(Request::Ping.required_scope(), None);
        // The window plane is its own scope: it exposes titles, not pixels,
        // so a screenshot-only token must not reach it and vice versa.
        assert_eq!(
            Request::ListWindows {
                include_skip_taskbar: false
            }
            .required_scope(),
            Some(Scope::Window)
        );
        assert_eq!(
            Request::LaunchApp {
                app_id: "x.desktop".into()
            }
            .required_scope(),
            Some(Scope::Window)
        );
        let screenshot_only = ScopeSet::parse_list("screenshot").unwrap();
        assert!(!screenshot_only.allows(
            Request::ListWindows {
                include_skip_taskbar: false
            }
            .required_scope()
        ));
    }

    #[test]
    fn window_action_wire_shape_flattens_the_op() {
        // The TS client builds `{type, target, action, ...args}` in one
        // object literal; a nested op here would break it silently.
        let req = Request::WindowAction {
            target: WindowTarget {
                title: Some("notes".into()),
                ..Default::default()
            },
            op: WindowOp::MoveResize {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(v["type"], "WindowAction");
        assert_eq!(v["action"], "move_resize");
        assert_eq!(v["width"], 800);
        assert_eq!(v["target"]["title"], "notes");
        let back: Request = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn keymap_letters_and_shift() {
        let (code, shift) = keymap::ascii_to_evdev('a').unwrap();
        assert_eq!(code, 30);
        assert!(!shift);
        let (code, shift) = keymap::ascii_to_evdev('A').unwrap();
        assert_eq!(code, 30);
        assert!(shift);
        assert_eq!(keymap::ascii_to_evdev('1').unwrap(), (2, false));
        assert_eq!(keymap::ascii_to_evdev('!').unwrap(), (2, true));
        assert_eq!(keymap::ascii_to_evdev(' ').unwrap(), (57, false));
        assert!(keymap::ascii_to_evdev('é').is_none());
    }

    #[tokio::test]
    async fn framing_roundtrip() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut writer: DuplexStream = client;
        let mut reader: DuplexStream = server;

        let msg = Request::TypeText {
            text: "hello".into(),
        };
        let write = framing::write_message(&mut writer, &msg);
        let read = framing::read_message::<_, Request>(&mut reader);
        let (w, r) = tokio::join!(write, read);
        w.unwrap();
        assert_eq!(r.unwrap(), msg);
    }

    #[tokio::test]
    async fn framing_rejects_oversized_length_prefix() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        // Craft a length prefix larger than MAX_FRAME_BYTES.
        let bad_len = (MAX_FRAME_BYTES + 1).to_be_bytes();
        tokio::io::AsyncWriteExt::write_all(&mut client, &bad_len)
            .await
            .unwrap();
        let err = framing::read_message::<_, Request>(&mut server)
            .await
            .unwrap_err();
        match err {
            ProtocolError::FrameTooLarge(n) => assert_eq!(n, MAX_FRAME_BYTES + 1),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn framing_multiple_messages() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        framing::write_message(&mut a, &Request::Ping).await.unwrap();
        framing::write_message(
            &mut a,
            &Request::Screenshot {
                connector: Some("eDP-1".into()),
            },
        )
        .await
        .unwrap();

        let r1: Request = framing::read_message(&mut b).await.unwrap();
        let r2: Request = framing::read_message(&mut b).await.unwrap();
        assert_eq!(r1, Request::Ping);
        assert_eq!(
            r2,
            Request::Screenshot {
                connector: Some("eDP-1".into())
            }
        );
    }
}
