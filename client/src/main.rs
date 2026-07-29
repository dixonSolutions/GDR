mod config;
mod manage;
mod pinning_verifier;
mod remote_admin;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clap::{Parser, Subcommand};
use common::{framing, Request, Response};
use config::{HostProfile, ResolvedHost};
use pinning_verifier::PinnedFingerprintVerifier;
use rustls::pki_types::ServerName;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

/// evdev button codes
mod btn {
    pub const LEFT: i32 = 0x110;
    pub const RIGHT: i32 = 0x111;
    pub const MIDDLE: i32 = 0x112;
}

#[derive(Parser, Debug)]
#[command(
    name = "gdr",
    about = "Controller CLI for gdrd (GNOME desktop remote)"
)]
struct Cli {
    /// Device id, label, or alias from ~/.config/gdr/config.json
    #[arg(long, global = true, env = "GDR_HOST", visible_alias = "dev")]
    host: Option<String>,

    /// server[:port], e.g. 192.168.1.50:7337 (overrides profile address)
    #[arg(long, global = true, env = "GDR_ADDR")]
    addr: Option<String>,

    #[arg(long, global = true, env = "GDR_TOKEN")]
    token: Option<String>,

    /// Pin the server's cert fingerprint (from its first-run output).
    #[arg(long, global = true, env = "GDR_PIN")]
    pin: Option<String>,

    /// Emit structured JSON on stdout instead of human text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Take a screenshot.
    Screenshot {
        #[arg(short, long, default_value = "screenshot.png")]
        out: String,
    },
    /// Move the pointer, no click.
    Move { x: f64, y: f64 },
    /// Move + click in one call.
    Click {
        x: f64,
        y: f64,
        #[arg(default_value = "left")]
        button: String,
    },
    Key { keycode: u32 },
    Type { text: String },
    Ping,
    /// Agent loop mode: one JSON action per stdin line, one result per stdout line.
    Batch,

    /// Manage remembered host profiles (~/.config/gdr/config.json).
    Host {
        #[command(subcommand)]
        cmd: HostCmd,
    },

    /// Manage devices (preferred alias of `host` + label/alias/sudo/token helpers).
    Device {
        #[command(subcommand)]
        cmd: manage::DeviceCmd,
    },

    /// Local systemd --user gdrd service (system package).
    Service {
        #[command(subcommand)]
        cmd: manage::ServiceCmd,
    },

    /// MCP / Cursor coding-tool wiring.
    Mcp {
        #[command(subcommand)]
        cmd: manage::McpCmd,
    },

    /// Installed system package info / update.
    Pkg {
        #[command(subcommand)]
        cmd: manage::PkgCmd,
    },

    /// Manage auth tokens on a target (over SSH, admin plane).
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },

    /// Read the remote audit log (over SSH).
    Audit {
        /// Host profile name (or use --host / default).
        host: Option<String>,
        #[arg(long, default_value_t = 50)]
        lines: usize,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        token_id: Option<String>,
    },

    /// Print a stored sudo/user password for a host (or a clear "not set" message).
    /// Host may be a positional arg or the global `--host` flag.
    GetPassword {
        /// sudo | user
        kind: String,
        /// Host profile name (default_host if omitted).
        #[arg(value_name = "HOST")]
        name: Option<String>,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum HostCmd {
    /// Add or update a remembered host profile.
    Add {
        name: String,
        /// Host/IP for the TLS data plane. Use `local` / `localhost` for same-machine
        /// (no LAN/Tailscale IP). Mutually exclusive with `--local`.
        #[arg(long, conflicts_with = "local")]
        address: Option<String>,
        /// Same-machine profile: stores address as `local` (connects via 127.0.0.1).
        #[arg(long)]
        local: bool,
        #[arg(long, default_value_t = common::DEFAULT_PORT)]
        port: u16,
        #[arg(long)]
        token: String,
        #[arg(long)]
        pin: Option<String>,
        /// SSH target for admin ops, e.g. user@host (optional for `--local`)
        #[arg(long)]
        ssh: Option<String>,
        #[arg(long)]
        sudo_password: Option<String>,
        #[arg(long)]
        user_password: Option<String>,
        /// Prompt interactively for sudo password (not echoed).
        #[arg(long)]
        ask_sudo: bool,
        /// Prompt interactively for user password (not echoed).
        #[arg(long)]
        ask_user: bool,
        /// Make this the default host.
        #[arg(long)]
        default: bool,
        /// Human-friendly name (also matched by --host / MCP dev=).
        #[arg(long)]
        label: Option<String>,
        #[arg(long = "alias")]
        aliases: Vec<String>,
    },
    List,
    Show { name: String },
    Remove { name: String },
    /// Set default_host.
    Default { name: String },
}

#[derive(Subcommand, Debug, Clone)]
enum TokenCmd {
    Create {
        /// Host profile (uses its ssh target).
        host: String,
        #[arg(long)]
        label: String,
        #[arg(long, default_value = "all")]
        scope: String,
        #[arg(long, default_value = "never")]
        expires: String,
    },
    List { host: String },
    Revoke { host: String, id: String },
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum BatchAction {
    Screenshot,
    Move {
        x: f64,
        y: f64,
    },
    Click {
        x: f64,
        y: f64,
        #[serde(default = "default_button")]
        button: String,
    },
    Key {
        keycode: u32,
    },
    Type {
        text: String,
    },
    Ping,
}

fn default_button() -> String {
    "left".to_string()
}

#[derive(Serialize)]
struct JsonResult<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screenshot_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screenshot_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_id: Option<String>,
}

fn button_code(button: &str) -> Result<i32> {
    Ok(match button {
        "left" => btn::LEFT,
        "right" => btn::RIGHT,
        "middle" => btn::MIDDLE,
        other => anyhow::bail!("unknown button '{other}', use left/right/middle"),
    })
}

fn resolve_from_cli(cli: &Cli) -> Result<ResolvedHost> {
    let cfg = config::load()?;
    config::resolve(
        &cfg,
        cli.host.as_deref(),
        cli.addr.as_deref(),
        cli.token.as_deref(),
        cli.pin.as_deref(),
    )
}

async fn connect(host: &ResolvedHost) -> Result<TlsStream<TcpStream>> {
    // Install ring crypto provider once (rustls 0.23).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let verifier = Arc::new(PinnedFingerprintVerifier::new(host.pin.clone()));
    let tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));

    let addr = host.addr_port();
    let tcp = TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    let server_name = ServerName::try_from("gdrd")?.to_owned();
    let mut stream = connector.connect(server_name, tcp).await?;

    framing::write_message(
        &mut stream,
        &Request::Auth {
            token: host.token.clone(),
        },
    )
    .await?;
    match framing::read_message::<_, Response>(&mut stream).await? {
        Response::AuthOk | Response::AuthOkScoped { .. } => {}
        other => anyhow::bail!("auth failed: {other:?}"),
    }
    Ok(stream)
}

async fn run_request(stream: &mut TlsStream<TcpStream>, req: Request) -> Result<Response> {
    framing::write_message(stream, &req).await?;
    Ok(framing::read_message::<_, Response>(stream).await?)
}

async fn click(
    stream: &mut TlsStream<TcpStream>,
    x: f64,
    y: f64,
    button: &str,
) -> Result<Response> {
    let code = button_code(button)?;
    run_request(stream, Request::MouseMove { x, y }).await?;
    run_request(
        stream,
        Request::MouseButton {
            button: code,
            pressed: true,
        },
    )
    .await?;
    run_request(
        stream,
        Request::MouseButton {
            button: code,
            pressed: false,
        },
    )
    .await
}

fn print_result(json: bool, result: &JsonResult, human_ok: &str) {
    if json {
        println!("{}", serde_json::to_string(result).unwrap());
    } else if result.ok {
        println!("{human_ok}");
    } else {
        eprintln!("error: {}", result.error.as_deref().unwrap_or("unknown"));
    }
}

fn response_to_result<'a>(
    resp: &Response,
    out_path: Option<&'a str>,
    include_b64: bool,
) -> Result<JsonResult<'a>> {
    Ok(match resp {
        Response::Screenshot { png_base64 } => {
            if let Some(path) = out_path {
                let bytes = B64
                    .decode(png_base64)
                    .context("server returned invalid base64")?;
                std::fs::write(path, &bytes)?;
            }
            JsonResult {
                ok: true,
                error: None,
                screenshot_path: out_path,
                screenshot_base64: if include_b64 {
                    Some(png_base64.clone())
                } else {
                    None
                },
                note: None,
                password: None,
                token: None,
                token_id: None,
            }
        }
        Response::Ok => JsonResult {
            ok: true,
            error: None,
            screenshot_path: None,
            screenshot_base64: None,
            note: None,
            password: None,
            token: None,
            token_id: None,
        },
        Response::Pong => JsonResult {
            ok: true,
            error: None,
            screenshot_path: None,
            screenshot_base64: None,
            note: Some("pong"),
            password: None,
            token: None,
            token_id: None,
        },
        Response::Error { message } => JsonResult {
            ok: false,
            error: Some(message.clone()),
            screenshot_path: None,
            screenshot_base64: None,
            note: None,
            password: None,
            token: None,
            token_id: None,
        },
        other => JsonResult {
            ok: false,
            error: Some(format!("unexpected response: {other:?}")),
            screenshot_path: None,
            screenshot_base64: None,
            note: None,
            password: None,
            token: None,
            token_id: None,
        },
    })
}

fn ssh_target_for(cfg: &config::Config, host_name: &str) -> Result<String> {
    let name = config::find_device_name(cfg, host_name)
        .with_context(|| format!("unknown device '{host_name}'"))?;
    let p = cfg
        .hosts
        .get(&name)
        .with_context(|| format!("unknown host '{name}'"))?;
    Ok(p.ssh_target())
}

fn run_host_cmd(cmd: HostCmd, json: bool) -> Result<()> {
    let mut cfg = config::load()?;
    match cmd {
        HostCmd::Add {
            name,
            address,
            local,
            port,
            token,
            pin,
            ssh,
            mut sudo_password,
            mut user_password,
            ask_sudo,
            ask_user,
            default,
            label,
            aliases,
        } => {
            let address = if local {
                // Stored as localhost so even older resolvers (no alias map) work.
                "localhost".to_string()
            } else {
                address.context("pass --address <host> or --local for same-machine")?
            };
            if ask_sudo {
                sudo_password = Some(rpassword::prompt_password("sudo password: ")?);
            }
            if ask_user {
                user_password = Some(rpassword::prompt_password("user password: ")?);
            }
            let profile = HostProfile {
                address: address.clone(),
                port,
                token,
                pin,
                ssh,
                sudo_password,
                user_password,
                label,
                aliases,
            };
            config::upsert_host(&mut cfg, &name, profile);
            if default {
                cfg.default_host = Some(name.clone());
            }
            config::save(&cfg)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "host": name,
                        "address": address,
                        "same_machine": config::is_loopback_alias(&address),
                        "path": config::config_path()?.display().to_string()
                    })
                );
            } else {
                let where_ = if config::is_loopback_alias(&address) {
                    "same machine (loopback)"
                } else {
                    address.as_str()
                };
                println!(
                    "saved host '{name}' → {where_} in {}",
                    config::config_path()?.display()
                );
            }
        }
        HostCmd::List => {
            if json {
                println!("{}", serde_json::to_string_pretty(&cfg)?);
            } else if cfg.hosts.is_empty() {
                println!("(no hosts saved)");
            } else {
                for (name, p) in &cfg.hosts {
                    let def = if cfg.default_host.as_deref() == Some(name.as_str()) {
                        " (default)"
                    } else {
                        ""
                    };
                    let pin = p.pin.as_deref().unwrap_or("-");
                    let sudo = if p.sudo_password.as_ref().is_some_and(|s| !s.is_empty()) {
                        "sudo=set"
                    } else {
                        "sudo=-"
                    };
                    let user = if p.user_password.as_ref().is_some_and(|s| !s.is_empty()) {
                        "user=set"
                    } else {
                        "user=-"
                    };
                    let endpoint = if config::is_loopback_alias(&p.address) {
                        format!("{} → {}", p.address, p.addr_port())
                    } else {
                        p.addr_port()
                    };
                    println!(
                        "{name}{def}  {endpoint}  pin={}  ssh={}  {sudo} {user}",
                        &pin[..pin.len().min(12)],
                        p.ssh_target()
                    );
                }
            }
        }
        HostCmd::Show { name } => {
            let p = cfg
                .hosts
                .get(&name)
                .with_context(|| format!("unknown host '{name}'"))?;
            // Never dump passwords in human mode unless --json (agent asked).
            if json {
                println!("{}", serde_json::to_string_pretty(p)?);
            } else {
                println!("name:    {name}");
                println!("address: {}:{}", p.address, p.port);
                println!("token:   {}…", &p.token[..p.token.len().min(8)]);
                println!("pin:     {}", p.pin.as_deref().unwrap_or("(none)"));
                println!("ssh:     {}", p.ssh_target());
                println!(
                    "sudo_password: {}",
                    if p.sudo_password.as_ref().is_some_and(|s| !s.is_empty()) {
                        "(set)"
                    } else {
                        "(not set)"
                    }
                );
                println!(
                    "user_password: {}",
                    if p.user_password.as_ref().is_some_and(|s| !s.is_empty()) {
                        "(set)"
                    } else {
                        "(not set)"
                    }
                );
            }
        }
        HostCmd::Remove { name } => {
            if cfg.hosts.remove(&name).is_none() {
                anyhow::bail!("unknown host '{name}'");
            }
            if cfg.default_host.as_deref() == Some(name.as_str()) {
                cfg.default_host = cfg.hosts.keys().next().cloned();
            }
            config::save(&cfg)?;
            println!("removed host '{name}'");
        }
        HostCmd::Default { name } => {
            if !cfg.hosts.contains_key(&name) {
                anyhow::bail!("unknown host '{name}'");
            }
            cfg.default_host = Some(name.clone());
            config::save(&cfg)?;
            println!("default_host = {name}");
        }
    }
    Ok(())
}

fn run_token_cmd(cmd: TokenCmd, json: bool) -> Result<()> {
    let cfg = config::load()?;
    match cmd {
        TokenCmd::Create {
            host,
            label,
            scope,
            expires,
        } => {
            let ssh = ssh_target_for(&cfg, &host)?;
            let (id, plaintext) = remote_admin::create_token(&ssh, &label, &scope, &expires)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&JsonResult {
                        ok: true,
                        error: None,
                        screenshot_path: None,
                        screenshot_base64: None,
                        note: Some("token plaintext shown once — save it now"),
                        password: None,
                        token: Some(plaintext),
                        token_id: Some(id),
                    })?
                );
            } else {
                println!("created token id={id}");
                println!("scopes={scope}  expires={expires}  label={label}");
                println!();
                println!("PLAINTEXT (shown once — save to config / MCP now):");
                println!("{plaintext}");
            }
        }
        TokenCmd::List { host } => {
            let ssh = ssh_target_for(&cfg, &host)?;
            let tokens = remote_admin::list_tokens(&ssh)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tokens)?);
            } else if tokens.is_empty() {
                println!("(no tokens on {host})");
            } else {
                for t in tokens {
                    let status = if t.revoked { "REVOKED" } else { "active" };
                    println!(
                        "{}  {:20}  scopes={}  expires={}  last_used={}  [{status}]",
                        t.id,
                        t.label,
                        t.scopes.join(","),
                        t.expires_at.as_deref().unwrap_or("never"),
                        t.last_used_at.as_deref().unwrap_or("-"),
                    );
                }
            }
        }
        TokenCmd::Revoke { host, id } => {
            let ssh = ssh_target_for(&cfg, &host)?;
            remote_admin::revoke_token(&ssh, &id)?;
            if json {
                println!("{}", serde_json::json!({"ok": true, "revoked": id}));
            } else {
                println!("revoked {id} on {host} (existing connections keep working until disconnect)");
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let json = cli.json;

    match &cli.cmd {
        Command::Host { cmd } => return run_host_cmd(cmd.clone(), json),
        Command::Device { cmd } => return manage::run_device(cmd.clone(), json),
        Command::Service { cmd } => return manage::run_service(cmd.clone()),
        Command::Mcp { cmd } => return manage::run_mcp(cmd.clone()),
        Command::Pkg { cmd } => return manage::run_pkg(cmd.clone()),
        Command::Token { cmd } => return run_token_cmd(cmd.clone(), json),
        Command::Audit {
            host,
            lines,
            since,
            token_id,
        } => {
            let cfg = config::load()?;
            let name = host
                .clone()
                .or_else(|| cli.host.clone())
                .or_else(|| cfg.default_host.clone())
                .context("pass a host name or set default_host")?;
            let ssh = ssh_target_for(&cfg, &name)?;
            let out = remote_admin::audit_tail(&ssh, *lines, since.as_deref(), token_id.as_deref())?;
            println!("{out}");
            return Ok(());
        }
        Command::GetPassword { kind, name } => {
            let cfg = config::load()?;
            let msg = config::password_message(&cfg, name.as_deref().or(cli.host.as_deref()), kind);
            let is_error = msg.starts_with("No ") || msg.starts_with("Unknown");
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&JsonResult {
                        ok: !is_error,
                        error: if is_error { Some(msg.clone()) } else { None },
                        screenshot_path: None,
                        screenshot_base64: None,
                        note: None,
                        password: if is_error { None } else { Some(msg) },
                        token: None,
                        token_id: None,
                    })?
                );
            } else {
                println!("{msg}");
            }
            if is_error {
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }

    // Live control-plane commands need a resolved host.
    let host = resolve_from_cli(&cli)?;

    if let Command::Batch = &cli.cmd {
        let mut stream = connect(&host).await?;
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let result = match serde_json::from_str::<BatchAction>(line) {
                Ok(action) => {
                    let resp: Response = match action {
                        BatchAction::Screenshot => {
                            run_request(&mut stream, Request::Screenshot { connector: None })
                                .await
                                .unwrap_or_else(|e| Response::Error {
                                    message: e.to_string(),
                                })
                        }
                        BatchAction::Move { x, y } => {
                            run_request(&mut stream, Request::MouseMove { x, y })
                                .await
                                .unwrap_or_else(|e| Response::Error {
                                    message: e.to_string(),
                                })
                        }
                        BatchAction::Key { keycode } => {
                            let _ = run_request(
                                &mut stream,
                                Request::KeyEvent {
                                    keycode,
                                    pressed: true,
                                },
                            )
                            .await;
                            run_request(
                                &mut stream,
                                Request::KeyEvent {
                                    keycode,
                                    pressed: false,
                                },
                            )
                            .await
                            .unwrap_or_else(|e| Response::Error {
                                message: e.to_string(),
                            })
                        }
                        BatchAction::Type { text } => {
                            run_request(&mut stream, Request::TypeText { text })
                                .await
                                .unwrap_or_else(|e| Response::Error {
                                    message: e.to_string(),
                                })
                        }
                        BatchAction::Ping => run_request(&mut stream, Request::Ping)
                            .await
                            .unwrap_or_else(|e| Response::Error {
                                message: e.to_string(),
                            }),
                        BatchAction::Click { x, y, button } => click(&mut stream, x, y, &button)
                            .await
                            .unwrap_or_else(|e| Response::Error {
                                message: e.to_string(),
                            }),
                    };
                    response_to_result(&resp, None, true)
                }
                Err(e) => Ok(JsonResult {
                    ok: false,
                    error: Some(format!("bad action: {e}")),
                    screenshot_path: None,
                    screenshot_base64: None,
                    note: None,
                    password: None,
                    token: None,
                    token_id: None,
                }),
            };
            let result = result.unwrap_or_else(|e| JsonResult {
                ok: false,
                error: Some(e.to_string()),
                screenshot_path: None,
                screenshot_base64: None,
                note: None,
                password: None,
                token: None,
                token_id: None,
            });
            println!("{}", serde_json::to_string(&result)?);
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
        return Ok(());
    }

    let mut stream = connect(&host).await?;

    let (resp, out_path): (Response, Option<String>) = match &cli.cmd {
        Command::Ping => (run_request(&mut stream, Request::Ping).await?, None),
        Command::Screenshot { out } => (
            run_request(&mut stream, Request::Screenshot { connector: None }).await?,
            Some(out.clone()),
        ),
        Command::Move { x, y } => (
            run_request(
                &mut stream,
                Request::MouseMove {
                    x: *x,
                    y: *y,
                },
            )
            .await?,
            None,
        ),
        Command::Click { x, y, button } => (click(&mut stream, *x, *y, button).await?, None),
        Command::Key { keycode } => {
            run_request(
                &mut stream,
                Request::KeyEvent {
                    keycode: *keycode,
                    pressed: true,
                },
            )
            .await?;
            (
                run_request(
                    &mut stream,
                    Request::KeyEvent {
                        keycode: *keycode,
                        pressed: false,
                    },
                )
                .await?,
                None,
            )
        }
        Command::Type { text } => (
            run_request(
                &mut stream,
                Request::TypeText {
                    text: text.clone(),
                },
            )
            .await?,
            None,
        ),
        Command::Batch
        | Command::Host { .. }
        | Command::Device { .. }
        | Command::Service { .. }
        | Command::Mcp { .. }
        | Command::Pkg { .. }
        | Command::Token { .. }
        | Command::Audit { .. }
        | Command::GetPassword { .. } => unreachable!(),
    };

    let result = response_to_result(&resp, out_path.as_deref(), json)?;
    let human_ok = match (&cli.cmd, &out_path) {
        (Command::Screenshot { .. }, Some(p)) => format!("saved screenshot to {p}"),
        _ => "ok".to_string(),
    };
    print_result(json, &result, &human_ok);
    if !result.ok {
        std::process::exit(1);
    }
    Ok(())
}

