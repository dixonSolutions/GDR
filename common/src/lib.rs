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

pub mod scopes;

pub use scopes::{Scope, ScopeSet};

pub const MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024; // 64 MiB, generous for a full screenshot as base64

/// Default TCP port for gdrd.
pub const DEFAULT_PORT: u16 = 7337;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Request {
    /// Must be the first message on every connection.
    Auth { token: String },

    /// Take a single screenshot of the given monitor (or the primary one
    /// if `connector` is None) and return it as base64-encoded PNG.
    Screenshot { connector: Option<String> },

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

    Ping,
}

impl Request {
    /// Which permission scope this request needs after Auth.
    /// `Auth` and `Ping` need no scope (Ping only requires a live session).
    pub fn required_scope(&self) -> Option<Scope> {
        match self {
            Request::Auth { .. } | Request::Ping => None,
            Request::Screenshot { .. } => Some(Scope::Screenshot),
            Request::MouseMove { .. }
            | Request::MouseButton { .. }
            | Request::MouseScroll { .. } => Some(Scope::Mouse),
            Request::KeyEvent { .. } => Some(Scope::Keyboard),
            Request::TypeText { .. } => Some(Scope::Type),
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
    Pong,
    Error { message: String },
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
