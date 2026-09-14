//! Subscription hooks: standing watches that gdrd runs *between* requests.
//!
//! Every other request in this protocol is a question the controller asks and
//! the daemon answers immediately. A hook is the opposite shape: the agent
//! says once "tell me when the screen moves near here" or "tell me when this
//! app opens a window", gdrd keeps watching on its own, and the agent drains a
//! journal whenever it likes — or blocks on [`crate::Request::HookPoll`] with
//! `wait_ms` and is woken the moment something lands.
//!
//! Two kinds, because the two useful questions are genuinely different:
//!
//! * [`HookKind::Activity`] — *pixels changed, roughly here*. Derived from the
//!   same capture stream screenshots come from, so it carries the Screenshot
//!   scope. The report is a **circle** in capture-stream pixels rather than a
//!   rectangle: change is reported as a place to look, and a circle says
//!   "centred there, about this big" without implying the pixel-exact edges a
//!   rect does.
//! * [`HookKind::Window`] — *a window opened, closed, moved, resized, was
//!   retitled*. Metadata only, so it carries the Window scope — titles and
//!   process owners, never pixels.
//!
//! ## Buffer time
//!
//! Both kinds take `buffer_ms`: how long the thing being watched has to hold
//! still before the event is emitted. This is the difference between one
//! useful event and four hundred useless ones — a window drag-resize emits a
//! geometry change per frame, and a scrolling page repaints continuously. The
//! event that finally arrives describes the *whole* burst (where it started,
//! where it ended, how long it ran), and by construction the state it reports
//! has been stable for `buffer_ms`, which is what makes it safe to act on.
//!
//! A burst that never goes quiet would otherwise never be reported, so both
//! kinds also carry `max_burst_ms`: at that point the burst is emitted anyway,
//! flagged `settled: false`, and a new one starts. An unsettled event means
//! "this was still moving when I told you", and is the one case where a
//! follow-up screenshot may disagree with the report.

use serde::{Deserialize, Serialize};

use crate::scopes::Scope;
use crate::windows::{LogicalRect, WindowTarget};
use crate::Region;

/// Which of the two watchers a hook drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    Activity,
    Window,
}

impl HookKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HookKind::Activity => "activity",
            HookKind::Window => "window",
        }
    }

    /// Scope a token must hold to create, change, read or drain this hook.
    ///
    /// Activity is Screenshot rather than a scope of its own: it is computed
    /// from the capture stream, and "the screen changed in this circle" is a
    /// (very coarse) read of the screen. A token that cannot screenshot must
    /// not be able to watch the screen move either.
    pub fn required_scope(self) -> Scope {
        match self {
            HookKind::Activity => Scope::Screenshot,
            HookKind::Window => Scope::Window,
        }
    }
}

impl std::fmt::Display for HookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default sampling cadence for the activity watcher, in milliseconds.
pub const ACTIVITY_POLL_MS: u64 = 120;
/// Default quiet period before an activity burst is reported.
pub const ACTIVITY_BUFFER_MS: u64 = 400;
/// Default ceiling on one activity burst.
pub const ACTIVITY_MAX_BURST_MS: u64 = 5_000;
/// Default polling cadence for the window watcher.
pub const WINDOW_POLL_MS: u64 = 250;
/// Default quiet period before a geometry change is reported.
pub const WINDOW_BUFFER_MS: u64 = 250;
/// Default ceiling on one coalesced window burst.
pub const WINDOW_MAX_BURST_MS: u64 = 5_000;
/// Ceiling on a [`crate::Request::HookPoll`] long-poll.
///
/// Deliberately above the largest `buffer_ms` a hook can be given (60 s), so
/// that "block until the next event" is always a wait a caller can actually
/// make. The previous 30 s cap allowed a hook whose settle time exceeded the
/// longest poll anyone could issue — which looks exactly like a hook that
/// does not work.
pub const MAX_POLL_WAIT_MS: u64 = 120_000;
/// How many events gdrd keeps for pollers that are not currently draining.
pub const HOOK_JOURNAL_CAPACITY: usize = 1024;

/// Where an activity hook is allowed to look.
///
/// `window` is the interesting one: the region is re-derived from the
/// window's live geometry on every sample, so a watch on "the terminal"
/// follows the terminal when it is moved rather than watching the patch of
/// desktop it used to occupy.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ActivitySpec {
    /// Watch only inside this window, tracking it as it moves.
    #[serde(default)]
    pub target: Option<WindowTarget>,
    /// Watch only inside this fixed rectangle of capture-stream pixels.
    /// Ignored when `target` is set.
    #[serde(default)]
    pub region: Option<Region>,
    /// Quiet period, in ms, before a burst is closed and reported.
    #[serde(default = "default_activity_buffer")]
    pub buffer_ms: u64,
    /// Report a burst anyway once it has run this long. 0 disables the cap.
    #[serde(default = "default_activity_max_burst")]
    pub max_burst_ms: u64,
    /// How often to sample the stream.
    #[serde(default = "default_activity_poll")]
    pub poll_ms: u64,
    /// Per-cell luma delta (0..=255) that counts as "this cell changed".
    ///
    /// This interacts with `grid` and the two cannot be tuned separately: a
    /// cell is the *mean* brightness of its area, so the coarser the grid,
    /// the more a small change is diluted before it is compared. At the
    /// default 64-cell grid a cell covers ~30x30 native pixels, and a few
    /// characters changing inside one moves its mean by single digits — far
    /// under a threshold of 12. Watching text means raising `grid` and
    /// lowering `threshold` together.
    #[serde(default = "default_threshold")]
    pub threshold: u8,
    /// How many cells must change before a sample counts as activity at all.
    /// 1 catches a blinking caret; raise it to ignore small ornaments.
    #[serde(default = "default_min_cells")]
    pub min_cells: u32,
    /// Diff grid resolution along the long edge. 64 puts a cell at roughly
    /// 30x30 native pixels on a 1920-wide stream — fine enough to place a
    /// dialog, coarse enough to be free.
    #[serde(default = "default_grid")]
    pub grid: u32,
    /// Never report more often than this, in ms. 0 (default) is off.
    ///
    /// For a region that repaints on a timer — a clock, a progress bar, a
    /// blinking panel — `buffer_ms` does not help: each repaint is its own
    /// settled burst, so a 2 Hz flash produces two events a second, all
    /// identical. This is a floor on the reporting rate, and it coalesces
    /// rather than drops: the burst keeps accumulating while it is held
    /// back, so the event that eventually comes out covers everything since
    /// the last one.
    #[serde(default)]
    pub min_interval_ms: u64,
    /// Drop bursts whose circle is larger than this radius, in stream px.
    /// A full-screen repaint (workspace switch, video) is rarely the thing
    /// the agent subscribed for. 0 keeps everything.
    #[serde(default)]
    pub max_radius: u32,
}

fn default_activity_buffer() -> u64 {
    ACTIVITY_BUFFER_MS
}
fn default_activity_max_burst() -> u64 {
    ACTIVITY_MAX_BURST_MS
}
fn default_activity_poll() -> u64 {
    ACTIVITY_POLL_MS
}
fn default_threshold() -> u8 {
    // The old comment here warned that "below ~8 the compositor's own noise
    // reads as motion". Measured rather than assumed: an idle headless GNOME
    // session at grid 256 and threshold 1 produced zero events in 20
    // seconds. The stream is damage-driven, so a screen that is not
    // repainting sends nothing at all and there is no noise floor to clear.
    // The floor that does exist belongs to whatever is animating on that
    // particular desktop, which no default can predict.
    //
    // 12 stays because nothing has shown it to be wrong. It survived one
    // report of "the hook sees nothing", which turned out to be a browser
    // tab in the background: Chromium does not repaint an occluded tab, so
    // there was no change on screen to detect at any threshold.
    12
}
fn default_min_cells() -> u32 {
    1
}
fn default_grid() -> u32 {
    64
}

impl ActivitySpec {
    /// Clamp user input to ranges the watcher can actually honour.
    ///
    /// Silently, and deliberately: a `poll_ms: 0` typo should not become a
    /// spin loop inside the daemon, and refusing the hook outright over a
    /// tunable is worse than running it slightly slower than asked.
    pub fn normalized(mut self) -> Self {
        self.poll_ms = self.poll_ms.clamp(30, 10_000);
        self.buffer_ms = self.buffer_ms.min(60_000);
        if self.max_burst_ms > 0 {
            self.max_burst_ms = self.max_burst_ms.clamp(self.poll_ms, 600_000);
        }
        self.grid = self.grid.clamp(8, 256);
        self.min_interval_ms = self.min_interval_ms.min(600_000);
        self.min_cells = self.min_cells.max(1);
        self.threshold = self.threshold.max(1);
        self
    }
}

/// Window lifecycle events a hook can subscribe to.
///
/// Stored as strings on the wire so an older daemon meeting a newer client
/// says "unknown event" instead of failing to parse the whole subscription.
pub const WINDOW_EVENT_KINDS: [&str; 9] = [
    "opened",
    "closed",
    "resized",
    "moved",
    "retitled",
    "focused",
    "minimized",
    "unminimized",
    "workspace",
];

/// Events subscribed to by default: the three the agent almost always means.
pub const WINDOW_EVENTS_DEFAULT: [&str; 3] = ["opened", "closed", "resized"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowHookSpec {
    /// Only report windows matching this selector. Unset watches every
    /// window on the desktop.
    #[serde(default)]
    pub target: Option<WindowTarget>,
    /// Which of [`WINDOW_EVENT_KINDS`] to report.
    #[serde(default = "default_window_events")]
    pub events: Vec<String>,
    /// Quiet period, in ms, before a coalesced geometry change is reported.
    /// A drag-resize emits one event at the end instead of one per frame.
    #[serde(default = "default_window_buffer")]
    pub buffer_ms: u64,
    /// Report a coalesced burst anyway once it has run this long.
    #[serde(default = "default_window_max_burst")]
    pub max_burst_ms: u64,
    /// How often to re-list windows. Open/close also wake the watcher
    /// immediately via the extension's `Changed` signal, so this cadence
    /// really only bounds how fast a *resize* is noticed.
    #[serde(default = "default_window_poll")]
    pub poll_ms: u64,
    /// Include docks, panels and notifications, which are normally noise.
    #[serde(default)]
    pub include_skip_taskbar: bool,
    /// Ignore geometry changes smaller than this many logical pixels.
    #[serde(default = "default_geometry_threshold")]
    pub geometry_threshold: u32,
    /// Look up the owning process (user, exe, cmdline) for each event.
    /// On by default: "which process owns this window" is most of the value
    /// of knowing a window appeared, and it is a `/proc` read, not a probe.
    #[serde(default = "default_true")]
    pub include_process: bool,
}

fn default_window_events() -> Vec<String> {
    WINDOW_EVENTS_DEFAULT.iter().map(|s| s.to_string()).collect()
}
fn default_window_buffer() -> u64 {
    WINDOW_BUFFER_MS
}
fn default_window_max_burst() -> u64 {
    WINDOW_MAX_BURST_MS
}
fn default_window_poll() -> u64 {
    WINDOW_POLL_MS
}
fn default_geometry_threshold() -> u32 {
    2
}
fn default_true() -> bool {
    true
}

impl Default for WindowHookSpec {
    fn default() -> Self {
        Self {
            target: None,
            events: default_window_events(),
            buffer_ms: WINDOW_BUFFER_MS,
            max_burst_ms: WINDOW_MAX_BURST_MS,
            poll_ms: WINDOW_POLL_MS,
            include_skip_taskbar: false,
            geometry_threshold: default_geometry_threshold(),
            include_process: true,
        }
    }
}

impl WindowHookSpec {
    /// Reject unknown event names and clamp cadences.
    ///
    /// Unknown names *are* rejected, unlike the numeric tunables: a hook
    /// asked to watch "resize" (the name that is not `resized`) would sit
    /// there reporting nothing, and looking correct while doing it.
    pub fn normalized(mut self) -> Result<Self, String> {
        if self.events.is_empty() {
            self.events = default_window_events();
        }
        let mut seen: Vec<String> = Vec::new();
        for e in &self.events {
            let name = e.trim().to_ascii_lowercase();
            if !WINDOW_EVENT_KINDS.contains(&name.as_str()) {
                return Err(format!(
                    "unknown window event '{e}' — pick from {}",
                    WINDOW_EVENT_KINDS.join(", ")
                ));
            }
            if !seen.contains(&name) {
                seen.push(name);
            }
        }
        self.events = seen;
        self.poll_ms = self.poll_ms.clamp(50, 10_000);
        self.buffer_ms = self.buffer_ms.min(60_000);
        if self.max_burst_ms > 0 {
            self.max_burst_ms = self.max_burst_ms.clamp(self.poll_ms, 600_000);
        }
        Ok(self)
    }

    pub fn wants(&self, kind: &str) -> bool {
        self.events.iter().any(|e| e == kind)
    }
}

/// What to watch. The `kind` tag is what the two watchers dispatch on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookSpec {
    Activity(ActivitySpec),
    Window(WindowHookSpec),
}

impl HookSpec {
    pub fn kind(&self) -> HookKind {
        match self {
            HookSpec::Activity(_) => HookKind::Activity,
            HookSpec::Window(_) => HookKind::Window,
        }
    }

    pub fn required_scope(&self) -> Scope {
        self.kind().required_scope()
    }

    pub fn normalized(self) -> Result<Self, String> {
        Ok(match self {
            HookSpec::Activity(a) => HookSpec::Activity(a.normalized()),
            HookSpec::Window(w) => HookSpec::Window(w.normalized()?),
        })
    }

    pub fn buffer_ms(&self) -> u64 {
        match self {
            HookSpec::Activity(a) => a.buffer_ms,
            HookSpec::Window(w) => w.buffer_ms,
        }
    }

    pub fn poll_ms(&self) -> u64 {
        match self {
            HookSpec::Activity(a) => a.poll_ms,
            HookSpec::Window(w) => w.poll_ms,
        }
    }

    /// One-line summary for logs, audit lines and `gdr hooks`.
    pub fn describe(&self) -> String {
        match self {
            HookSpec::Activity(a) => {
                let where_ = match (&a.target, &a.region) {
                    (Some(t), _) => format!("window[{}]", t.describe()),
                    (None, Some(r)) => {
                        format!("region {}x{}+{}+{}", r.width, r.height, r.x, r.y)
                    }
                    (None, None) => "whole screen".to_string(),
                };
                format!("activity on {where_}, buffer {}ms", a.buffer_ms)
            }
            HookSpec::Window(w) => {
                let who = match &w.target {
                    Some(t) => format!("window[{}]", t.describe()),
                    None => "any window".to_string(),
                };
                format!(
                    "{} on {who}, buffer {}ms",
                    w.events.join("/"),
                    w.buffer_ms
                )
            }
        }
    }
}

/// A circle in capture-stream pixels — the coordinate space `gdr_click` and
/// `Region` already use, so `center` can be clicked without conversion.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Circle {
    pub x: f64,
    pub y: f64,
    pub radius: f64,
}

/// Where an activity report was measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ActivityScope {
    Screen,
    Region {
        region: Region,
    },
    Window {
        id: u64,
        title: Option<String>,
        app_id: Option<String>,
        region: Region,
    },
}

/// One settled burst of screen change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityReport {
    /// Affected area, as a circle covering everything that changed.
    pub circle: Circle,
    /// The same area as the bounding box it was derived from, for callers
    /// that want to crop rather than aim.
    pub bbox: Region,
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: u64,
    /// Quiet period this burst actually waited out before being reported.
    pub buffer_ms: u64,
    /// Samples in this burst that showed change.
    pub samples: u32,
    /// Share of the watched area that changed, at the burst's peak.
    pub changed_fraction: f64,
    pub area: ActivityScope,
    /// Full stream dimensions, so a circle can be placed on a resized shot.
    pub stream_width: u32,
    pub stream_height: u32,
    /// False when `max_burst_ms` cut the burst off while it was still
    /// moving — the screen may already look different from this report.
    pub settled: bool,
    /// Whether the session was locked when this was measured.
    ///
    /// The capture stream keeps running across a lock — the lock screen is
    /// composited like anything else — so without this an agent watching a
    /// desktop cannot tell that what it is now watching is a clock on a
    /// shield, and every circle it chases is on the wrong screen. `None`
    /// means gdrd could not ask (no session bus).
    #[serde(default)]
    pub session_locked: Option<bool>,
}

/// The process behind a window, as `/proc` describes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub pid: i32,
    #[serde(default)]
    pub uid: Option<u32>,
    /// Login name for `uid`, resolved from `/etc/passwd`.
    #[serde(default)]
    pub user: Option<String>,
    /// `/proc/pid/comm` — the short name, e.g. `chromium`.
    #[serde(default)]
    pub comm: Option<String>,
    /// Resolved `/proc/pid/exe`.
    #[serde(default)]
    pub exe: Option<String>,
    /// Full argv, space-joined and truncated.
    #[serde(default)]
    pub cmdline: Option<String>,
    /// Parent pid, for "which shell launched this".
    #[serde(default)]
    pub ppid: Option<i32>,
    /// Why the lookup came back empty, when it did.
    #[serde(default)]
    pub error: Option<String>,
}

/// Window geometry before a `resized` / `moved` / `retitled` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowChange {
    #[serde(default)]
    pub frame_rect: Option<LogicalRect>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub minimized: Option<bool>,
    #[serde(default)]
    pub workspace: Option<i32>,
    /// Signed size delta in logical pixels, for `resized`.
    #[serde(default)]
    pub dw: Option<i32>,
    #[serde(default)]
    pub dh: Option<i32>,
    /// Signed position delta in logical pixels, for `moved`.
    #[serde(default)]
    pub dx: Option<i32>,
    #[serde(default)]
    pub dy: Option<i32>,
}

/// Everything a window hook reports about the window an event concerns.
///
/// Deliberately self-contained rather than "an id you can look up": for a
/// `closed` event the window is already gone by the time the agent reads
/// this, and a bare id would be unanswerable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowEventInfo {
    pub id: u64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub wm_class: Option<String>,
    pub pid: i32,
    #[serde(default)]
    pub process: Option<ProcessInfo>,
    #[serde(default)]
    pub window_type: String,
    /// Size and position in logical pixels, at the moment it settled.
    pub frame_rect: LogicalRect,
    /// The same rect in capture-stream pixels — `None` when the window is
    /// not on the monitor gdrd streams, so a crop would show the wrong
    /// desktop.
    #[serde(default)]
    pub stream_region: Option<Region>,
    #[serde(default)]
    pub monitor: i32,
    #[serde(default)]
    pub workspace: Option<i32>,
    #[serde(default)]
    pub minimized: bool,
    #[serde(default)]
    pub focus: bool,
    #[serde(default)]
    pub maximized: String,
    #[serde(default)]
    pub fullscreen: bool,
    /// State before the change, for the events that describe one.
    #[serde(default)]
    pub previous: Option<WindowChange>,
    /// Samples coalesced into this event by `buffer_ms`.
    #[serde(default)]
    pub samples: u32,
    /// False when `max_burst_ms` cut a still-moving drag short.
    #[serde(default = "default_true")]
    pub settled: bool,
}

/// One journal entry. `hook_id` is what a multi-hook drain sorts on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookEvent {
    /// Monotonic across every hook on this daemon; pass the reply's
    /// `next_seq` back as `since` to resume without gaps or repeats.
    pub seq: u64,
    pub hook_id: String,
    pub hook_kind: HookKind,
    #[serde(default)]
    pub label: Option<String>,
    /// `activity` for the screen watcher; one of [`WINDOW_EVENT_KINDS`]
    /// for the window watcher.
    pub kind: String,
    /// RFC-3339, stamped by gdrd when the burst settled.
    pub at: String,
    #[serde(default)]
    pub activity: Option<ActivityReport>,
    #[serde(default)]
    pub window: Option<WindowEventInfo>,
}

/// Whether a hook's watcher is actually running, and why not when it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookState {
    /// Enabled and sampling.
    Watching,
    /// Toggled off. Kept, with its config, so it can be switched back on.
    Paused,
    /// Enabled, but there is nothing to observe *yet* — the watched window
    /// is not open, the capture stream has not produced a first frame. This
    /// state clears on its own; nobody needs to do anything.
    Waiting,
    /// Enabled, and the last attempt errored. `last_error` says why, and it
    /// needs a human or an agent to act: the shell extension went away, the
    /// session is locked, ScreenCast was refused. The distinction from
    /// `Waiting` is the whole point — a hook that will never report again
    /// must not look like one that is simply having a quiet minute.
    Failing,
}

impl HookState {
    pub fn as_str(self) -> &'static str {
        match self {
            HookState::Watching => "watching",
            HookState::Paused => "paused",
            HookState::Waiting => "waiting",
            HookState::Failing => "failing",
        }
    }
}

/// A hook as the daemon currently holds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookStatus {
    pub id: String,
    pub kind: HookKind,
    #[serde(default)]
    pub label: Option<String>,
    /// The toggle. A disabled hook keeps its id, config and buffered events.
    pub enabled: bool,
    pub state: HookState,
    pub spec: HookSpec,
    /// One-line human summary of `spec`.
    pub summary: String,
    pub created_at: String,
    pub updated_at: String,
    /// Scope this hook needs — the one its creator had to hold, and the one
    /// every later read of it is checked against.
    pub required_scope: String,
    /// Scopes the token that created it held at the time. Kept so that
    /// "what is watching my desktop, and under whose authority" has an
    /// answer that does not depend on the token still existing.
    pub created_with_scopes: Vec<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    pub events_emitted: u64,
    #[serde(default)]
    pub last_event_at: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Events from this hook still in the journal.
    pub buffered: u32,
}

/// Reply to [`crate::Request::HookPoll`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookPollResult {
    pub events: Vec<HookEvent>,
    /// Pass back as `since` on the next poll **of the same shape**.
    ///
    /// Sequence numbers are global across hooks, so this cursor belongs to
    /// the filter that produced it: a cursor from a drain of one hook has
    /// walked past nothing, but it also has not seen the other hooks'
    /// events, and reusing it on an unfiltered poll would skip them.
    /// [`HookPollResult::cursor_scope`] says which filter it is for.
    pub next_seq: u64,
    /// `None` for a drain of every hook, otherwise the hook id it was
    /// filtered to — and therefore the only poll `next_seq` is valid for.
    #[serde(default)]
    pub cursor_scope: Option<String>,
    /// Events from *other* hooks that lie inside the range this filtered
    /// drain covered. They are still waiting; they were not consumed. Said
    /// out loud because the failure it warns about is silent.
    #[serde(default)]
    pub skipped_other_hooks: u32,
    /// The caller fell behind the journal and lost events.
    pub dropped: bool,
    /// Hooks the caller's token is allowed to see, with live counters.
    pub hooks: Vec<HookStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_spec_defaults_survive_an_empty_object() {
        let spec: HookSpec = serde_json::from_str(r#"{"kind":"activity"}"#).unwrap();
        let HookSpec::Activity(a) = &spec else {
            panic!("wrong kind")
        };
        assert_eq!(a.buffer_ms, ACTIVITY_BUFFER_MS);
        assert_eq!(a.poll_ms, ACTIVITY_POLL_MS);
        assert_eq!(spec.required_scope(), Scope::Screenshot);
    }

    #[test]
    fn window_spec_defaults_to_open_close_resize() {
        let spec: HookSpec = serde_json::from_str(r#"{"kind":"window"}"#).unwrap();
        let HookSpec::Window(w) = &spec else {
            panic!("wrong kind")
        };
        assert!(w.wants("opened") && w.wants("closed") && w.wants("resized"));
        assert!(!w.wants("focused"));
        assert_eq!(spec.required_scope(), Scope::Window);
    }

    #[test]
    fn a_zero_poll_is_clamped_rather_than_spinning() {
        let a = ActivitySpec {
            poll_ms: 0,
            grid: 4000,
            min_cells: 0,
            ..Default::default()
        }
        .normalized();
        assert!(a.poll_ms >= 30);
        assert!(a.grid <= 256);
        assert_eq!(a.min_cells, 1);
    }

    #[test]
    fn max_burst_is_never_shorter_than_one_sample() {
        let a = ActivitySpec {
            poll_ms: 500,
            max_burst_ms: 10,
            ..Default::default()
        }
        .normalized();
        assert!(a.max_burst_ms >= a.poll_ms);
    }

    #[test]
    fn a_misspelled_event_is_refused_not_silently_ignored() {
        // "resize" watches nothing and looks fine doing it, which is the
        // worst possible outcome for a subscription.
        let err = WindowHookSpec {
            events: vec!["resize".into()],
            ..Default::default()
        }
        .normalized()
        .unwrap_err();
        assert!(err.contains("resize"), "{err}");
        assert!(err.contains("resized"), "{err}");
    }

    #[test]
    fn duplicate_events_collapse_and_case_is_ignored() {
        let w = WindowHookSpec {
            events: vec!["Opened".into(), "opened".into(), "CLOSED".into()],
            ..Default::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(w.events, vec!["opened".to_string(), "closed".to_string()]);
    }

    #[test]
    fn spec_roundtrips_through_the_wire_shape() {
        let spec = HookSpec::Window(WindowHookSpec {
            target: Some(WindowTarget {
                app_id: Some("chromium".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let v: serde_json::Value = serde_json::to_value(&spec).unwrap();
        assert_eq!(v["kind"], "window");
        assert_eq!(v["target"]["app_id"], "chromium");
        let back: HookSpec = serde_json::from_value(v).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn describe_names_the_window_and_the_buffer() {
        let spec = HookSpec::Activity(ActivitySpec {
            target: Some(WindowTarget {
                title: Some("Inbox".into()),
                ..Default::default()
            }),
            buffer_ms: 750,
            ..Default::default()
        });
        let s = spec.describe();
        assert!(s.contains("Inbox"), "{s}");
        assert!(s.contains("750ms"), "{s}");
    }
}
