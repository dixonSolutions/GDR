mod audit;
mod capture;
mod mutter_dbus;
mod tls;
mod tokens;

use anyhow::{Context, Result};
use audit::{now_ts, AuditEvent, AuditLog};
use base64::Engine;
use clap::Parser;
use common::{framing, Request, Response};
use mutter_dbus::MutterSession;
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

    /// Legacy single bearer token (env GDR_TOKEN). Still accepted for
    /// backwards compatibility; prefer tokens.json managed via
    /// `gdr token create` / deploy.sh. When both are present, tokens.json
    /// entries are checked first, then this env token (scope=all).
    #[arg(long, env = "GDR_TOKEN")]
    token: Option<String>,

    /// Path to the multi-token store (hashes only).
    #[arg(long, env = "GDR_TOKENS_PATH")]
    tokens_path: Option<PathBuf>,

    /// Path to the JSON-lines audit log.
    #[arg(long, env = "GDR_AUDIT_PATH")]
    audit_path: Option<PathBuf>,

    /// Monitor connector to capture, e.g. "eDP-1". Leave unset to auto-pick.
    #[arg(long)]
    connector: Option<String>,

    #[arg(long, default_value = "~/.local/share/gdr/cert.pem")]
    cert_path: String,
    #[arg(long, default_value = "~/.local/share/gdr/key.pem")]
    key_path: String,

    /// If set, write/ensure an initial tokens.json entry for --token /
    /// GDR_TOKEN and exit. Used by deploy.sh so the hashed store is
    /// populated without a separate admin step.
    #[arg(long)]
    seed_token: bool,
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

    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!(
        "listening on {} (tokens={}, audit={})",
        args.bind,
        store.path().display(),
        audit.path().display()
    );

    let connector = Arc::new(args.connector);

    loop {
        let (stream, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let store = store.clone();
        let audit = audit.clone();
        let connector = connector.clone();
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
                handle_connection(tls_stream, &store, &audit, &peer_s, connector.as_deref()).await
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
    connector: Option<&str>,
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

    let mut session: Option<MutterSession> = None;

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

        let resp = match handle_request(req, &mut session, connector).await {
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
                // Wire-compat: current clients/MCP expect `AuthOk`.
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

async fn ensure_session<'a>(
    session: &'a mut Option<MutterSession>,
    connector: Option<&str>,
) -> Result<&'a MutterSession> {
    if session.is_none() {
        *session = Some(MutterSession::open(connector).await?);
    }
    Ok(session.as_ref().unwrap())
}

async fn handle_request(
    req: Request,
    session: &mut Option<MutterSession>,
    connector: Option<&str>,
) -> Result<Response> {
    match req {
        Request::Auth { .. } => Ok(Response::Error {
            message: "already authenticated".into(),
        }),
        Request::Ping => Ok(Response::Pong),

        Request::Screenshot { connector: req_conn } => {
            let conn = req_conn.as_deref().or(connector);
            let s = ensure_session(session, conn).await?;
            let node_id = s.wait_for_pipewire_node().await?;
            let png = tokio::task::spawn_blocking(move || capture::capture_single_frame_png(node_id))
                .await??;
            let png_base64 = base64::engine::general_purpose::STANDARD.encode(&png);
            Ok(Response::Screenshot { png_base64 })
        }

        Request::MouseMove { x, y } => {
            let s = ensure_session(session, connector).await?;
            s.rd_session
                .notify_pointer_motion_absolute(&s.stream_id, x, y)
                .await?;
            Ok(Response::Ok)
        }

        Request::MouseButton { button, pressed } => {
            let s = ensure_session(session, connector).await?;
            s.rd_session.notify_pointer_button(button, pressed).await?;
            Ok(Response::Ok)
        }

        Request::MouseScroll { dx, dy } => {
            let s = ensure_session(session, connector).await?;
            s.rd_session.notify_pointer_axis(dx, dy, 0).await?;
            Ok(Response::Ok)
        }

        Request::KeyEvent { keycode, pressed } => {
            let s = ensure_session(session, connector).await?;
            s.rd_session
                .notify_keyboard_keycode(keycode, pressed)
                .await?;
            Ok(Response::Ok)
        }

        Request::TypeText { text } => {
            let s = ensure_session(session, connector).await?;
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
