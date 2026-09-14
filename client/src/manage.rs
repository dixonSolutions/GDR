//! System-package management CLI: devices, systemd user service, MCP/Cursor, package info.

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use std::path::PathBuf;
use std::process::Command;

use crate::config::{self, HostProfile};

#[derive(Subcommand, Debug, Clone)]
pub enum DeviceCmd {
    /// Add or update a device profile (token, pin, sudo, label, aliases).
    Add {
        /// Canonical id (e.g. home, desktop). Stable key in config.json.
        id: String,
        #[arg(long, conflicts_with = "local")]
        address: Option<String>,
        #[arg(long)]
        local: bool,
        #[arg(long, default_value_t = common::DEFAULT_PORT)]
        port: u16,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        pin: Option<String>,
        #[arg(long)]
        ssh: Option<String>,
        #[arg(long)]
        label: Option<String>,
        /// Repeatable: extra names that resolve to this device (e.g. "home computer").
        #[arg(long = "alias")]
        aliases: Vec<String>,
        #[arg(long)]
        sudo_password: Option<String>,
        #[arg(long)]
        user_password: Option<String>,
        #[arg(long)]
        ask_sudo: bool,
        #[arg(long)]
        ask_user: bool,
        #[arg(long)]
        ask_token: bool,
        #[arg(long)]
        default: bool,
    },
    List,
    Show { query: String },
    Remove { query: String },
    Default { query: String },
    /// Set / clear stored sudo password (plaintext, chmod 600 file).
    SetSudo {
        query: String,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        ask: bool,
        #[arg(long)]
        clear: bool,
    },
    /// Set / rotate auth token stored for this device.
    SetToken {
        query: String,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        ask: bool,
    },
    /// Set human label (e.g. "home computer").
    SetLabel { query: String, label: String },
    /// Add an alias that resolves to this device.
    AddAlias { query: String, alias: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ServiceCmd {
    Status,
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
    Logs {
        #[arg(long, default_value_t = 50)]
        lines: usize,
        #[arg(long)]
        follow: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum McpCmd {
    /// Wire Cursor global ~/.cursor/mcp.json for gdr.
    SetupCursor {
        /// Bind default device: gdr-mcp --dev "…"
        #[arg(long)]
        dev: Option<String>,
        /// One MCP server entry per device (gdr-<id>).
        #[arg(long)]
        per_device: bool,
        #[arg(long)]
        system: bool,
        #[arg(long)]
        repo: bool,
        #[arg(long)]
        restart: bool,
    },
    Status,
    /// Rebuild MCP (runs scripts/update-mcp.sh when available).
    Update {
        #[arg(long)]
        restart_cursor: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum PkgCmd {
    Version,
    Info,
    /// Rebuild + reinstall system package (scripts/update.sh --yes).
    Update {
        #[arg(long)]
        skip_deps: bool,
        /// Skip SSH remotes from config.json (local only).
        #[arg(long)]
        no_remotes: bool,
        /// Comma-separated machine ids (e.g. local,desktop).
        #[arg(long)]
        machines: Option<String>,
        /// Skip git force-pull on selected machines.
        #[arg(long)]
        no_git_pull: bool,
        /// Skip Cursor MCP process restart.
        #[arg(long)]
        no_restart_cursor: bool,
    },
    Paths,
}

pub fn run_device(cmd: DeviceCmd, json: bool) -> Result<()> {
    let mut cfg = config::load()?;
    match cmd {
        DeviceCmd::Add {
            id,
            address,
            local,
            port,
            mut token,
            pin,
            ssh,
            label,
            aliases,
            mut sudo_password,
            mut user_password,
            ask_sudo,
            ask_user,
            ask_token,
            default,
        } => {
            let address = if local {
                "localhost".to_string()
            } else {
                address.context("pass --address <host> or --local")?
            };
            if ask_token || token.as_ref().is_none_or(|t| t.is_empty()) {
                if ask_token || token.is_none() {
                    token = Some(rpassword::prompt_password("auth token: ")?);
                }
            }
            let token = token.context("pass --token or --ask-token")?;
            if ask_sudo {
                sudo_password = Some(rpassword::prompt_password("sudo password: ")?);
            }
            if ask_user {
                user_password = Some(rpassword::prompt_password("user password: ")?);
            }
            let prev = cfg.hosts.get(&id).cloned();
            // Read before the literal: `aliases` below moves `prev`.
            let pinned_window = prev.as_ref().and_then(|p| p.pinned_window.clone());
            let profile = HostProfile {
                address: address.clone(),
                port,
                token,
                pin: pin.or_else(|| prev.as_ref().and_then(|p| p.pin.clone())),
                ssh: ssh.or_else(|| prev.as_ref().and_then(|p| p.ssh.clone())),
                sudo_password: sudo_password
                    .or_else(|| prev.as_ref().and_then(|p| p.sudo_password.clone())),
                user_password: user_password
                    .or_else(|| prev.as_ref().and_then(|p| p.user_password.clone())),
                label: label.or_else(|| prev.as_ref().and_then(|p| p.label.clone())),
                aliases: if aliases.is_empty() {
                    prev.map(|p| p.aliases).unwrap_or_default()
                } else {
                    aliases
                },
                // Carried through explicitly: this rebuilds the whole profile,
                // so a `gdr device add` on an existing id would otherwise
                // silently drop the pinned window.
                pinned_window,
            };
            config::upsert_host(&mut cfg, &id, profile);
            if default {
                cfg.default_host = Some(id.clone());
            }
            config::save(&cfg)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({"ok": true, "device": id, "address": address})
                );
            } else {
                println!(
                    "saved device '{id}' → {} in {}",
                    if config::is_loopback_alias(&address) {
                        "same machine (loopback)"
                    } else {
                        address.as_str()
                    },
                    config::config_path()?.display()
                );
            }
        }
        DeviceCmd::List => {
            if json {
                println!("{}", serde_json::to_string_pretty(&cfg)?);
            } else if cfg.hosts.is_empty() {
                println!("(no devices)");
            } else {
                for (name, p) in &cfg.hosts {
                    let def = if cfg.default_host.as_deref() == Some(name.as_str()) {
                        " (default)"
                    } else {
                        ""
                    };
                    let label = p
                        .label
                        .as_deref()
                        .map(|l| format!(" \"{l}\""))
                        .unwrap_or_default();
                    let aliases = if p.aliases.is_empty() {
                        String::new()
                    } else {
                        format!(" aliases=[{}]", p.aliases.join(", "))
                    };
                    let sudo = if p.sudo_password.as_ref().is_some_and(|s| !s.is_empty()) {
                        "sudo=set"
                    } else {
                        "sudo=-"
                    };
                    println!(
                        "{name}{def}{label}{aliases}  {}  token=set  {sudo}  pin={}",
                        if config::is_loopback_alias(&p.address) {
                            format!("{} → {}", p.address, p.addr_port())
                        } else {
                            p.addr_port()
                        },
                        p.pin.as_deref().map(|x| &x[..x.len().min(12)]).unwrap_or("-")
                    );
                }
            }
        }
        DeviceCmd::Show { query } => {
            let name = config::find_device_name(&cfg, &query)
                .with_context(|| format!("unknown device '{query}'"))?;
            let p = cfg.hosts.get(&name).unwrap();
            if json {
                let mut v = serde_json::to_value(p)?;
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("id".into(), name.clone().into());
                    if let Some(t) = obj.get_mut("token") {
                        *t = "(set)".into();
                    }
                    for k in ["sudo_password", "user_password"] {
                        if obj.get(k).and_then(|x| x.as_str()).is_some_and(|s| !s.is_empty()) {
                            obj.insert(k.into(), "(set)".into());
                        }
                    }
                }
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("id:      {name}");
                println!("label:   {}", p.label.as_deref().unwrap_or("-"));
                println!("aliases: {}", p.aliases.join(", "));
                println!("address: {}", p.addr_port());
                println!("ssh:     {}", p.ssh_target());
                println!("token:   set");
                println!(
                    "sudo:    {}",
                    if p.sudo_password.as_ref().is_some_and(|s| !s.is_empty()) {
                        "set"
                    } else {
                        "not set"
                    }
                );
                println!("pin:     {}", p.pin.as_deref().unwrap_or("-"));
            }
        }
        DeviceCmd::Remove { query } => {
            let name = config::find_device_name(&cfg, &query)
                .with_context(|| format!("unknown device '{query}'"))?;
            cfg.hosts.remove(&name);
            if cfg.default_host.as_deref() == Some(name.as_str()) {
                cfg.default_host = cfg.hosts.keys().next().cloned();
            }
            config::save(&cfg)?;
            println!("removed device '{name}'");
        }
        DeviceCmd::Default { query } => {
            let name = config::find_device_name(&cfg, &query)
                .with_context(|| format!("unknown device '{query}'"))?;
            cfg.default_host = Some(name.clone());
            config::save(&cfg)?;
            println!("default_host = {name}");
        }
        DeviceCmd::SetSudo {
            query,
            password,
            ask,
            clear,
        } => {
            let name = config::find_device_name(&cfg, &query)
                .with_context(|| format!("unknown device '{query}'"))?;
            let p = cfg.hosts.get_mut(&name).unwrap();
            if clear {
                p.sudo_password = None;
            } else if ask {
                p.sudo_password = Some(rpassword::prompt_password("sudo password: ")?);
            } else {
                p.sudo_password = Some(password.context("pass --password, --ask, or --clear")?);
            }
            config::save(&cfg)?;
            println!("updated sudo password for '{name}'");
        }
        DeviceCmd::SetToken { query, token, ask } => {
            let name = config::find_device_name(&cfg, &query)
                .with_context(|| format!("unknown device '{query}'"))?;
            let p = cfg.hosts.get_mut(&name).unwrap();
            p.token = if ask {
                rpassword::prompt_password("auth token: ")?
            } else {
                token.context("pass --token or --ask")?
            };
            config::save(&cfg)?;
            println!("updated token for '{name}'");
        }
        DeviceCmd::SetLabel { query, label } => {
            let name = config::find_device_name(&cfg, &query)
                .with_context(|| format!("unknown device '{query}'"))?;
            cfg.hosts.get_mut(&name).unwrap().label = Some(label.clone());
            config::save(&cfg)?;
            println!("label for '{name}' = \"{label}\"");
        }
        DeviceCmd::AddAlias { query, alias } => {
            let name = config::find_device_name(&cfg, &query)
                .with_context(|| format!("unknown device '{query}'"))?;
            let p = cfg.hosts.get_mut(&name).unwrap();
            if !p.aliases.iter().any(|a| a.eq_ignore_ascii_case(&alias)) {
                p.aliases.push(alias.clone());
            }
            config::save(&cfg)?;
            println!("alias \"{alias}\" → '{name}'");
        }
    }
    Ok(())
}

fn systemctl_user(args: &[&str]) -> Result<()> {
    let st = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("systemctl --user")?;
    if !st.success() {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

pub fn run_service(cmd: ServiceCmd) -> Result<()> {
    match cmd {
        ServiceCmd::Status => {
            let _ = systemctl_user(&["status", "gdr.service", "--no-pager"]);
        }
        ServiceCmd::Start => systemctl_user(&["start", "gdr.service"])?,
        ServiceCmd::Stop => systemctl_user(&["stop", "gdr.service"])?,
        ServiceCmd::Restart => systemctl_user(&["restart", "gdr.service"])?,
        ServiceCmd::Enable => systemctl_user(&["enable", "--now", "gdr.service"])?,
        ServiceCmd::Disable => {
            let _ = systemctl_user(&["disable", "--now", "gdr.service"]);
        }
        ServiceCmd::Logs { lines, follow } => {
            let mut c = Command::new("journalctl");
            c.args(["--user", "-u", "gdr.service", "-n", &lines.to_string()]);
            if follow {
                c.arg("-f");
            } else {
                c.arg("--no-pager");
            }
            let st = c.status().context("journalctl")?;
            if !st.success() {
                bail!("journalctl failed");
            }
        }
    }
    Ok(())
}

fn repo_root() -> Option<PathBuf> {
    let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for _ in 0..6 {
        if dir.join("scripts/setup-mcp-cursor.sh").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    let cwd = std::env::current_dir().ok()?;
    if cwd.join("scripts/setup-mcp-cursor.sh").is_file() {
        return Some(cwd);
    }
    None
}

pub fn run_mcp(cmd: McpCmd) -> Result<()> {
    match cmd {
        McpCmd::SetupCursor {
            dev,
            per_device,
            system,
            repo,
            restart,
        } => {
            if let Some(root) = repo_root() {
                let script = root.join("scripts/setup-mcp-cursor.sh");
                let mut c = Command::new(&script);
                if system {
                    c.arg("--system");
                }
                if repo {
                    c.arg("--repo");
                }
                if restart {
                    c.arg("--restart");
                }
                // Extended flags handled in setup script (we'll add them)
                if let Some(d) = &dev {
                    c.args(["--dev", d]);
                }
                if per_device {
                    c.arg("--per-device");
                }
                std::env::set_var("GDR_YES", "1");
                let st = c.status().with_context(|| format!("run {}", script.display()))?;
                if !st.success() {
                    bail!("setup-mcp-cursor.sh failed");
                }
            } else {
                // Inline minimal Cursor merge when scripts aren't beside the binary
                let mcp = if system || std::path::Path::new("/usr/bin/gdr-mcp").exists() {
                    "/usr/bin/gdr-mcp".to_string()
                } else {
                    bail!("gdr package scripts not found; install from source tree or use /usr/bin/gdr-mcp");
                };
                let mut args = vec![];
                if let Some(d) = &dev {
                    args.extend(["--dev".into(), d.clone()]);
                }
                write_cursor_mcp(&mcp, &args, per_device)?;
                println!("Updated ~/.cursor/mcp.json");
            }
        }
        McpCmd::Status => {
            let path = dirs::home_dir()
                .context("HOME")?
                .join(".cursor/mcp.json");
            if path.exists() {
                let raw = std::fs::read_to_string(&path)?;
                let v: serde_json::Value = serde_json::from_str(&raw)?;
                println!("{}", serde_json::to_string_pretty(&v["mcpServers"]["gdr"])?);
                if let Some(obj) = v["mcpServers"].as_object() {
                    for (k, _) in obj {
                        if k.starts_with("gdr-") {
                            println!("also: {k}");
                        }
                    }
                }
            } else {
                println!("no ~/.cursor/mcp.json");
            }
            println!(
                "gdr-mcp: {}",
                if std::path::Path::new("/usr/bin/gdr-mcp").exists() {
                    "/usr/bin/gdr-mcp"
                } else {
                    "(not installed system-wide)"
                }
            );
        }
        McpCmd::Update { restart_cursor } => {
            let root = repo_root().context("source tree not found (need scripts/update-mcp.sh)")?;
            let mut c = Command::new(root.join("scripts/update-mcp.sh"));
            if restart_cursor {
                c.arg("--restart-cursor");
            } else {
                c.arg("--no-restart");
            }
            std::env::set_var("GDR_YES", "1");
            let st = c.status()?;
            if !st.success() {
                bail!("update-mcp.sh failed");
            }
        }
    }
    Ok(())
}

fn write_cursor_mcp(command: &str, args: &[String], per_device: bool) -> Result<()> {
    let path = dirs::home_dir()
        .context("HOME")?
        .join(".cursor/mcp.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cfg: serde_json::Value = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?)?
    } else {
        serde_json::json!({ "mcpServers": {} })
    };
    let servers = cfg
        .as_object_mut()
        .unwrap()
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();

    let entry = if command.ends_with("gdr-mcp") || command == "gdr-mcp" {
        serde_json::json!({ "command": command, "args": args })
    } else {
        let mut a = vec![command.to_string()];
        a.extend(args.iter().cloned());
        serde_json::json!({ "command": "node", "args": a })
    };
    servers.insert("gdr".into(), entry.clone());

    if per_device {
        let gdr_cfg = config::load()?;
        for (id, _) in &gdr_cfg.hosts {
            let slug = id
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>();
            let mut dargs = args.to_vec();
            if !dargs.iter().any(|a| a == "--dev" || a.starts_with("--dev=")) {
                dargs.extend(["--dev".into(), id.clone()]);
            }
            let e = if command.ends_with("gdr-mcp") || command == "gdr-mcp" {
                serde_json::json!({ "command": command, "args": dargs })
            } else {
                let mut a = vec![command.to_string()];
                a.extend(dargs);
                serde_json::json!({ "command": "node", "args": a })
            };
            servers.insert(format!("gdr-{slug}"), e);
        }
    }

    std::fs::write(&path, serde_json::to_string_pretty(&cfg)? + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn run_pkg(cmd: PkgCmd) -> Result<()> {
    match cmd {
        PkgCmd::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
        }
        PkgCmd::Info | PkgCmd::Paths => {
            println!("version:  {}", env!("CARGO_PKG_VERSION"));
            println!(
                "gdr:      {}",
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "?".into())
            );
            println!(
                "gdrd:     {}",
                if std::path::Path::new("/usr/bin/gdrd").exists() {
                    "/usr/bin/gdrd"
                } else {
                    "(not in /usr/bin)"
                }
            );
            println!(
                "gdr-mcp:  {}",
                if std::path::Path::new("/usr/bin/gdr-mcp").exists() {
                    "/usr/bin/gdr-mcp"
                } else {
                    "(not in /usr/bin)"
                }
            );
            println!(
                "unit:     {}",
                if std::path::Path::new("/usr/lib/systemd/user/gdr.service").exists() {
                    "/usr/lib/systemd/user/gdr.service"
                } else {
                    "(not installed)"
                }
            );
            println!(
                "config:   {}",
                config::config_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "~/.config/gdr/config.json".into())
            );
            println!(
                "mcp data: {}",
                if std::path::Path::new("/usr/share/gdr/mcp-server").exists() {
                    "/usr/share/gdr/mcp-server"
                } else {
                    "(not installed)"
                }
            );
        }
        PkgCmd::Update {
            skip_deps,
            no_remotes,
            machines,
            no_git_pull,
            no_restart_cursor,
        } => {
            let root = repo_root().context("source tree not found")?;
            let mut c = Command::new(root.join("scripts/update.sh"));
            c.arg("--yes");
            if skip_deps {
                c.arg("--skip-deps");
            }
            if let Some(m) = machines {
                c.arg("--machines").arg(m);
            } else if no_remotes {
                c.arg("--local-only");
            }
            if no_git_pull {
                c.arg("--no-git-pull");
            }
            if no_restart_cursor {
                c.arg("--no-restart-cursor");
            }
            std::env::set_var("GDR_YES", "1");
            let st = c.status()?;
            if !st.success() {
                bail!("update.sh failed");
            }
        }
    }
    Ok(())
}
