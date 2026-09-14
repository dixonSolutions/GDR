mod audit;
mod capture;
mod display;
mod mutter_dbus;
mod tls;
mod tokens;
mod windows;

use anyhow::{Context, Result};
use audit::{now_ts, AuditEvent, AuditLog};
use base64::Engine;
use clap::Parser;
use common::{framing, Request, Response};
use display::SharedDisplay;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

/// Last absolute pointer position from `MouseMove` (shared across connections).
type CursorState = Arc<tokio::sync::Mutex<Option<(f64, f64)>>>;
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tokens::{AuthInfo, TokenStore};

#[derive(Parser, Debug)]
#[command(name = "gdrd", about = "GNOME desktop remote control daemon")]
struct Args {
    /// Address to bind, e.g. 0.0.0.0:7337
    #[arg(long, default_value = "0.0.0.0:7337")]
    bind: String,

    /// Legacy single bearer token (env GDR_TOKEN).
    #[arg(long, env = "GDR_TOKEN")]
    token: Option<String>,

    #[arg(long, env = "GDR_TOKENS_PATH")]
    tokens_path: Option<PathBuf>,

    #[arg(long, env = "GDR_AUDIT_PATH")]
    audit_path: Option<PathBuf>,

    /// Monitor connector to capture, e.g. "eDP-1". Leave unset to auto-pick
    /// or create a platform virtual monitor when none exist.
    #[arg(long)]
    connector: Option<String>,

    /// Virtual / negotiated capture width (PipeWire caps offered to Mutter).
    #[arg(long, default_value_t = capture::DEFAULT_WIDTH)]
    width: i32,

    /// Virtual / negotiated capture height.
    #[arg(long, default_value_t = capture::DEFAULT_HEIGHT)]
    height: i32,

    #[arg(long, default_value = "~/.local/share/gdr/cert.pem")]
    cert_path: String,
    #[arg(long, default_value = "~/.local/share/gdr/key.pem")]
    key_path: String,

    #[arg(long)]
    seed_token: bool,

    /// Never open Mutter ScreenCast (control-plane only: Ping/Auth work;
    /// screenshots/input fail).
    #[arg(long)]
    no_display: bool,

    /// Open Mutter ScreenCast at gdrd startup (old always-on behavior).
    /// Useful on headless hosts so Meta-* exists before the first client.
    /// Env: `GDR_EAGER_DISPLAY=true` / `false` (clap bool; not `1`/`0`).
    #[arg(long, env = "GDR_EAGER_DISPLAY", default_value_t = false)]
    eager_display: bool,

    /// Seconds without screenshot/input before tearing down physical
    /// ScreenCast. Default 45. `0` = never idle-stop once started.
    /// Platform virtual monitors are never idle-stopped (see HEADLESS.md).
    #[arg(long, env = "GDR_DISPLAY_IDLE_SECS", default_value_t = 45)]
    display_idle_secs: u64,
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

#[tokio::main]
async fn main() -> Result<()> {
    // rustls 0.23 requires an explicit process-level crypto provider when
    // multiple backends could be linked; we enable the `ring` feature.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let tokens_path = args.tokens_path.unwrap_or_else(TokenStore::default_path);

    if args.seed_token {
        let plain = args
            .token
            .as_deref()
            .context("--seed-token requires --token / GDR_TOKEN")?;
        let id = TokenStore::ensure_initial_token(&tokens_path, plain, "initial-install")?;
        println!("seeded token id={id} path={}", tokens_path.display());
        return Ok(());
    }

    if args.token.is_none() && !tokens_path.exists() {
        anyhow::bail!(
            "no GDR_TOKEN and no tokens file at {}. \
             Set GDR_TOKEN or run deploy.sh / `gdrd --seed-token`.",
            tokens_path.display()
        );
    }

    capture::init()?;

    let cert_path = expand_home(&args.cert_path);
    let key_path = expand_home(&args.key_path);
    let generated = tls::load_or_generate(&cert_path, &key_path)?;
    tracing::info!("cert fingerprint={}", generated.fingerprint_sha256_hex);

    let tls_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![generated.cert_der],
            PrivateKeyDer::Pkcs8(generated.key_der),
        )
        .context("bad TLS cert/key")?;
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));

    let store = Arc::new(TokenStore::load(
        tokens_path.clone(),
        args.token.as_deref(),
    )?);
    let audit = Arc::new(AuditLog::open(
        args.audit_path.unwrap_or_else(AuditLog::default_path),
    ));

    // Display/ScreenCast: lazy by default — no broadcast until a client
    // actually needs screenshot/input; physical sessions idle-stop.
    let display: Option<SharedDisplay> = if args.no_display {
        tracing::warn!("--no-display: Mutter ScreenCast disabled");
        None
    } else {
        let mgr = display::DisplayManager::new(display::DisplayConfig {
            connector: args.connector.clone(),
            size: capture::CaptureSize {
                width: args.width,
                height: args.height,
            },
            idle_secs: args.display_idle_secs,
            keep_virtual: true,
        });
        let shared = Arc::new(tokio::sync::Mutex::new(mgr));
        if args.eager_display {
            let mut g = shared.lock().await;
            if let Err(e) = g.warm_start().await {
                tracing::error!("eager display start failed: {e:#}");
            }
        } else {
            tracing::info!(
                "display lazy (idle_stop={}s for physical monitors; first \
                 screenshot/input opens Mutter)",
                args.display_idle_secs
            );
        }
        // Periodic idle reap — authority for "not broadcasting 24/7".
        if args.display_idle_secs > 0 {
            let watch = shared.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    watch.lock().await.idle_reap().await;
                }
            });
        }
        Some(shared)
    };

    // Last absolute pointer from MouseMove — Mutter RD has no live query.
    let cursor: CursorState = Arc::new(tokio::sync::Mutex::new(None));

    // Window plane. One session-bus connection for the process; the
    // extension is looked up per call, so gdrd survives the shell (and the
    // extension) restarting under it.
    let window_plane: Option<Arc<windows::WindowPlane>> =
        match windows::WindowPlane::connect().await {
            Ok(plane) => {
                if plane.available().await {
                    tracing::info!("window plane ready (gdr-windows extension answering)");
                } else {
                    tracing::warn!("window plane: {}", windows::INSTALL_HINT);
                }
                Some(Arc::new(plane))
            }
            Err(e) => {
                tracing::warn!("no session bus for the window plane: {e:#}");
                None
            }
        };

    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!(
        "listening on {} (tokens={}, audit={})",
        args.bind,
        store.path().display(),
        audit.path().display()
    );

    loop {
        let (stream, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let store = store.clone();
        let audit = audit.clone();
        let display = display.clone();
        let cursor = cursor.clone();
        let window_plane = window_plane.clone();
        let peer_s = peer.to_string();

        tokio::spawn(async move {
            tracing::info!("connection from {peer_s}");
            let tls_stream = match acceptor.accept(stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("TLS handshake failed from {peer_s}: {e}");
                    audit.write(&AuditEvent {
                        ts: now_ts(),
                        event: "tls_fail",
                        peer: &peer_s,
                        token_id: None,
                        token_label: None,
                        request: None,
                        detail: Some(&e.to_string()),
                    });
                    return;
                }
            };
            if let Err(e) = handle_connection(
                tls_stream,
                &store,
                &audit,
                &peer_s,
                display.as_ref(),
                &cursor,
                window_plane.as_deref(),
            )
            .await
            {
                tracing::warn!("connection {peer_s} ended: {e}");
            }
        });
    }
}

async fn handle_connection(
    mut stream: tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    store: &TokenStore,
    audit: &AuditLog,
    peer: &str,
    display: Option<&SharedDisplay>,
    cursor: &CursorState,
    window_plane: Option<&windows::WindowPlane>,
) -> Result<()> {
    let auth_info = authenticate(&mut stream, store, audit, peer).await?;

    audit.write(&AuditEvent {
        ts: now_ts(),
        event: "auth_ok",
        peer,
        token_id: Some(&auth_info.id),
        token_label: Some(&auth_info.label),
        request: Some("Auth"),
        detail: Some(&auth_info.scopes.to_string()),
    });

    loop {
        let req = match framing::read_message::<_, Request>(&mut stream).await {
            Ok(r) => r,
            Err(_) => {
                audit.write(&AuditEvent {
                    ts: now_ts(),
                    event: "disconnect",
                    peer,
                    token_id: Some(&auth_info.id),
                    token_label: Some(&auth_info.label),
                    request: None,
                    detail: None,
                });
                return Ok(());
            }
        };

        let req_name = request_name(&req);
        if let Some(needed) = req.required_scope() {
            if !auth_info.scopes.contains(needed) {
                let msg = format!(
                    "permission denied: token '{}' lacks scope '{needed}' (has {})",
                    auth_info.label, auth_info.scopes
                );
                framing::write_message(
                    &mut stream,
                    &Response::Error {
                        message: msg.clone(),
                    },
                )
                .await?;
                audit.write(&AuditEvent {
                    ts: now_ts(),
                    event: "denied",
                    peer,
                    token_id: Some(&auth_info.id),
                    token_label: Some(&auth_info.label),
                    request: Some(req_name),
                    detail: Some(&msg),
                });
                continue;
            }
        }

        // Capture the detail before the request is consumed: an audit line
        // saying only "WindowAction" cannot answer "what closed my editor?".
        let detail = window_audit_detail(&req);

        let resp = match handle_request(req, display, cursor, window_plane).await {
            Ok(r) => r,
            Err(e) => Response::Error {
                message: e.to_string(),
            },
        };

        audit.write(&AuditEvent {
            ts: now_ts(),
            event: "request",
            peer,
            token_id: Some(&auth_info.id),
            token_label: Some(&auth_info.label),
            request: Some(req_name),
            detail: detail.as_deref(),
        });

        framing::write_message(&mut stream, &resp).await?;
    }
}

async fn authenticate(
    stream: &mut tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
    store: &TokenStore,
    audit: &AuditLog,
    peer: &str,
) -> Result<AuthInfo> {
    match framing::read_message::<_, Request>(stream).await? {
        Request::Auth { token } => match store.authenticate(&token) {
            Ok(info) => {
                framing::write_message(stream, &Response::AuthOk).await?;
                Ok(info)
            }
            Err(e) => {
                framing::write_message(stream, &Response::AuthFailed).await?;
                audit.write(&AuditEvent {
                    ts: now_ts(),
                    event: "auth_fail",
                    peer,
                    token_id: None,
                    token_label: None,
                    request: Some("Auth"),
                    detail: Some(&e.to_string()),
                });
                Err(e).context("auth failed")
            }
        },
        _ => {
            framing::write_message(stream, &Response::AuthFailed).await?;
            anyhow::bail!("first message was not Auth")
        }
    }
}

fn request_name(req: &Request) -> &'static str {
    match req {
        Request::Auth { .. } => "Auth",
        Request::Screenshot { .. } => "Screenshot",
        Request::CaptureFrame { .. } => "CaptureFrame",
        Request::MouseMove { .. } => "MouseMove",
        Request::MouseButton { .. } => "MouseButton",
        Request::MouseScroll { .. } => "MouseScroll",
        Request::KeyEvent { .. } => "KeyEvent",
        Request::TypeText { .. } => "TypeText",
        Request::GetCursor => "GetCursor",
        Request::ListWindows { .. } => "ListWindows",
        Request::WindowAction { .. } => "WindowAction",
        Request::LaunchApp { .. } => "LaunchApp",
        Request::ListApps { .. } => "ListApps",
        Request::WindowEvents { .. } => "WindowEvents",
        Request::Ping => "Ping",
    }
}

/// Detail string recorded in the audit log for window-plane requests.
///
/// The generic per-request audit line only names the request type, which is
/// useless for "who closed my editor?". Window operations name a specific
/// window, so record which one and what was done to it.
fn window_audit_detail(req: &Request) -> Option<String> {
    match req {
        Request::WindowAction { target, op } => Some(format!(
            "{} target[{}]{}",
            op.name(),
            target.describe(),
            if op.is_destructive() { " destructive" } else { "" }
        )),
        Request::LaunchApp { app_id } => Some(format!("launch {app_id}")),
        _ => None,
    }
}

async fn handle_request(
    req: Request,
    display: Option<&SharedDisplay>,
    cursor: &CursorState,
    window_plane: Option<&windows::WindowPlane>,
) -> Result<Response> {
    match req {
        Request::Auth { .. } => Ok(Response::Error {
            message: "already authenticated".into(),
        }),
        Request::Ping => Ok(Response::Pong),

        Request::GetCursor => {
            let pos = cursor.lock().await;
            match *pos {
                Some((x, y)) => Ok(Response::CursorPosition {
                    x,
                    y,
                    known: true,
                }),
                None => Ok(Response::CursorPosition {
                    x: 0.0,
                    y: 0.0,
                    known: false,
                }),
            }
        }

        Request::Screenshot { .. } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let mut guard = display.lock().await;
            let png = guard.capture_png().await?;
            let png_base64 = base64::engine::general_purpose::STANDARD.encode(&png);
            Ok(Response::Screenshot { png_base64 })
        }

        Request::CaptureFrame {
            region,
            max_width,
            max_height,
            max_long_edge,
            max_patches,
            patch_size,
            format,
            quality,
            settle,
            if_none_match,
        } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let started = std::time::Instant::now();
            let opts = capture::FrameOptions {
                region,
                max_width,
                max_height,
                max_long_edge,
                max_patches,
                patch_size,
                format,
                quality,
                settle,
            };
            let frame = {
                let mut guard = display.lock().await;
                guard.capture_frame(opts).await?
            };

            // Identical screen: reply with the hash only. Saves ~1500 visual
            // tokens and, more usefully, tells the agent its action had no
            // visible effect instead of leaving it to diff two images.
            let unchanged = if_none_match.as_deref() == Some(frame.hash.as_str());
            let data_base64 = if unchanged {
                String::new()
            } else {
                base64::engine::general_purpose::STANDARD.encode(&frame.data)
            };

            Ok(Response::Frame(Box::new(common::Frame {
                data_base64,
                format: frame.format,
                native_width: frame.native_width,
                native_height: frame.native_height,
                region: frame.region,
                image_width: frame.image_width,
                image_height: frame.image_height,
                hash: frame.hash,
                unchanged,
                settled: frame.settled,
                capture_ms: started.elapsed().as_millis() as u64,
            })))
        }

        Request::MouseMove { x, y } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let mut guard = display.lock().await;
            let s = guard.session().await?;
            s.rd_session
                .notify_pointer_motion_absolute(&s.stream_id, x, y)
                .await?;
            *cursor.lock().await = Some((x, y));
            Ok(Response::Ok)
        }

        Request::MouseButton { button, pressed } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let mut guard = display.lock().await;
            guard
                .session()
                .await?
                .rd_session
                .notify_pointer_button(button, pressed)
                .await?;
            Ok(Response::Ok)
        }

        Request::MouseScroll { dx, dy } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let mut guard = display.lock().await;
            guard
                .session()
                .await?
                .rd_session
                .notify_pointer_axis(dx, dy, 0)
                .await?;
            Ok(Response::Ok)
        }

        Request::KeyEvent { keycode, pressed } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let mut guard = display.lock().await;
            guard
                .session()
                .await?
                .rd_session
                .notify_keyboard_keycode(keycode, pressed)
                .await?;
            Ok(Response::Ok)
        }

        Request::ListWindows {
            include_skip_taskbar,
        } => {
            let plane = require_window_plane(window_plane)?;
            let (connector, size) = capture_context(display).await;
            let list = plane
                .list(connector.as_deref(), size, include_skip_taskbar)
                .await?;
            Ok(Response::Windows(Box::new(list)))
        }

        Request::WindowAction { target, op } => {
            let plane = require_window_plane(window_plane)?;
            let (connector, size) = capture_context(display).await;
            let list = plane.list(connector.as_deref(), size, true).await?;
            let window = target.resolve(&list.windows).map_err(|e| anyhow::anyhow!("{e}"))?;
            let id = window.id;
            let label = window.label();
            let (action, after) = plane.act(id, op).await?;
            tracing::info!("window {action}: id={id} {label}");
            Ok(Response::WindowActed {
                action,
                window: after.map(Box::new),
                detail: Some(label),
            })
        }

        Request::LaunchApp { app_id } => {
            let plane = require_window_plane(window_plane)?;
            let (id, name, was_running) = plane.launch(&app_id).await?;
            Ok(Response::AppLaunched {
                app_id: id,
                name,
                was_running,
            })
        }

        Request::ListApps { filter } => {
            let plane = require_window_plane(window_plane)?;
            let apps = plane.list_apps(filter.as_deref()).await?;
            Ok(Response::Apps { apps })
        }

        Request::WindowEvents {
            since,
            limit,
            wait_ms,
        } => {
            let plane = require_window_plane(window_plane)?;
            let (events, next_seq, dropped, reset) =
                plane.events(since, limit, wait_ms).await?;
            Ok(Response::WindowEvents {
                events,
                next_seq,
                dropped,
                reset,
            })
        }

        Request::TypeText { text } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let mut guard = display.lock().await;
            let s = guard.session().await?;
            const KEY_LEFTSHIFT: u32 = 42;
            for c in text.chars() {
                if let Some((code, needs_shift)) = common::keymap::ascii_to_evdev(c) {
                    if needs_shift {
                        s.rd_session
                            .notify_keyboard_keycode(KEY_LEFTSHIFT, true)
                            .await?;
                    }
                    s.rd_session.notify_keyboard_keycode(code, true).await?;
                    s.rd_session.notify_keyboard_keycode(code, false).await?;
                    if needs_shift {
                        s.rd_session
                            .notify_keyboard_keycode(KEY_LEFTSHIFT, false)
                            .await?;
                    }
                }
            }
            Ok(Response::Ok)
        }
    }
}

fn require_window_plane(plane: Option<&windows::WindowPlane>) -> Result<&windows::WindowPlane> {
    plane.ok_or_else(|| {
        anyhow::anyhow!(
            "gdrd has no session bus connection, so it cannot see windows at all. \
             It must run inside the graphical session (systemd --user), not as a \
             system service."
        )
    })
}

/// What gdrd is currently streaming, for logical → stream conversion.
///
/// Deliberately does *not* start ScreenCast: listing windows is metadata and
/// must not be the thing that begins broadcasting the desktop. Before the
/// first capture the size is unknown and windows come back without a
/// `stream_region`, which is honest — we genuinely cannot say where a crop
/// would land until the stream has negotiated.
async fn capture_context(
    display: Option<&SharedDisplay>,
) -> (Option<String>, Option<(u32, u32)>) {
    let connector = match display {
        Some(d) => d.lock().await.capture_connector(),
        None => None,
    };
    (connector, capture::stream_size())
}
