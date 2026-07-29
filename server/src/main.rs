mod audit;
mod capture;
mod display;
mod mutter_dbus;
mod tls;
mod tokens;

use anyhow::{Context, Result};
use audit::{now_ts, AuditEvent, AuditLog};
use base64::Engine;
use clap::Parser;
use common::{framing, Request, Response};
use display::SharedDisplay;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
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

    /// Skip opening the Mutter display provider at startup (control-plane
    /// only; screenshots/input will fail until a provider is available).
    #[arg(long)]
    no_display: bool,
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

    // Long-lived display: physical monitor if present, else we *become*
    // the display via RecordVirtual is-platform + PipeWire negotiation.
    let display: Option<SharedDisplay> = if args.no_display {
        tracing::warn!("--no-display: Mutter session not opened at startup");
        None
    } else {
        match display::DisplayProvider::start(
            args.connector.as_deref(),
            capture::CaptureSize {
                width: args.width,
                height: args.height,
            },
        )
        .await
        {
            Ok(p) => {
                tracing::info!(
                    "display provider ready (virtual={}, node={}, {}x{})",
                    p.is_virtual(),
                    p.node_id(),
                    p.size().width,
                    p.size().height
                );
                Some(Arc::new(tokio::sync::Mutex::new(p)))
            }
            Err(e) => {
                // Still serve the control plane; first screenshot will error
                // with a clear message. Avoid taking down gdrd if Mutter
                // briefly isn't ready at boot.
                tracing::error!("display provider failed to start: {e:#}");
                None
            }
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
            if let Err(e) =
                handle_connection(tls_stream, &store, &audit, &peer_s, display.as_ref()).await
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

        let resp = match handle_request(req, display).await {
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
            detail: None,
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
        Request::MouseMove { .. } => "MouseMove",
        Request::MouseButton { .. } => "MouseButton",
        Request::MouseScroll { .. } => "MouseScroll",
        Request::KeyEvent { .. } => "KeyEvent",
        Request::TypeText { .. } => "TypeText",
        Request::Ping => "Ping",
    }
}

async fn handle_request(req: Request, display: Option<&SharedDisplay>) -> Result<Response> {
    match req {
        Request::Auth { .. } => Ok(Response::Error {
            message: "already authenticated".into(),
        }),
        Request::Ping => Ok(Response::Pong),

        Request::Screenshot { .. } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let guard = display.lock().await;
            let png = guard.capture_png().await?;
            let png_base64 = base64::engine::general_purpose::STANDARD.encode(&png);
            Ok(Response::Screenshot { png_base64 })
        }

        Request::MouseMove { x, y } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let guard = display.lock().await;
            let s = guard.session();
            s.rd_session
                .notify_pointer_motion_absolute(&s.stream_id, x, y)
                .await?;
            Ok(Response::Ok)
        }

        Request::MouseButton { button, pressed } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let guard = display.lock().await;
            guard
                .session()
                .rd_session
                .notify_pointer_button(button, pressed)
                .await?;
            Ok(Response::Ok)
        }

        Request::MouseScroll { dx, dy } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let guard = display.lock().await;
            guard
                .session()
                .rd_session
                .notify_pointer_axis(dx, dy, 0)
                .await?;
            Ok(Response::Ok)
        }

        Request::KeyEvent { keycode, pressed } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let guard = display.lock().await;
            guard
                .session()
                .rd_session
                .notify_keyboard_keycode(keycode, pressed)
                .await?;
            Ok(Response::Ok)
        }

        Request::TypeText { text } => {
            let display = display
                .ok_or_else(|| anyhow::anyhow!("display provider not available"))?;
            let guard = display.lock().await;
            let s = guard.session();
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
