//! The window plane: enumerate windows, act on one, watch them open and close.
//!
//! Everything here is *metadata*. Pixels still come from `CaptureFrame`, and
//! input still goes through the pointer/keyboard requests — so a token that
//! holds only `window` can see what is open and rearrange it, but cannot read
//! the screen or type into it.
//!
//! Two coordinate spaces meet in this module and mixing them up produces
//! clicks that land tens of pixels off:
//!
//! * **logical** — GNOME stage pixels, what the compositor reports. On a
//!   1920×1200 panel at 125% these run 0..1536 × 0..960.
//! * **stream** — native capture pixels, what `Region`, `MouseMove` and every
//!   existing gdr coordinate already use.
//!
//! [`WindowInfo::frame_rect`] is logical and [`WindowInfo::stream_region`] is
//! stream. gdrd fills the second in; nothing downstream should be scaling
//! anything by hand.

use serde::{Deserialize, Serialize};

use crate::Region;

/// Rectangle in GNOME logical (stage) pixels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogicalRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl LogicalRect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MonitorInfo {
    pub index: i32,
    /// DRM connector ("eDP-1"), when the compositor would tell us.
    #[serde(default)]
    pub connector: Option<String>,
    /// Monitor placement and size in logical pixels.
    pub geometry: LogicalRect,
    /// Logical → stream factor. Fractional scaling makes this non-integer.
    pub scale: f64,
    pub primary: bool,
    /// True for the monitor gdrd is currently streaming. Windows anywhere
    /// else cannot be screenshotted without moving them here first.
    pub captured: bool,
}

/// One managed window, as the compositor sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowInfo {
    /// Compositor window id. Stable while the window lives, reused freely
    /// after it closes — pin by app/title if you need to survive a restart.
    pub id: u64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub wm_class: Option<String>,
    /// Desktop-file id ("org.gnome.Nautilus.desktop"), when the shell could
    /// match the window to an installed app. This is what `LaunchApp` takes.
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub window_type: String,
    /// Window frame in logical pixels.
    pub frame_rect: LogicalRect,
    /// Same frame in capture-stream pixels — directly usable as a
    /// `CaptureFrame` region. `None` when the window is not on the monitor
    /// gdrd is streaming, so a crop would show the wrong desktop.
    #[serde(default)]
    pub stream_region: Option<Region>,
    pub monitor: i32,
    #[serde(default)]
    pub connector: Option<String>,
    #[serde(default)]
    pub workspace: Option<i32>,
    #[serde(default)]
    pub on_active_workspace: bool,
    pub minimized: bool,
    #[serde(default)]
    pub maximized: String,
    #[serde(default)]
    pub fullscreen: bool,
    pub focus: bool,
    #[serde(default)]
    pub above: bool,
    #[serde(default)]
    pub on_all_workspaces: bool,
    #[serde(default)]
    pub skip_taskbar: bool,
    #[serde(default)]
    pub can_close: bool,
}

impl WindowInfo {
    /// Whether a screenshot of this window would actually show it.
    ///
    /// Wayland composites; the capture stream carries what is on screen, not
    /// a per-window buffer. A minimized window, one on another workspace, or
    /// one on a monitor we are not streaming has to be activated first.
    pub fn visible_in_capture(&self) -> bool {
        self.stream_region.is_some() && !self.minimized && self.on_active_workspace
    }

    /// Short human label for logs and error messages.
    pub fn label(&self) -> String {
        let name = self
            .app_id
            .as_deref()
            .or(self.wm_class.as_deref())
            .unwrap_or("?");
        match self.title.as_deref() {
            Some(t) if !t.is_empty() => format!("{name} — {t}"),
            _ => name.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    pub windows: u32,
    pub running: bool,
}

/// How to name the window an operation applies to.
///
/// Fields are combined with AND. `id` alone is the fast, exact path; the
/// string fields are what a *pin* stores, because ids do not survive the
/// window closing and reopening.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WindowTarget {
    #[serde(default)]
    pub id: Option<u64>,
    /// Desktop-file id. Matched case-insensitively, with or without the
    /// trailing `.desktop`.
    #[serde(default)]
    pub app_id: Option<String>,
    /// WM_CLASS. Matched case-insensitively as a substring.
    #[serde(default)]
    pub wm_class: Option<String>,
    /// Window title. Matched case-insensitively as a substring.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub pid: Option<i32>,
    /// Match whatever currently has keyboard focus.
    #[serde(default)]
    pub focused: bool,
}

/// Why a [`WindowTarget`] did not name exactly one window.
#[derive(Debug, Clone, PartialEq)]
pub enum TargetError {
    Empty,
    NotFound,
    /// More than one window matched. Carries the candidates so the caller can
    /// show them instead of silently acting on an arbitrary one.
    Ambiguous(Vec<WindowInfo>),
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TargetError::Empty => write!(
                f,
                "no window selector given — pass id/app_id/wm_class/title/pid, \
                 set focused=true, or pin a window first"
            ),
            TargetError::NotFound => write!(f, "no open window matches that selector"),
            TargetError::Ambiguous(c) => {
                let names: Vec<String> = c
                    .iter()
                    .take(8)
                    .map(|w| format!("id={} {}", w.id, w.label()))
                    .collect();
                write!(
                    f,
                    "{} windows match that selector — narrow it or pass an id: {}",
                    c.len(),
                    names.join(" | ")
                )
            }
        }
    }
}

impl std::error::Error for TargetError {}

fn norm_app_id(s: &str) -> String {
    s.trim()
        .trim_end_matches(".desktop")
        .to_ascii_lowercase()
}

impl WindowTarget {
    pub fn by_id(id: u64) -> Self {
        Self {
            id: Some(id),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.app_id.is_none()
            && self.wm_class.is_none()
            && self.title.is_none()
            && self.pid.is_none()
            && !self.focused
    }

    fn matches(&self, w: &WindowInfo) -> bool {
        if let Some(id) = self.id {
            if w.id != id {
                return false;
            }
        }
        if self.focused && !w.focus {
            return false;
        }
        if let Some(pid) = self.pid {
            if w.pid != pid {
                return false;
            }
        }
        if let Some(app) = &self.app_id {
            match &w.app_id {
                Some(actual) if norm_app_id(actual) == norm_app_id(app) => {}
                _ => return false,
            }
        }
        if let Some(class) = &self.wm_class {
            let needle = class.trim().to_ascii_lowercase();
            match &w.wm_class {
                Some(actual) if actual.to_ascii_lowercase().contains(&needle) => {}
                _ => return false,
            }
        }
        if let Some(title) = &self.title {
            let needle = title.trim().to_ascii_lowercase();
            match &w.title {
                Some(actual) if actual.to_ascii_lowercase().contains(&needle) => {}
                _ => return false,
            }
        }
        true
    }

    /// Pick the one window this selector names.
    ///
    /// An explicit `id` wins outright — including over ambiguity in the other
    /// fields — because it is the only exact handle we have. Otherwise several
    /// matches is an error, not a coin flip: acting on the wrong window is
    /// worse than a message telling the caller to be specific. The single
    /// exception is a focused match, which is a deliberate tie-break rather
    /// than an arbitrary one.
    pub fn resolve<'a>(&self, windows: &'a [WindowInfo]) -> Result<&'a WindowInfo, TargetError> {
        if self.is_empty() {
            return Err(TargetError::Empty);
        }
        if let Some(id) = self.id {
            return windows
                .iter()
                .find(|w| w.id == id)
                .ok_or(TargetError::NotFound);
        }
        let hits: Vec<&WindowInfo> = windows.iter().filter(|w| self.matches(w)).collect();
        match hits.len() {
            0 => Err(TargetError::NotFound),
            1 => Ok(hits[0]),
            _ => match hits.iter().find(|w| w.focus) {
                Some(w) => Ok(w),
                None => Err(TargetError::Ambiguous(
                    hits.into_iter().cloned().collect(),
                )),
            },
        }
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(id) = self.id {
            parts.push(format!("id={id}"));
        }
        if let Some(a) = &self.app_id {
            parts.push(format!("app_id={a}"));
        }
        if let Some(c) = &self.wm_class {
            parts.push(format!("wm_class~{c}"));
        }
        if let Some(t) = &self.title {
            parts.push(format!("title~{t}"));
        }
        if let Some(p) = self.pid {
            parts.push(format!("pid={p}"));
        }
        if self.focused {
            parts.push("focused".into());
        }
        if parts.is_empty() {
            "(empty)".into()
        } else {
            parts.join(" ")
        }
    }
}

/// What to do to a window. Serializes flat: `{"action":"move","x":10,"y":20}`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WindowOp {
    /// Unminimize, switch workspace, raise and focus — the shell's own
    /// "show me this window" path. This is what makes a window that is not
    /// currently on screen screenshottable.
    Activate,
    Focus,
    Raise,
    Minimize,
    Unminimize,
    Maximize,
    Unmaximize,
    Fullscreen,
    Unfullscreen,
    Above,
    Unabove,
    Stick,
    Unstick,
    Close,
    Move { x: i32, y: i32 },
    Resize { width: i32, height: i32 },
    MoveResize { x: i32, y: i32, width: i32, height: i32 },
    Workspace { index: i32 },
}

impl WindowOp {
    /// Wire name understood by the shell extension's `Act`.
    pub fn name(&self) -> &'static str {
        match self {
            WindowOp::Activate => "activate",
            WindowOp::Focus => "focus",
            WindowOp::Raise => "raise",
            WindowOp::Minimize => "minimize",
            WindowOp::Unminimize => "unminimize",
            WindowOp::Maximize => "maximize",
            WindowOp::Unmaximize => "unmaximize",
            WindowOp::Fullscreen => "fullscreen",
            WindowOp::Unfullscreen => "unfullscreen",
            WindowOp::Above => "above",
            WindowOp::Unabove => "unabove",
            WindowOp::Stick => "stick",
            WindowOp::Unstick => "unstick",
            WindowOp::Close => "close",
            WindowOp::Move { .. } => "move",
            WindowOp::Resize { .. } => "resize",
            WindowOp::MoveResize { .. } => "move_resize",
            WindowOp::Workspace { .. } => "workspace",
        }
    }

    /// Geometry/workspace arguments, as the extension's `args_json`.
    pub fn args_json(&self) -> String {
        match *self {
            WindowOp::Move { x, y } => format!(r#"{{"x":{x},"y":{y}}}"#),
            WindowOp::Resize { width, height } => {
                format!(r#"{{"width":{width},"height":{height}}}"#)
            }
            WindowOp::MoveResize {
                x,
                y,
                width,
                height,
            } => format!(r#"{{"x":{x},"y":{y},"width":{width},"height":{height}}}"#),
            WindowOp::Workspace { index } => format!(r#"{{"index":{index}}}"#),
            _ => "{}".to_string(),
        }
    }

    /// True for operations that change what the user sees or loses work.
    /// Used for audit detail, not for permission checks.
    pub fn is_destructive(&self) -> bool {
        matches!(self, WindowOp::Close)
    }
}

/// Something that happened to a window, in the order it happened.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowEvent {
    /// Monotonic within one run of the shell extension. Pass the last one
    /// back as `since` to resume without gaps.
    pub seq: u64,
    /// `opened` | `closed` | `focused` | `minimized` | `unminimized`.
    pub kind: String,
    /// RFC-3339, stamped by gdrd.
    pub at: String,
    pub id: u64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub wm_class: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
}

/// Which implementation answered a window request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WindowBackend {
    /// The `gdr-windows` GNOME Shell extension over `org.gdr.Windows`.
    Extension,
    /// `org.gnome.Shell.Introspect` — read-only, and refused outright by
    /// GNOME 50, so only older desktops ever land here.
    Introspect,
}

impl WindowBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowBackend::Extension => "extension",
            WindowBackend::Introspect => "introspect",
        }
    }

    /// Whether this backend can do anything other than list.
    pub fn supports_actions(self) -> bool {
        matches!(self, WindowBackend::Extension)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u64, app: &str, class: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id,
            title: Some(title.into()),
            wm_class: Some(class.into()),
            app_id: Some(app.into()),
            pid: 100 + id as i32,
            window_type: "normal".into(),
            frame_rect: LogicalRect {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
            stream_region: None,
            monitor: 0,
            connector: Some("eDP-1".into()),
            workspace: Some(0),
            on_active_workspace: true,
            minimized: false,
            maximized: "none".into(),
            fullscreen: false,
            focus: false,
            above: false,
            on_all_workspaces: false,
            skip_taskbar: false,
            can_close: true,
        }
    }

    fn sample() -> Vec<WindowInfo> {
        vec![
            win(1, "org.gnome.Nautilus.desktop", "org.gnome.Nautilus", "Home"),
            win(2, "firefox.desktop", "firefox", "gdr — Mozilla Firefox"),
            win(3, "firefox.desktop", "firefox", "docs — Mozilla Firefox"),
        ]
    }

    #[test]
    fn empty_target_is_an_error_not_a_guess() {
        assert_eq!(
            WindowTarget::default().resolve(&sample()).unwrap_err(),
            TargetError::Empty
        );
    }

    #[test]
    fn id_resolves_exactly() {
        let windows = sample();
        assert_eq!(WindowTarget::by_id(2).resolve(&windows).unwrap().id, 2);
        assert_eq!(
            WindowTarget::by_id(99).resolve(&sample()).unwrap_err(),
            TargetError::NotFound
        );
    }

    #[test]
    fn app_id_ignores_desktop_suffix_and_case() {
        let t = WindowTarget {
            app_id: Some("ORG.GNOME.NAUTILUS".into()),
            ..Default::default()
        };
        assert_eq!(t.resolve(&sample()).unwrap().id, 1);
    }

    #[test]
    fn title_is_a_substring_match() {
        let t = WindowTarget {
            title: Some("docs".into()),
            ..Default::default()
        };
        assert_eq!(t.resolve(&sample()).unwrap().id, 3);
    }

    #[test]
    fn ambiguity_is_reported_with_candidates() {
        let t = WindowTarget {
            app_id: Some("firefox".into()),
            ..Default::default()
        };
        match t.resolve(&sample()).unwrap_err() {
            TargetError::Ambiguous(c) => {
                assert_eq!(c.len(), 2);
                // The message has to name the alternatives, or the caller
                // cannot narrow the selector without another round trip.
                let msg = TargetError::Ambiguous(c).to_string();
                assert!(msg.contains("id=2"), "{msg}");
                assert!(msg.contains("id=3"), "{msg}");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn focus_breaks_a_tie_deliberately() {
        let mut windows = sample();
        windows[2].focus = true;
        let t = WindowTarget {
            app_id: Some("firefox".into()),
            ..Default::default()
        };
        assert_eq!(t.resolve(&windows).unwrap().id, 3);
    }

    #[test]
    fn selectors_combine_with_and() {
        let t = WindowTarget {
            app_id: Some("firefox".into()),
            title: Some("gdr".into()),
            ..Default::default()
        };
        assert_eq!(t.resolve(&sample()).unwrap().id, 2);
    }

    #[test]
    fn visible_in_capture_needs_a_stream_region() {
        let mut w = win(1, "a", "a", "a");
        assert!(!w.visible_in_capture(), "no stream_region yet");
        w.stream_region = Some(Region {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        });
        assert!(w.visible_in_capture());
        w.minimized = true;
        assert!(!w.visible_in_capture());
    }

    #[test]
    fn window_op_wire_shape_is_flat() {
        let v = serde_json::to_value(WindowOp::Move { x: 10, y: 20 }).unwrap();
        assert_eq!(v["action"], "move");
        assert_eq!(v["x"], 10);
        let back: WindowOp = serde_json::from_value(v).unwrap();
        assert_eq!(back, WindowOp::Move { x: 10, y: 20 });

        let v = serde_json::to_value(WindowOp::Activate).unwrap();
        assert_eq!(v["action"], "activate");
    }

    #[test]
    fn window_op_args_json_matches_extension_contract() {
        assert_eq!(WindowOp::Activate.args_json(), "{}");
        assert_eq!(
            WindowOp::MoveResize {
                x: 1,
                y: 2,
                width: 3,
                height: 4
            }
            .args_json(),
            r#"{"x":1,"y":2,"width":3,"height":4}"#
        );
        assert_eq!(WindowOp::Workspace { index: 2 }.name(), "workspace");
    }
}
