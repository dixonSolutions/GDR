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

    /// List windows on the target desktop (needs the gdr-windows extension).
    Windows {
        /// Only windows whose app id, WM_CLASS or title contains this.
        #[arg(long)]
        filter: Option<String>,
        /// Include docks, panels and notification popups.
        #[arg(long)]
        all: bool,
    },

    /// Act on one window: activate, minimize, close, move, resize, …
    Window {
        /// activate | focus | raise | minimize | unminimize | maximize |
        /// unmaximize | fullscreen | unfullscreen | above | unabove |
        /// stick | unstick | close | move | resize | move_resize | workspace
        action: String,
        /// Exact window id from `gdr windows`.
        #[arg(long)]
        id: Option<u64>,
        #[arg(long)]
        app_id: Option<String>,
        #[arg(long)]
        wm_class: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Act on whatever currently has focus.
        #[arg(long)]
        focused: bool,
        #[arg(long)]
        x: Option<i32>,
        #[arg(long)]
        y: Option<i32>,
        #[arg(long)]
        width: Option<i32>,
        #[arg(long)]
        height: Option<i32>,
        /// Workspace index for `workspace`.
        #[arg(long)]
        index: Option<i32>,
    },

    /// Poll window open/close/focus events.
    WindowEvents {
        /// Resume after this sequence number.
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        /// Block up to this many ms waiting for the first event.
        #[arg(long, default_value_t = 0)]
        wait_ms: u64,
    },

    /// Standing subscriptions: watch screen activity or window lifecycle.
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },

    /// Shorthand for `gdr hook list`.
    Hooks,

    /// List installed apps, or launch/raise one.
    App {
        /// Desktop-file id to launch. Omit to list.
        app_id: Option<String>,
        #[arg(long)]
        filter: Option<String>,
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

/// Subscription hooks. `screen` and `window` create one; the rest manage
/// what is already there.
#[derive(Subcommand, Debug, Clone)]
enum HookCmd {
    /// Watch the screen — or one window — for activity, reported as a circle.
    Screen {
        /// Watch only this window (tracks it as it moves).
        #[arg(long)]
        id: Option<u64>,
        #[arg(long)]
        app_id: Option<String>,
        #[arg(long)]
        wm_class: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Watch a fixed rectangle of stream pixels: x,y,width,height.
        #[arg(long, value_name = "X,Y,W,H")]
        region: Option<String>,
        /// Quiet period before a burst is reported, in ms.
        #[arg(long, default_value_t = common::hooks::ACTIVITY_BUFFER_MS)]
        buffer_ms: u64,
        /// Never report more often than this, in ms. 0 = off. For regions
        /// that repaint on a timer, where every repaint is its own burst.
        #[arg(long, default_value_t = 0)]
        min_interval_ms: u64,
        #[arg(long, default_value_t = common::hooks::ACTIVITY_POLL_MS)]
        poll_ms: u64,
        /// Report a still-moving burst anyway after this long. 0 disables.
        #[arg(long, default_value_t = common::hooks::ACTIVITY_MAX_BURST_MS)]
        max_burst_ms: u64,
        /// Per-cell luma delta that counts as change (1-255).
        #[arg(long, default_value_t = 12)]
        threshold: u8,
        /// Cells that must change before a sample counts as activity.
        #[arg(long, default_value_t = 1)]
        min_cells: u32,
        /// Diff grid resolution along the long edge.
        #[arg(long, default_value_t = 64)]
        grid: u32,
        /// Ignore bursts whose circle is bigger than this radius, in px.
        #[arg(long, default_value_t = 0)]
        max_radius: u32,
        #[arg(long)]
        label: Option<String>,
        /// Create it switched off.
        #[arg(long)]
        off: bool,
    },

    /// Watch windows opening, closing, resizing and more.
    Window {
        /// Comma-separated: opened,closed,resized,moved,retitled,focused,
        /// minimized,unminimized,workspace
        #[arg(long, default_value = "opened,closed,resized")]
        events: String,
        /// Watch only windows matching this selector.
        #[arg(long)]
        id: Option<u64>,
        #[arg(long)]
        app_id: Option<String>,
        #[arg(long)]
        wm_class: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Quiet period before a geometry change is reported, in ms.
        #[arg(long, default_value_t = common::hooks::WINDOW_BUFFER_MS)]
        buffer_ms: u64,
        #[arg(long, default_value_t = common::hooks::WINDOW_POLL_MS)]
        poll_ms: u64,
        #[arg(long, default_value_t = common::hooks::WINDOW_MAX_BURST_MS)]
        max_burst_ms: u64,
        /// Include docks, panels and notification popups.
        #[arg(long)]
        all: bool,
        /// Skip the /proc lookup for the owning process.
        #[arg(long)]
        no_process: bool,
        #[arg(long)]
        label: Option<String>,
        /// Create it switched off.
        #[arg(long)]
        off: bool,
    },

    /// Show every hook this token may see.
    List,
    /// Switch one on.
    On { id: String },
    /// Switch one off, keeping its config and buffered events.
    Off { id: String },
    /// Forget one, dropping its buffered events.
    Remove { id: String },
    /// Drain the hook journal.
    Events {
        /// Only this hook. Omit to drain every hook at once.
        #[arg(long)]
        id: Option<String>,
        /// Resume after this sequence number.
        #[arg(long, default_value_t = 0)]
        since: u64,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        /// Block up to this many ms for the first event.
        #[arg(long, default_value_t = 0)]
        wait_ms: u64,
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
        /// Device id / label / alias. Use `me` for this machine.
        /// Omit to default to this machine (same as `--host me`).
        /// (Named `device` so it does not shadow global `--host`.)
        #[arg(value_name = "HOST")]
        device: Option<String>,
        #[arg(long)]
        label: String,
        #[arg(long, default_value = "all")]
        scope: String,
        #[arg(long, default_value = "never")]
        expires: String,
    },
    /// List tokens gdrd accepts on a device (hashed store).
    /// Omit HOST to default to this machine (`me` → local).
    List {
        #[arg(value_name = "HOST")]
        device: Option<String>,
    },
    Revoke {
        /// Token id to revoke (e.g. tok_…).
        id: String,
        #[arg(value_name = "HOST")]
        device: Option<String>,
    },
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

/// Handle the window-plane subcommands, or `None` if `cmd` is not one.
///
/// These print their own output because a window listing has no useful
/// projection onto `JsonResult`, which exists to describe one screenshot or
/// one input event.
async fn run_window_command(
    stream: &mut TlsStream<TcpStream>,
    cmd: &Command,
    json: bool,
) -> Result<Option<()>> {
    let req = match cmd {
        Command::Windows { filter: _, all } => Request::ListWindows {
            include_skip_taskbar: *all,
        },
        Command::Window {
            action,
            id,
            app_id,
            wm_class,
            title,
            focused,
            x,
            y,
            width,
            height,
            index,
        } => {
            let target = common::WindowTarget {
                id: *id,
                app_id: app_id.clone(),
                wm_class: wm_class.clone(),
                title: title.clone(),
                pid: None,
                focused: *focused,
            };
            Request::WindowAction {
                target,
                op: parse_window_op(action, *x, *y, *width, *height, *index)?,
            }
        }
        Command::WindowEvents {
            since,
            limit,
            wait_ms,
        } => Request::WindowEvents {
            since: *since,
            limit: *limit,
            wait_ms: *wait_ms,
        },
        Command::App { app_id, filter } => match app_id {
            Some(id) => Request::LaunchApp { app_id: id.clone() },
            None => Request::ListApps {
                filter: filter.clone(),
            },
        },
        Command::Hooks => Request::HookList,
        Command::Hook { cmd } => hook_request(cmd)?,
        _ => return Ok(None),
    };

    let resp = run_request(stream, req).await?;
    print_window_response(cmd, &resp, json)?;
    if matches!(resp, Response::Error { .. }) {
        std::process::exit(1);
    }
    Ok(Some(()))
}

/// Turn a `gdr hook …` invocation into one protocol request.
fn hook_request(cmd: &HookCmd) -> Result<Request> {
    use common::hooks::{ActivitySpec, HookSpec, WindowHookSpec};

    fn target(
        id: Option<u64>,
        app_id: &Option<String>,
        wm_class: &Option<String>,
        title: &Option<String>,
    ) -> Option<common::WindowTarget> {
        if id.is_none() && app_id.is_none() && wm_class.is_none() && title.is_none() {
            return None;
        }
        Some(common::WindowTarget {
            id,
            app_id: app_id.clone(),
            wm_class: wm_class.clone(),
            title: title.clone(),
            pid: None,
            focused: false,
        })
    }

    Ok(match cmd {
        HookCmd::Screen {
            id,
            app_id,
            wm_class,
            title,
            region,
            buffer_ms,
            min_interval_ms,
            poll_ms,
            max_burst_ms,
            threshold,
            min_cells,
            grid,
            max_radius,
            label,
            off,
        } => {
            let region = match region {
                None => None,
                Some(spec) => {
                    let nums: Vec<u32> = spec
                        .split(',')
                        .map(|p| p.trim().parse::<u32>())
                        .collect::<Result<_, _>>()
                        .context("--region takes four numbers: x,y,width,height")?;
                    if nums.len() != 4 {
                        anyhow::bail!("--region takes four numbers: x,y,width,height");
                    }
                    Some(common::Region {
                        x: nums[0],
                        y: nums[1],
                        width: nums[2],
                        height: nums[3],
                    })
                }
            };
            Request::HookCreate {
                spec: HookSpec::Activity(ActivitySpec {
                    target: target(*id, app_id, wm_class, title),
                    region,
                    buffer_ms: *buffer_ms,
                    min_interval_ms: *min_interval_ms,
                    max_burst_ms: *max_burst_ms,
                    poll_ms: *poll_ms,
                    threshold: *threshold,
                    min_cells: *min_cells,
                    grid: *grid,
                    max_radius: *max_radius,
                }),
                label: label.clone(),
                enabled: !off,
            }
        }
        HookCmd::Window {
            events,
            id,
            app_id,
            wm_class,
            title,
            buffer_ms,
            poll_ms,
            max_burst_ms,
            all,
            no_process,
            label,
            off,
        } => Request::HookCreate {
            spec: HookSpec::Window(WindowHookSpec {
                target: target(*id, app_id, wm_class, title),
                events: events
                    .split(',')
                    .map(|e| e.trim().to_string())
                    .filter(|e| !e.is_empty())
                    .collect(),
                buffer_ms: *buffer_ms,
                max_burst_ms: *max_burst_ms,
                poll_ms: *poll_ms,
                include_skip_taskbar: *all,
                geometry_threshold: 2,
                include_process: !no_process,
            }),
            label: label.clone(),
            enabled: !off,
        },
        HookCmd::List => Request::HookList,
        HookCmd::On { id } => Request::HookUpdate {
            id: id.clone(),
            enabled: Some(true),
            label: None,
            spec: None,
        },
        HookCmd::Off { id } => Request::HookUpdate {
            id: id.clone(),
            enabled: Some(false),
            label: None,
            spec: None,
        },
        HookCmd::Remove { id } => Request::HookRemove { id: id.clone() },
        HookCmd::Events {
            id,
            since,
            limit,
            wait_ms,
        } => Request::HookPoll {
            id: id.clone(),
            since: *since,
            limit: *limit,
            wait_ms: *wait_ms,
        },
    })
}

fn parse_window_op(
    action: &str,
    x: Option<i32>,
    y: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
    index: Option<i32>,
) -> Result<common::WindowOp> {
    use common::WindowOp as Op;
    Ok(match action.trim().to_ascii_lowercase().as_str() {
        "activate" => Op::Activate,
        "focus" => Op::Focus,
        "raise" => Op::Raise,
        "minimize" => Op::Minimize,
        "unminimize" => Op::Unminimize,
        "maximize" => Op::Maximize,
        "unmaximize" => Op::Unmaximize,
        "fullscreen" => Op::Fullscreen,
        "unfullscreen" => Op::Unfullscreen,
        "above" => Op::Above,
        "unabove" => Op::Unabove,
        "stick" => Op::Stick,
        "unstick" => Op::Unstick,
        "close" => Op::Close,
        "move" => Op::Move {
            x: x.context("move needs --x")?,
            y: y.context("move needs --y")?,
        },
        "resize" => Op::Resize {
            width: width.context("resize needs --width")?,
            height: height.context("resize needs --height")?,
        },
        "move_resize" | "move-resize" => Op::MoveResize {
            x: x.context("move_resize needs --x")?,
            y: y.context("move_resize needs --y")?,
            width: width.context("move_resize needs --width")?,
            height: height.context("move_resize needs --height")?,
        },
        "workspace" => Op::Workspace {
            index: index.context("workspace needs --index")?,
        },
        other => anyhow::bail!("unknown window action '{other}' (see --help)"),
    })
}

fn print_window_response(cmd: &Command, resp: &Response, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(resp)?);
        return Ok(());
    }
    match resp {
        Response::Error { message } => eprintln!("error: {message}"),
        Response::Windows(list) => {
            let filter = match cmd {
                Command::Windows { filter, .. } => filter.as_deref(),
                _ => None,
            };
            let needle = filter.map(|f| f.to_ascii_lowercase());
            println!(
                "backend={} capturing={} workspace {}/{}",
                list.backend.as_str(),
                list.capture_connector.as_deref().unwrap_or("(unknown)"),
                list.active_workspace,
                list.n_workspaces
            );
            for w in &list.windows {
                if let Some(n) = &needle {
                    let hay = format!(
                        "{} {} {}",
                        w.app_id.as_deref().unwrap_or(""),
                        w.wm_class.as_deref().unwrap_or(""),
                        w.title.as_deref().unwrap_or("")
                    )
                    .to_ascii_lowercase();
                    if !hay.contains(n) {
                        continue;
                    }
                }
                println!(
                    "  {:>6}  ws{:<2} {:<28} {}{}",
                    w.id,
                    w.workspace.unwrap_or(-1),
                    w.app_id
                        .as_deref()
                        .or(w.wm_class.as_deref())
                        .unwrap_or("?"),
                    w.title.as_deref().unwrap_or(""),
                    match (w.focus, w.minimized, w.stream_region.is_some()) {
                        (true, _, _) => "  [focused]",
                        (_, true, _) => "  [minimized]",
                        (_, _, false) => "  [not on captured monitor]",
                        _ => "",
                    }
                );
            }
        }
        Response::WindowActed {
            action,
            window,
            detail,
        } => {
            println!(
                "{action} ok: {}",
                detail
                    .as_deref()
                    .or_else(|| window.as_ref().map(|w| w.title.as_deref().unwrap_or("?")))
                    .unwrap_or("?")
            );
        }
        Response::AppLaunched {
            app_id,
            name,
            was_running,
        } => println!(
            "{} {} ({})",
            if *was_running { "raised" } else { "launched" },
            name.as_deref().unwrap_or(app_id),
            app_id
        ),
        Response::Apps { apps } => {
            for a in apps {
                println!(
                    "  {:<48} {}{}",
                    a.app_id,
                    a.name,
                    if a.running { "  [running]" } else { "" }
                );
            }
            println!("{} apps", apps.len());
        }
        Response::WindowEvents {
            events,
            next_seq,
            dropped,
            reset,
        } => {
            for e in events {
                println!(
                    "  #{:<5} {} {:<12} {} {}",
                    e.seq,
                    e.at,
                    e.kind,
                    e.app_id.as_deref().or(e.wm_class.as_deref()).unwrap_or("?"),
                    e.title.as_deref().unwrap_or("")
                );
            }
            println!(
                "{} events, next_seq={next_seq}{}{}",
                events.len(),
                if *dropped { " (older events aged out)" } else { "" },
                if *reset { " (sequence restarted)" } else { "" }
            );
        }
        Response::Hook { hook } => print_hooks(std::slice::from_ref(hook.as_ref())),
        Response::Hooks { hooks } => print_hooks(hooks),
        Response::HookEvents(result) => {
            for e in &result.events {
                let what = match (&e.activity, &e.window) {
                    (Some(a), _) => format!(
                        "circle ({:.0},{:.0}) r={:.0}  {}x{} box  {}",
                        a.circle.x,
                        a.circle.y,
                        a.circle.radius,
                        a.bbox.width,
                        a.bbox.height,
                        if a.settled { "settled" } else { "STILL MOVING" }
                    ),
                    (_, Some(w)) => format!(
                        "{:<30} {}x{}+{}+{}  pid {}{}",
                        w.title.as_deref().unwrap_or(w.app_id.as_deref().unwrap_or("?")),
                        w.frame_rect.width,
                        w.frame_rect.height,
                        w.frame_rect.x,
                        w.frame_rect.y,
                        w.pid,
                        w.process
                            .as_ref()
                            .and_then(|p| p.comm.clone())
                            .map(|c| format!(" ({c})"))
                            .unwrap_or_default()
                    ),
                    _ => String::new(),
                };
                println!(
                    "  #{:<5} {} {:<12} {:<14} {}",
                    e.seq, e.at, e.hook_id, e.kind, what
                );
            }
            println!(
                "{} events, next_seq={}{}",
                result.events.len(),
                result.next_seq,
                if result.dropped {
                    " (older events aged out — poll more often)"
                } else {
                    ""
                }
            );
            print_hooks(&result.hooks);
        }
        other => println!("{other:?}"),
    }
    Ok(())
}

fn print_hooks(hooks: &[common::HookStatus]) {
    if hooks.is_empty() {
        println!("no hooks");
        return;
    }
    for h in hooks {
        println!(
            "  {:<12} {:<8} {:<9} scope={:<10} {}{}",
            h.id,
            if h.enabled { "on" } else { "off" },
            h.state.as_str(),
            h.required_scope,
            h.summary,
            h.label
                .as_deref()
                .map(|l| format!("  '{l}'"))
                .unwrap_or_default()
        );
        if let Some(err) = &h.last_error {
            println!("               ! {err}");
        }
        println!(
            "               {} events emitted, {} buffered{}",
            h.events_emitted,
            h.buffered,
            h.last_event_at
                .as_deref()
                .map(|t| format!(", last {t}"))
                .unwrap_or_default()
        );
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

/// Same-machine devices edit local `tokens.json`; remotes use SSH.
fn admin_target_for(cfg: &config::Config, host_name: &str) -> Result<remote_admin::AdminTarget> {
    let name = config::find_device_name(cfg, host_name).with_context(|| {
        format!(
            "unknown device '{host_name}'. Known: {}. \
             Tip: --host me  (this machine) or  gdr token list desktop",
            config::known_device_names(cfg).join(", ")
        )
    })?;
    let p = cfg
        .hosts
        .get(&name)
        .with_context(|| format!("unknown host '{name}'"))?;
    if config::is_loopback_alias(&p.address) {
        Ok(remote_admin::AdminTarget::Local)
    } else {
        Ok(remote_admin::AdminTarget::Ssh(p.ssh_target()))
    }
}

/// Positional HOST → global `--host`/`--dev` → this machine (`me`).
/// Returns (canonical device id, human query used, whether we defaulted).
fn resolve_token_host(
    cfg: &config::Config,
    positional: Option<&str>,
    global_host: Option<&str>,
) -> Result<(String, String, bool)> {
    if let Some(q) = positional.map(str::trim).filter(|s| !s.is_empty()) {
        let name = config::find_device_name(cfg, q).with_context(|| {
            format!(
                "unknown device '{q}'. Known: {}. Use --host me for this machine.",
                config::known_device_names(cfg).join(", ")
            )
        })?;
        return Ok((name, q.to_string(), false));
    }
    if let Some(q) = global_host.map(str::trim).filter(|s| !s.is_empty()) {
        let name = config::find_device_name(cfg, q).with_context(|| {
            format!(
                "unknown device '{q}'. Known: {}. Use --host me for this machine.",
                config::known_device_names(cfg).join(", ")
            )
        })?;
        return Ok((name, q.to_string(), false));
    }
    let name = config::find_same_machine_device(cfg).context(
        "no same-machine device configured. Add one:\n  \
         gdr device add me --local --token <TOKEN> --label \"home computer\"\n  \
         or: gdr token list --host desktop",
    )?;
    Ok((name, "me".into(), true))
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
                // Preserved across a re-add of the same id: the pinned window
                // belongs to the device, not to whoever last edited it.
                pinned_window: cfg
                    .hosts
                    .get(&name)
                    .and_then(|p| p.pinned_window.clone()),
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

fn run_token_cmd(cmd: TokenCmd, json: bool, global_host: Option<&str>) -> Result<()> {
    let cfg = config::load()?;
    match cmd {
        TokenCmd::Create {
            device,
            label,
            scope,
            expires,
        } => {
            let (name, query, defaulted) =
                resolve_token_host(&cfg, device.as_deref(), global_host)?;
            if defaulted && !json {
                eprintln!(
                    "defaulting to host me (this machine → device '{name}')"
                );
            }
            let target = admin_target_for(&cfg, &name)?;
            let (id, plaintext) =
                remote_admin::create_token(&target, &label, &scope, &expires)?;
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
                println!("created token id={id} on {name} (query={query})");
                println!("scopes={scope}  expires={expires}  label={label}");
                println!();
                println!("PLAINTEXT (shown once — save to config / MCP now):");
                println!("{plaintext}");
            }
        }
        TokenCmd::List { device } => {
            let (name, query, defaulted) =
                resolve_token_host(&cfg, device.as_deref(), global_host)?;
            if defaulted && !json {
                eprintln!(
                    "defaulting to host me (this machine → device '{name}')"
                );
            }
            let target = admin_target_for(&cfg, &name)?;
            let tokens = remote_admin::list_tokens(&target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tokens)?);
            } else if tokens.is_empty() {
                println!("(no tokens on {name})");
            } else {
                let where_ = match &target {
                    remote_admin::AdminTarget::Local => {
                        "~/.local/share/gdr/tokens.json (this machine)".to_string()
                    }
                    remote_admin::AdminTarget::Ssh(s) => format!("via ssh {s}"),
                };
                println!("# device={name}  query={query}  {where_}");
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
        TokenCmd::Revoke { device, id } => {
            let (name, _query, defaulted) =
                resolve_token_host(&cfg, device.as_deref(), global_host)?;
            if defaulted && !json {
                eprintln!(
                    "defaulting to host me (this machine → device '{name}')"
                );
            }
            let target = admin_target_for(&cfg, &name)?;
            remote_admin::revoke_token(&target, &id)?;
            if json {
                println!("{}", serde_json::json!({"ok": true, "revoked": id, "device": name}));
            } else {
                println!("revoked {id} on {name} (existing connections keep working until disconnect)");
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
        Command::Token { cmd } => {
            return run_token_cmd(cmd.clone(), json, cli.host.as_deref())
        }
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
            let target = admin_target_for(&cfg, &name)?;
            let out =
                remote_admin::audit_tail(&target, *lines, since.as_deref(), token_id.as_deref())?;
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

    // Window commands answer with structured data that does not fit the flat
    // JsonResult shape the pixel/input commands share, so they print and
    // return here rather than being flattened into it.
    if let Some(()) = run_window_command(&mut stream, &cli.cmd, json).await? {
        return Ok(());
    }

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
        | Command::GetPassword { .. }
        | Command::Windows { .. }
        | Command::Window { .. }
        | Command::WindowEvents { .. }
        | Command::Hook { .. }
        | Command::Hooks
        | Command::App { .. } => unreachable!(),
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

