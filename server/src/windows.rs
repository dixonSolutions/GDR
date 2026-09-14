//! Window plane for gdrd — the bridge between the `gdr-windows` GNOME Shell
//! extension and the gdr wire protocol.
//!
//! ## Why an extension is required
//!
//! Mutter's RemoteDesktop/ScreenCast APIs that gdrd already speaks deal in
//! *pixels*: they can hand over the composited screen and inject input, but
//! they cannot say what windows exist or raise one. The API that could,
//! `org.gnome.Shell.Introspect.GetWindows`, answers `AccessDenied` to every
//! caller except the desktop portal on GNOME 50 — the `org.gnome.shell
//! introspect` GSetting that used to open it up was removed. So the only
//! supported route is a shell extension running inside gnome-shell, and this
//! module is a typed client for it. Older desktops that still allow
//! Introspect get a read-only fallback so `ListWindows` at least works.
//!
//! ## Coordinate conversion
//!
//! The extension reports logical (stage) pixels. Every other coordinate in
//! gdr — `Region`, `MouseMove` — is in native capture-stream pixels. This
//! module is the single place that converts, so that a window rect can be
//! handed straight to `CaptureFrame` as a crop.

use anyhow::{anyhow, Context, Result};
use common::windows::{
    AppInfo, LogicalRect, MonitorInfo, WindowBackend, WindowEvent, WindowInfo, WindowOp,
};
use common::{Region, WindowList};
use serde::Deserialize;
use std::time::Duration;
use zbus::Connection;

const EXT_UUID: &str = "gdr-windows@gdr.dixonsolutions.github.io";

/// Ceiling on a `WindowEvents` long-poll. Long enough to make a watcher
/// cheap, short enough that a client blocked here still notices a dead
/// connection within a sensible time.
pub const MAX_WAIT_MS: u64 = 30_000;

/// Advice attached to every "extension missing" error. Users hit this once,
/// during install, and the fix is unguessable — the shell only picks up a
/// newly installed extension at session start.
/// Advice for an extension that *was* answering and stopped.
///
/// The commonest cause is the screen locking: gnome-shell unloads every
/// extension whose `session-modes` does not include `unlock-dialog` when the
/// lock shield goes up, and that was this extension until the version that
/// added it. So the window plane vanishing mid-session usually means the
/// target is sitting at its lock screen with an older copy installed —
/// reinstalling and logging out again would change nothing.
pub const VANISHED_HINT: &str = concat!(
    "the gdr-windows GNOME Shell extension was answering and has stopped. ",
    "It is still installed — do not reinstall. Either gnome-shell restarted, ",
    "or the session locked and the installed copy of the extension predates ",
    "`session-modes: [user, unlock-dialog]`, so the shell unloaded it behind ",
    "the lock shield. Unlock the session, or update the extension with ",
    "`scripts/install-window-extension.sh` and log out once."
);

pub const INSTALL_HINT: &str = concat!(
    "the gdr-windows GNOME Shell extension is not running on the target. ",
    "Install it with `scripts/install-window-extension.sh`, ",
    "then log out and back in — GNOME/Wayland only scans for new extensions at ",
    "session start, so enabling it is not enough."
);

#[zbus::proxy(
    interface = "org.gdr.Windows",
    default_service = "org.gdr.Windows",
    default_path = "/org/gdr/Windows"
)]
pub trait GdrWindows {
    fn list(&self) -> zbus::Result<String>;
    fn act(&self, id: u64, action: &str, args_json: &str) -> zbus::Result<String>;
    fn launch(&self, app_id: &str) -> zbus::Result<String>;
    fn list_apps(&self, filter: &str) -> zbus::Result<String>;
    fn events(&self, since: u64, limit: u32) -> zbus::Result<String>;

    #[zbus(property, name = "ApiVersion")]
    fn api_version(&self) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn changed(&self, kind: String, id: u64) -> zbus::Result<()>;
}

// --- JSON shapes the extension emits. Mirrored by hand from
// --- shell-extension/…/extension.js; keep the two in step.

#[derive(Debug, Deserialize)]
struct RawRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl From<RawRect> for LogicalRect {
    fn from(r: RawRect) -> Self {
        LogicalRect {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawMonitor {
    index: i32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    #[serde(default = "one")]
    scale: f64,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    connector: Option<String>,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Deserialize)]
struct RawWindow {
    id: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    pid: i32,
    #[serde(default)]
    window_type: String,
    frame_rect: Option<RawRect>,
    #[serde(default)]
    monitor: i32,
    #[serde(default)]
    workspace: Option<i32>,
    #[serde(default)]
    on_active_workspace: bool,
    #[serde(default)]
    minimized: bool,
    #[serde(default)]
    maximized: String,
    #[serde(default)]
    fullscreen: bool,
    #[serde(default)]
    focus: bool,
    #[serde(default)]
    above: bool,
    #[serde(default)]
    on_all_workspaces: bool,
    #[serde(default)]
    skip_taskbar: bool,
    #[serde(default)]
    can_close: bool,
}

#[derive(Debug, Deserialize)]
struct RawList {
    #[serde(default)]
    seq: u64,
    #[serde(default)]
    focus_window: Option<u64>,
    #[serde(default)]
    active_workspace: i32,
    #[serde(default)]
    n_workspaces: i32,
    #[serde(default)]
    monitors: Vec<RawMonitor>,
    #[serde(default)]
    windows: Vec<RawWindow>,
}

#[derive(Debug, Deserialize)]
struct RawActed {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    window: Option<RawWindow>,
}

#[derive(Debug, Deserialize)]
struct RawLaunched {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    was_running: bool,
}

#[derive(Debug, Deserialize)]
struct RawApps {
    #[serde(default)]
    apps: Vec<AppInfo>,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    seq: u64,
    kind: String,
    #[serde(default)]
    at_ms: i64,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawEvents {
    #[serde(default)]
    dropped: bool,
    #[serde(default)]
    next_seq: u64,
    #[serde(default)]
    events: Vec<RawEvent>,
}

/// Convert the extension's logical geometry into capture-stream pixels.
///
/// `stream_size` is the real size of the frames gdrd is pulling. Preferring it
/// over the compositor's reported `scale` is deliberate: it is measured rather
/// than declared, so fractional scaling, rotation-free transforms and any
/// mismatch between mutter's notion of scale and what PipeWire actually
/// negotiated all come out right. The reported scale is the fallback for the
/// window in time before the first capture.
pub fn to_stream_region(
    frame: LogicalRect,
    monitor: &RawMonitorView,
    stream_size: Option<(u32, u32)>,
) -> Option<Region> {
    if monitor.width <= 0 || monitor.height <= 0 {
        return None;
    }
    let (sx, sy) = match stream_size {
        Some((w, h)) if w > 0 && h > 0 => (
            f64::from(w) / f64::from(monitor.width),
            f64::from(h) / f64::from(monitor.height),
        ),
        _ => (monitor.scale, monitor.scale),
    };
    if sx <= 0.0 || sy <= 0.0 {
        return None;
    }

    // Clamp to the monitor: a window can hang off the edge (or be positioned
    // negatively while animating), and a crop outside the frame is an error
    // in gdrd rather than a silently shifted image.
    let max_w = (f64::from(monitor.width) * sx).round() as i64;
    let max_h = (f64::from(monitor.height) * sy).round() as i64;

    let left = ((f64::from(frame.x - monitor.x)) * sx).round() as i64;
    let top = ((f64::from(frame.y - monitor.y)) * sy).round() as i64;
    let right = left + ((f64::from(frame.width)) * sx).round() as i64;
    let bottom = top + ((f64::from(frame.height)) * sy).round() as i64;

    let left = left.clamp(0, max_w);
    let top = top.clamp(0, max_h);
    let right = right.clamp(0, max_w);
    let bottom = bottom.clamp(0, max_h);

    if right <= left || bottom <= top {
        return None;
    }
    Some(Region {
        x: left as u32,
        y: top as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// Minimal view of a monitor, so the mapping above is testable without
/// building a whole `MonitorInfo`.
pub struct RawMonitorView {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
}

impl RawMonitorView {
    fn of(m: &MonitorInfo) -> Self {
        Self {
            x: m.geometry.x,
            y: m.geometry.y,
            width: m.geometry.width,
            height: m.geometry.height,
            scale: m.scale,
        }
    }
}

/// Live handle on the window plane.
pub struct WindowPlane {
    conn: Connection,
    /// Whether the extension has ever answered on this connection.
    ///
    /// Kept because the two ways the window plane is unavailable need
    /// opposite advice, and the error text is identical without it: never
    /// installed (install it, log out) versus was answering and stopped
    /// (the shell restarted, or the session locked and an older installed
    /// copy of the extension is unloaded on the lock screen). Telling
    /// someone to reinstall and log out when the extension worked twenty
    /// seconds ago wastes a whole trip.
    seen: std::sync::atomic::AtomicBool,
}

impl WindowPlane {
    pub async fn connect() -> Result<Self> {
        Ok(Self {
            seen: std::sync::atomic::AtomicBool::new(false),
            conn: Connection::session()
                .await
                .context("connect to the session bus")?,
        })
    }

    async fn proxy(&self) -> Result<GdrWindowsProxy<'_>> {
        GdrWindowsProxy::new(&self.conn).await.map_err(|e| {
            anyhow!(
                "{} (uuid {EXT_UUID}; session bus said: {e})",
                self.unavailable_hint()
            )
        })
    }

    /// True when the extension is answering. Cheap: one property read.
    pub async fn available(&self) -> bool {
        let ok = match GdrWindowsProxy::new(&self.conn).await {
            Ok(p) => p.api_version().await.is_ok(),
            Err(_) => false,
        };
        if ok {
            self.note_seen();
        }
        ok
    }

    fn note_seen(&self) {
        self.seen.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The advice to attach to "the window plane is not answering".
    ///
    /// Splits on whether we have ever had an answer, because the fix differs
    /// completely and the symptom does not.
    pub fn unavailable_hint(&self) -> &'static str {
        if self.seen.load(std::sync::atomic::Ordering::Relaxed) {
            VANISHED_HINT
        } else {
            INSTALL_HINT
        }
    }

    /// Whether the session is locked, for hooks that need to say so.
    pub async fn session_locked(&self) -> Option<bool> {
        crate::mutter_dbus::probe_lock_state(&self.conn).await.locked
    }

    /// Enumerate windows, converting geometry into capture-stream pixels.
    ///
    /// `capture_connector` is the monitor gdrd is streaming and
    /// `stream_size` the size of its frames; both may be unknown before the
    /// first capture, in which case windows come back without a
    /// `stream_region` and the caller is told why.
    pub async fn list(
        &self,
        capture_connector: Option<&str>,
        stream_size: Option<(u32, u32)>,
        include_skip_taskbar: bool,
    ) -> Result<WindowList> {
        let json = self
            .proxy()
            .await?
            .list()
            .await
            .map_err(|e| anyhow!("{} (List failed: {e})", self.unavailable_hint()))?;
        // A successful listing is the strongest evidence the extension is
        // alive, and it is the call every watcher makes — so this is where
        // "we have seen it work" gets recorded.
        self.note_seen();
        let raw: RawList = serde_json::from_str(&json).context("parse extension List payload")?;

        let monitors: Vec<MonitorInfo> = raw
            .monitors
            .into_iter()
            .map(|m| {
                let captured = match (capture_connector, m.connector.as_deref()) {
                    (Some(want), Some(have)) => want == have,
                    // Nothing to disambiguate with: on a single-monitor
                    // desktop the captured stream can only be this one.
                    _ => false,
                };
                MonitorInfo {
                    index: m.index,
                    connector: m.connector,
                    geometry: LogicalRect {
                        x: m.x,
                        y: m.y,
                        width: m.width,
                        height: m.height,
                    },
                    scale: m.scale,
                    primary: m.primary,
                    captured,
                }
            })
            .collect();

        // Single monitor, or no connector names to match on: whatever gdrd is
        // streaming, it is streaming that one. Marking it captured is what
        // lets window screenshots work on the overwhelmingly common
        // one-display laptop.
        let mut monitors = monitors;
        if monitors.len() == 1 && !monitors[0].captured {
            monitors[0].captured = true;
        }

        let windows = raw
            .windows
            .into_iter()
            .filter(|w| include_skip_taskbar || !w.skip_taskbar)
            .map(|w| {
                let frame = w
                    .frame_rect
                    .map(LogicalRect::from)
                    .unwrap_or(LogicalRect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    });
                let monitor = monitors.iter().find(|m| m.index == w.monitor);
                let stream_region = monitor.filter(|m| m.captured).and_then(|m| {
                    to_stream_region(frame, &RawMonitorView::of(m), stream_size)
                });
                WindowInfo {
                    id: w.id,
                    title: w.title,
                    wm_class: w.wm_class,
                    app_id: w.app_id,
                    pid: w.pid,
                    window_type: w.window_type,
                    frame_rect: frame,
                    stream_region,
                    monitor: w.monitor,
                    connector: monitor.and_then(|m| m.connector.clone()),
                    workspace: w.workspace,
                    on_active_workspace: w.on_active_workspace,
                    minimized: w.minimized,
                    maximized: w.maximized,
                    fullscreen: w.fullscreen,
                    focus: w.focus,
                    above: w.above,
                    on_all_workspaces: w.on_all_workspaces,
                    skip_taskbar: w.skip_taskbar,
                    can_close: w.can_close,
                }
            })
            .collect();

        Ok(WindowList {
            backend: WindowBackend::Extension,
            windows,
            monitors,
            focus_window: raw.focus_window,
            active_workspace: raw.active_workspace,
            n_workspaces: raw.n_workspaces,
            capture_connector: capture_connector.map(str::to_string),
            seq: raw.seq,
        })
    }

    /// Apply one operation and return the window as it ended up.
    pub async fn act(&self, id: u64, op: WindowOp) -> Result<(String, Option<WindowInfo>)> {
        let json = self
            .proxy()
            .await?
            .act(id, op.name(), &op.args_json())
            .await
            .map_err(|e| anyhow!("{} (Act failed: {e})", self.unavailable_hint()))?;
        let raw: RawActed = serde_json::from_str(&json).context("parse extension Act payload")?;
        if !raw.ok {
            return Err(anyhow!(
                "{} refused: {}",
                op.name(),
                raw.error.unwrap_or_else(|| "no reason given".into())
            ));
        }
        // Geometry here is logical only: the caller re-lists if it needs a
        // stream region, and converting with a stale stream size would be
        // worse than not converting at all.
        let window = raw.window.map(|w| WindowInfo {
            id: w.id,
            title: w.title,
            wm_class: w.wm_class,
            app_id: w.app_id,
            pid: w.pid,
            window_type: w.window_type,
            frame_rect: w.frame_rect.map(LogicalRect::from).unwrap_or(LogicalRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }),
            stream_region: None,
            monitor: w.monitor,
            connector: None,
            workspace: w.workspace,
            on_active_workspace: w.on_active_workspace,
            minimized: w.minimized,
            maximized: w.maximized,
            fullscreen: w.fullscreen,
            focus: w.focus,
            above: w.above,
            on_all_workspaces: w.on_all_workspaces,
            skip_taskbar: w.skip_taskbar,
            can_close: w.can_close,
        });
        Ok((raw.action.unwrap_or_else(|| op.name().to_string()), window))
    }

    pub async fn launch(&self, app_id: &str) -> Result<(String, Option<String>, bool)> {
        let json = self
            .proxy()
            .await?
            .launch(app_id)
            .await
            .map_err(|e| anyhow!("{} (Launch failed: {e})", self.unavailable_hint()))?;
        let raw: RawLaunched =
            serde_json::from_str(&json).context("parse extension Launch payload")?;
        if !raw.ok {
            return Err(anyhow!(
                "{}",
                raw.error.unwrap_or_else(|| "launch refused".into())
            ));
        }
        Ok((
            raw.app_id.unwrap_or_else(|| app_id.to_string()),
            raw.name,
            raw.was_running,
        ))
    }

    pub async fn list_apps(&self, filter: Option<&str>) -> Result<Vec<AppInfo>> {
        let json = self
            .proxy()
            .await?
            .list_apps(filter.unwrap_or(""))
            .await
            .map_err(|e| anyhow!("{} (ListApps failed: {e})", self.unavailable_hint()))?;
        let raw: RawApps =
            serde_json::from_str(&json).context("parse extension ListApps payload")?;
        Ok(raw.apps)
    }

    /// Poll the window journal, optionally waiting for something to happen.
    ///
    /// Returns `(events, next_seq, dropped, reset)`.
    pub async fn events(
        &self,
        since: u64,
        limit: u32,
        wait_ms: u64,
    ) -> Result<(Vec<WindowEvent>, u64, bool, bool)> {
        let proxy = self.proxy().await?;

        // Subscribe *before* the first read, or an event that lands between
        // the read and the subscribe is waited on forever despite having
        // already happened.
        let mut changed = if wait_ms > 0 {
            Some(
                proxy
                    .receive_changed()
                    .await
                    .context("subscribe to org.gdr.Windows.Changed")?,
            )
        } else {
            None
        };

        let mut result = fetch_events(&proxy, since, limit, self.unavailable_hint()).await?;
        if result.0.is_empty() {
            if let Some(stream) = changed.as_mut() {
                use futures_util::StreamExt;
                let wait = Duration::from_millis(wait_ms.min(MAX_WAIT_MS));
                if tokio::time::timeout(wait, stream.next()).await.is_ok() {
                    result = fetch_events(&proxy, since, limit, self.unavailable_hint()).await?;
                }
            }
        }
        Ok(result)
    }
}

async fn fetch_events(
    proxy: &GdrWindowsProxy<'_>,
    since: u64,
    limit: u32,
    hint: &str,
) -> Result<(Vec<WindowEvent>, u64, bool, bool)> {
    let json = proxy
        .events(since, limit)
        .await
        .map_err(|e| anyhow!("{hint} (Events failed: {e})"))?;
    let raw: RawEvents = serde_json::from_str(&json).context("parse extension Events payload")?;

    // The extension's counter restarts whenever gnome-shell (or just the
    // extension) restarts. A caller resuming from a higher `since` would then
    // silently see nothing forever, so say the sequence reset and hand back
    // everything we have.
    let reset = since > raw.next_seq;

    let events = raw
        .events
        .into_iter()
        .map(|e| WindowEvent {
            seq: e.seq,
            kind: e.kind,
            at: ms_to_rfc3339(e.at_ms),
            id: e.id,
            title: e.title,
            wm_class: e.wm_class,
            app_id: e.app_id,
        })
        .collect();
    Ok((events, raw.next_seq, raw.dropped, reset))
}

fn ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: i32, y: i32, w: i32, h: i32, scale: f64) -> RawMonitorView {
        RawMonitorView {
            x,
            y,
            width: w,
            height: h,
            scale,
        }
    }

    #[test]
    fn measured_stream_size_beats_reported_scale() {
        // The panel this was developed on: 1920x1200 native, 125% scaling,
        // so the compositor reports a 1536x960 stage.
        let m = monitor(0, 0, 1536, 960, 1.0); // deliberately wrong scale
        let r = to_stream_region(
            LogicalRect {
                x: 100,
                y: 50,
                width: 800,
                height: 600,
            },
            &m,
            Some((1920, 1200)),
        )
        .unwrap();
        // 100 * 1.25 = 125, 50 * 1.25 = 62.5 -> 63 (half away from zero).
        assert_eq!(
            r,
            Region {
                x: 125,
                y: 63,
                width: 1000,
                height: 750
            }
        );
    }

    #[test]
    fn falls_back_to_reported_scale_before_the_first_capture() {
        let m = monitor(0, 0, 1536, 960, 1.25);
        let r = to_stream_region(
            LogicalRect {
                x: 0,
                y: 0,
                width: 1536,
                height: 960,
            },
            &m,
            None,
        )
        .unwrap();
        assert_eq!(r.width, 1920);
        assert_eq!(r.height, 1200);
    }

    #[test]
    fn window_rect_is_relative_to_its_own_monitor() {
        // Second monitor placed to the right of a 1536-wide primary.
        let m = monitor(1536, 0, 1920, 1080, 1.0);
        let r = to_stream_region(
            LogicalRect {
                x: 1636,
                y: 10,
                width: 400,
                height: 300,
            },
            &m,
            Some((1920, 1080)),
        )
        .unwrap();
        assert_eq!(r.x, 100, "monitor origin must be subtracted first");
        assert_eq!(r.y, 10);
    }

    #[test]
    fn overhanging_windows_are_clamped_not_rejected() {
        let m = monitor(0, 0, 1000, 800, 1.0);
        let r = to_stream_region(
            LogicalRect {
                x: -200,
                y: 700,
                width: 600,
                height: 400,
            },
            &m,
            Some((1000, 800)),
        )
        .unwrap();
        assert_eq!(r.x, 0);
        assert_eq!(r.width, 400, "left overhang trimmed");
        assert_eq!(r.height, 100, "bottom overhang trimmed");
    }

    #[test]
    fn fully_offscreen_window_has_no_region() {
        let m = monitor(0, 0, 1000, 800, 1.0);
        assert!(to_stream_region(
            LogicalRect {
                x: 2000,
                y: 0,
                width: 100,
                height: 100
            },
            &m,
            Some((1000, 800))
        )
        .is_none());
    }

    #[test]
    fn parses_a_realistic_extension_payload() {
        let json = r#"{
          "api_version": 1, "seq": 7, "coordinate_space": "logical",
          "focus_window": 42, "active_workspace": 0, "n_workspaces": 2,
          "monitors": [{"index":0,"x":0,"y":0,"width":1536,"height":960,
                        "scale":1.25,"primary":true,"connector":"eDP-1"}],
          "windows": [{"id":42,"title":"Home","wm_class":"org.gnome.Nautilus",
                       "app_id":"org.gnome.Nautilus.desktop","pid":1234,
                       "window_type":"normal",
                       "frame_rect":{"x":10,"y":20,"width":800,"height":600},
                       "monitor":0,"workspace":0,"on_active_workspace":true,
                       "minimized":false,"maximized":"none","fullscreen":false,
                       "focus":true,"above":false,"on_all_workspaces":false,
                       "skip_taskbar":false,"can_close":true}]
        }"#;
        let raw: RawList = serde_json::from_str(json).unwrap();
        assert_eq!(raw.seq, 7);
        assert_eq!(raw.windows.len(), 1);
        assert_eq!(raw.windows[0].app_id.as_deref(), Some("org.gnome.Nautilus.desktop"));
        assert_eq!(raw.monitors[0].scale, 1.25);
    }

    #[test]
    fn tolerates_fields_a_newer_extension_adds() {
        // Forward compatibility matters here: the extension is installed
        // separately from gdrd and the two versions drift in the field.
        let json = r#"{"seq":1,"monitors":[],"windows":[
            {"id":1,"monitor":0,"minimized":false,"focus":false,
             "frame_rect":{"x":0,"y":0,"width":1,"height":1},
             "some_future_field":"ignored"}],"another_new_key":true}"#;
        let raw: RawList = serde_json::from_str(json).unwrap();
        assert_eq!(raw.windows[0].id, 1);
    }

    #[test]
    fn event_timestamps_become_rfc3339() {
        let s = ms_to_rfc3339(1_700_000_000_000);
        assert!(s.starts_with("2023-11-14T"), "{s}");
    }
}
