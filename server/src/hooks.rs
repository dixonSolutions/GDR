//! Standing subscriptions: the two watchers that run between requests.
//!
//! gdrd is otherwise strictly request/response — nothing happens unless a
//! controller asks. A hook inverts that for two specific questions:
//!
//! * **activity** — sample the capture stream on a cadence, diff successive
//!   frames as a coarse luma grid, and report a *circle* covering whatever
//!   moved, once it has held still for `buffer_ms`.
//! * **window** — re-list windows on a cadence, diff the list, and report
//!   opens, closes, resizes, moves and retitles, with the owning process.
//!
//! Both watchers are one task each for the whole daemon, not one per hook:
//! ten activity hooks still cost one probe per round, and when nothing is
//! enabled both tasks park on a `Notify` and cost nothing at all.
//!
//! ## Why polling, and not compositor signals
//!
//! The window extension does emit a `Changed` signal, and `WindowEvents`
//! already uses it. It carries opens, closes and focus changes — but not
//! geometry, which is most of what a subscription is for. Diffing the list
//! gets resize, move, retitle and workspace changes out of the extension
//! that is already deployed, rather than out of one the target has to
//! install and log out for. The cadence is the cost: an open is noticed
//! within `poll_ms`, not instantly.
//!
//! ## The buffer, and what "settled" means
//!
//! Every report waits for the thing it describes to stop changing for
//! `buffer_ms` first. That is the whole point of the buffer: the agent is
//! told about a window *after* it finished animating to its final size, so
//! the geometry in the event is geometry it can click on. A burst that never
//! goes quiet is emitted at `max_burst_ms` anyway with `settled: false`,
//! which is the one case where the report may already be out of date.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use common::hooks::{
    ActivityReport, ActivityScope, ActivitySpec, Circle, HookEvent, HookKind, HookPollResult,
    HookSpec, HookState, HookStatus, ProcessInfo, WindowChange, WindowEventInfo, WindowHookSpec,
    HOOK_JOURNAL_CAPACITY, MAX_POLL_WAIT_MS,
};
use common::windows::WindowInfo;
use common::{Region, ScopeSet};
use tokio::sync::{Mutex, Notify};

use crate::capture::{self, ProbeFrame};
use crate::display::SharedDisplay;
use crate::windows::WindowPlane;

/// How long a parked watcher waits before re-checking, as a backstop for a
/// missed wake-up. Nothing depends on it for correctness.
const IDLE_RECHECK: Duration = Duration::from_secs(5);

/// One hook, as the registry holds it.
struct Hook {
    id: String,
    label: Option<String>,
    spec: HookSpec,
    enabled: bool,
    state: HookState,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    created_with_scopes: Vec<String>,
    created_by: Option<String>,
    events_emitted: u64,
    last_event_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
}

impl Hook {
    fn status(&self, buffered: u32) -> HookStatus {
        HookStatus {
            id: self.id.clone(),
            kind: self.spec.kind(),
            label: self.label.clone(),
            enabled: self.enabled,
            state: self.state,
            summary: self.spec.describe(),
            spec: self.spec.clone(),
            created_at: self.created_at.to_rfc3339(),
            updated_at: self.updated_at.to_rfc3339(),
            required_scope: self.spec.required_scope().as_str().to_string(),
            created_with_scopes: self.created_with_scopes.clone(),
            created_by: self.created_by.clone(),
            events_emitted: self.events_emitted,
            last_event_at: self.last_event_at.map(|t| t.to_rfc3339()),
            last_error: self.last_error.clone(),
            buffered,
        }
    }
}

#[derive(Default)]
struct Inner {
    hooks: BTreeMap<String, Hook>,
    journal: VecDeque<HookEvent>,
    next_seq: u64,
    /// Lowest sequence still in the journal. A `since` below this means the
    /// caller fell behind and lost events, which they are told about rather
    /// than being handed a silently incomplete history.
    oldest_seq: u64,
    counter: u64,
}

/// Every hook on this daemon, plus the shared event journal.
///
/// One journal for all hooks, with one global sequence: a single
/// `HookPoll` then drains every subscription in order, and an agent
/// watching three things does not have to track three cursors.
pub struct HookRegistry {
    inner: Mutex<Inner>,
    /// Woken when events land — this is what `wait_ms` sleeps on.
    events: Notify,
    /// Woken when a hook is created, toggled or removed, so a parked
    /// watcher starts sampling without waiting out `IDLE_RECHECK`.
    changed: Notify,
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            events: Notify::new(),
            changed: Notify::new(),
        }
    }

    /// Refuse a hook the token has no business seeing.
    ///
    /// Phrased as a permission error rather than "no such hook": the caller
    /// holds a token that genuinely cannot see this, and pretending the hook
    /// does not exist would send them off creating a duplicate.
    fn check_scope(spec_kind: HookKind, scopes: &ScopeSet, id: &str) -> Result<()> {
        let needed = spec_kind.required_scope();
        if scopes.contains(needed) {
            return Ok(());
        }
        Err(anyhow!(
            "permission denied: hook '{id}' is a {spec_kind} hook and needs scope \
             '{needed}' (this token has {scopes})"
        ))
    }

    pub async fn create(
        &self,
        spec: HookSpec,
        label: Option<String>,
        enabled: bool,
        scopes: &ScopeSet,
        created_by: Option<String>,
    ) -> Result<HookStatus> {
        let spec = spec.normalized().map_err(|e| anyhow!("{e}"))?;
        let needed = spec.required_scope();
        if !scopes.contains(needed) {
            return Err(anyhow!(
                "permission denied: a {} hook needs scope '{needed}' (this token has {scopes})",
                spec.kind()
            ));
        }
        let now = Utc::now();
        let mut inner = self.inner.lock().await;
        inner.counter += 1;
        let id = format!("{}-{}", spec.kind(), inner.counter);
        let hook = Hook {
            id: id.clone(),
            label,
            state: if enabled {
                HookState::Waiting
            } else {
                HookState::Paused
            },
            spec,
            enabled,
            created_at: now,
            updated_at: now,
            created_with_scopes: scopes.to_string_list(),
            created_by,
            events_emitted: 0,
            last_event_at: None,
            last_error: None,
        };
        let status = hook.status(0);
        inner.hooks.insert(id, hook);
        drop(inner);
        self.changed.notify_waiters();
        Ok(status)
    }

    pub async fn update(
        &self,
        id: &str,
        enabled: Option<bool>,
        label: Option<String>,
        spec: Option<HookSpec>,
        scopes: &ScopeSet,
    ) -> Result<HookStatus> {
        let spec = match spec {
            Some(s) => Some(s.normalized().map_err(|e| anyhow!("{e}"))?),
            None => None,
        };
        let mut inner = self.inner.lock().await;
        let buffered = inner.buffered_for(id);
        let hook = inner
            .hooks
            .get_mut(id)
            .ok_or_else(|| anyhow!("no hook '{id}' — list them with HookList"))?;
        Self::check_scope(hook.spec.kind(), scopes, id)?;
        if let Some(new_spec) = spec {
            if new_spec.kind() != hook.spec.kind() {
                return Err(anyhow!(
                    "hook '{id}' is a {} hook; remove it and create a {} hook instead \
                     (changing kind in place would silently reset what it watches)",
                    hook.spec.kind(),
                    new_spec.kind()
                ));
            }
            Self::check_scope(new_spec.kind(), scopes, id)?;
            hook.spec = new_spec;
            // The watcher's baseline belongs to the old configuration; drop
            // the error from it so a re-enabled hook does not report a stale
            // reason for being unhappy.
            hook.last_error = None;
        }
        if let Some(l) = label {
            hook.label = Some(l);
        }
        if let Some(on) = enabled {
            hook.enabled = on;
            hook.state = if on {
                HookState::Waiting
            } else {
                HookState::Paused
            };
            if !on {
                hook.last_error = None;
            }
        }
        hook.updated_at = Utc::now();
        let status = hook.status(buffered);
        drop(inner);
        self.changed.notify_waiters();
        Ok(status)
    }

    pub async fn remove(&self, id: &str, scopes: &ScopeSet) -> Result<HookStatus> {
        let mut inner = self.inner.lock().await;
        let buffered = inner.buffered_for(id);
        let kind = inner
            .hooks
            .get(id)
            .map(|h| h.spec.kind())
            .ok_or_else(|| anyhow!("no hook '{id}' — list them with HookList"))?;
        Self::check_scope(kind, scopes, id)?;
        let hook = inner.hooks.remove(id).expect("checked above");
        let status = hook.status(buffered);
        inner.journal.retain(|e| e.hook_id != id);
        // Dropping this hook's events leaves the journal starting later than
        // it did; a stale `oldest_seq` would then under-report `dropped` for
        // everyone else.
        if let Some(front) = inner.journal.front().map(|e| e.seq) {
            inner.oldest_seq = front;
        }
        drop(inner);
        self.changed.notify_waiters();
        Ok(status)
    }

    /// Hooks this token may see. A screenshot-only token is not told that
    /// window hooks exist, and vice versa.
    pub async fn list(&self, scopes: &ScopeSet) -> Vec<HookStatus> {
        let inner = self.inner.lock().await;
        inner.visible(scopes)
    }

    pub async fn poll(
        &self,
        id: Option<&str>,
        since: u64,
        limit: u32,
        wait_ms: u64,
        scopes: &ScopeSet,
    ) -> Result<HookPollResult> {
        if let Some(id) = id {
            let inner = self.inner.lock().await;
            let kind = inner
                .hooks
                .get(id)
                .map(|h| h.spec.kind())
                .ok_or_else(|| anyhow!("no hook '{id}' — list them with HookList"))?;
            Self::check_scope(kind, scopes, id)?;
        }
        let limit = if limit == 0 { 100 } else { limit.min(512) };
        let deadline = Instant::now() + Duration::from_millis(wait_ms.min(MAX_POLL_WAIT_MS));

        loop {
            // Subscribe before reading, or an event landing between the read
            // and the wait is slept through despite having already happened.
            let waiter = self.events.notified();
            {
                let inner = self.inner.lock().await;
                let result = inner.drain(id, since, limit, scopes);
                if !result.events.is_empty() || wait_ms == 0 {
                    return Ok(result);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let inner = self.inner.lock().await;
                return Ok(inner.drain(id, since, limit, scopes));
            }
            let _ = tokio::time::timeout(left, waiter).await;
        }
    }

    /// Enabled hooks of one kind, as `(id, spec)` — what a watcher works from.
    async fn enabled_of_kind(&self, kind: HookKind) -> Vec<(String, HookSpec)> {
        let inner = self.inner.lock().await;
        inner
            .hooks
            .values()
            .filter(|h| h.enabled && h.spec.kind() == kind)
            .map(|h| (h.id.clone(), h.spec.clone()))
            .collect()
    }

    /// Park until a hook of this kind is enabled.
    async fn wait_for_work(&self, kind: HookKind) -> Vec<(String, HookSpec)> {
        loop {
            let waiter = self.changed.notified();
            let work = self.enabled_of_kind(kind).await;
            if !work.is_empty() {
                return work;
            }
            let _ = tokio::time::timeout(IDLE_RECHECK, waiter).await;
        }
    }

    async fn set_state(&self, id: &str, state: HookState, error: Option<String>) {
        let mut inner = self.inner.lock().await;
        if let Some(hook) = inner.hooks.get_mut(id) {
            hook.state = state;
            hook.last_error = error;
        }
    }

    /// Append one event and wake every blocked poller.
    async fn emit(&self, id: &str, kind: &str, at: DateTime<Utc>, payload: EventPayload) {
        let mut inner = self.inner.lock().await;
        let Some(hook) = inner.hooks.get_mut(id) else {
            return;
        };
        hook.events_emitted += 1;
        hook.last_event_at = Some(at);
        hook.state = HookState::Watching;
        hook.last_error = None;
        let (hook_kind, label) = (hook.spec.kind(), hook.label.clone());

        inner.next_seq += 1;
        let seq = inner.next_seq;
        let event = HookEvent {
            seq,
            hook_id: id.to_string(),
            hook_kind,
            label,
            kind: kind.to_string(),
            at: at.to_rfc3339(),
            activity: payload.activity,
            window: payload.window,
        };
        inner.journal.push_back(event);
        while inner.journal.len() > HOOK_JOURNAL_CAPACITY {
            inner.journal.pop_front();
        }
        inner.oldest_seq = inner.journal.front().map(|e| e.seq).unwrap_or(seq);
        drop(inner);
        self.events.notify_waiters();
    }
}

#[derive(Default)]
struct EventPayload {
    activity: Option<ActivityReport>,
    window: Option<WindowEventInfo>,
}

impl Inner {
    fn buffered_for(&self, id: &str) -> u32 {
        self.journal.iter().filter(|e| e.hook_id == id).count() as u32
    }

    fn visible(&self, scopes: &ScopeSet) -> Vec<HookStatus> {
        self.hooks
            .values()
            .filter(|h| scopes.contains(h.spec.required_scope()))
            .map(|h| h.status(self.buffered_for(&h.id)))
            .collect()
    }

    /// Read the journal for one caller.
    ///
    /// The cursor is the subtle part. Sequence numbers are global across
    /// hooks, so a drain filtered to one hook walks *past* events belonging
    /// to others — and if the resulting cursor is then reused on an
    /// unfiltered poll, those events are skipped forever. That is not
    /// hypothetical: it cost an agent a window-open event, which it
    /// reasonably concluded was a broken subscription.
    ///
    /// So a filtered drain never advances past anything it did not hand
    /// over, and the reply says which filter its cursor belongs to.
    fn drain(&self, id: Option<&str>, since: u64, limit: u32, scopes: &ScopeSet) -> HookPollResult {
        let allowed: BTreeSet<&str> = self
            .hooks
            .values()
            .filter(|h| scopes.contains(h.spec.required_scope()))
            .map(|h| h.id.as_str())
            .collect();

        let mut events: Vec<HookEvent> = Vec::new();
        // Lowest sequence this caller could legitimately still want, but
        // that this drain is not handing over because of the `id` filter.
        // Scope-hidden events do not count: the token will never be shown
        // them under any filter, so stepping over those loses nothing.
        let mut first_skipped: Option<u64> = None;
        for e in self.journal.iter().filter(|e| e.seq > since) {
            if !allowed.contains(e.hook_id.as_str()) {
                continue;
            }
            if id.is_some_and(|want| e.hook_id != want) {
                first_skipped.get_or_insert(e.seq);
                continue;
            }
            if events.len() >= limit as usize {
                break;
            }
            events.push(e.clone());
        }

        let last_returned = events.last().map(|e| e.seq);
        let skipped = match (id, last_returned) {
            (Some(want), Some(last)) => self
                .journal
                .iter()
                .filter(|e| e.seq > since && e.seq < last)
                .filter(|e| e.hook_id != want)
                .filter(|e| allowed.contains(e.hook_id.as_str()))
                .count() as u32,
            _ => 0,
        };

        // One journal, one sequence, so a cursor belongs to the filter that
        // produced it — the same contract a consumer group has over a log.
        // It advances to the last event handed over, and the reply says
        // which filter it is for and how many of *other* hooks' events lie
        // inside the range, because the failure this guards against is
        // silent: reuse a filtered cursor on an unfiltered poll and those
        // events are gone.
        //
        // Refusing to advance past them instead was tried and is worse: with
        // two hooks emitting, a filtered cursor can never get past the other
        // hook's first event, so every drain replays the whole journal. A
        // warned-about gap beats a livelock.
        let next_seq = match last_returned {
            Some(last) => last,
            // Nothing handed over. A filtered drain leaves the cursor where
            // it was; an unfiltered one can jump to the end, because there
            // is provably nothing this caller can see in between.
            None => match id {
                Some(_) => since,
                None => self.next_seq,
            },
        };
        let _ = first_skipped;
        HookPollResult {
            events,
            next_seq: next_seq.max(since),
            cursor_scope: id.map(|i| i.to_string()),
            skipped_other_hooks: skipped,
            dropped: since > 0 && self.oldest_seq > since + 1,
            hooks: self.visible(scopes),
        }
    }
}

pub type SharedHooks = Arc<HookRegistry>;

// ---------------------------------------------------------------------------
// Activity watcher
// ---------------------------------------------------------------------------

/// One run of screen change that has not yet gone quiet.
struct Burst {
    started: Instant,
    started_at: DateTime<Utc>,
    last_change: Instant,
    bbox: Region,
    samples: u32,
    peak_fraction: f64,
}

impl Burst {
    fn extend(&mut self, bbox: Region, fraction: f64, now: Instant) {
        self.bbox = union(self.bbox, bbox);
        self.samples += 1;
        self.peak_fraction = self.peak_fraction.max(fraction);
        self.last_change = now;
    }
}

fn union(a: Region, b: Region) -> Region {
    if a.width == 0 || a.height == 0 {
        return b;
    }
    if b.width == 0 || b.height == 0 {
        return a;
    }
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    Region {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

/// The circle that covers a rectangle.
///
/// Radius is the half-diagonal, so the circle contains the whole changed
/// box rather than merely most of it. A circle is what the agent gets
/// because that is what the measurement supports: the grid says "something
/// around here moved", and a rectangle with exact edges would overstate it.
pub fn circle_for(bbox: Region) -> Circle {
    let w = f64::from(bbox.width);
    let h = f64::from(bbox.height);
    Circle {
        x: f64::from(bbox.x) + w / 2.0,
        y: f64::from(bbox.y) + h / 2.0,
        radius: (w * w + h * h).sqrt() / 2.0,
    }
}

/// Per-hook state the activity watcher keeps between rounds.
#[derive(Default)]
struct ActivityState {
    previous: Option<ProbeFrame>,
    burst: Option<Burst>,
    area: Option<ActivityScope>,
    /// When this hook last emitted, for `min_interval_ms`.
    last_emit: Option<Instant>,
}

/// Sample the capture stream and report circles of change.
///
/// Holds the display open while any activity hook is enabled: watching the
/// screen means streaming the screen, and gdrd's idle teardown would
/// otherwise stop the stream under a hook that is meant to be watching. That
/// is a deliberate, visible cost of enabling one — the desktop is being
/// captured continuously until the hook is toggled off.
pub async fn run_activity_watcher(
    hooks: SharedHooks,
    display: SharedDisplay,
    window_plane: Option<Arc<WindowPlane>>,
) {
    let mut state: HashMap<String, ActivityState> = HashMap::new();
    loop {
        let work = hooks.wait_for_work(HookKind::Activity).await;
        state.retain(|id, _| work.iter().any(|(w, _)| w == id));

        // One stream start for the whole round, shared by every hook.
        let stream_ready = {
            let mut guard = display.lock().await;
            match guard.warm_start().await {
                Ok(()) => None,
                Err(e) => Some(format!("{e:#}")),
            }
        };

        let mut cadence = u64::MAX;
        // Window-scoped hooks need live geometry; list once per round, and
        // only when something actually asks for it.
        let needs_windows = work.iter().any(|(_, spec)| {
            matches!(spec, HookSpec::Activity(a) if a.target.is_some())
        });
        let windows = if needs_windows {
            match &window_plane {
                Some(plane) => {
                    let connector = display.lock().await.capture_connector();
                    plane
                        .list(connector.as_deref(), capture::stream_size(), true)
                        .await
                        .ok()
                        .map(|l| l.windows)
                }
                None => None,
            }
        } else {
            None
        };

        for (id, spec) in &work {
            let HookSpec::Activity(spec) = spec else {
                continue;
            };
            cadence = cadence.min(spec.poll_ms);
            if let Some(err) = &stream_ready {
                // The display provider refusing to start is a real failure,
                // not a "not yet": it needs someone to act (unlock the
                // session, plug a monitor in). Waiting is reserved for
                // states that clear on their own.
                hooks
                    .set_state(id, HookState::Failing, Some(err.clone()))
                    .await;
                state.remove(id);
                continue;
            }
            let entry = state.entry(id.clone()).or_default();
            if let Err(e) =
                sample_activity(&hooks, id, spec, entry, windows.as_deref(), window_plane.as_deref())
                    .await
            {
                hooks
                    .set_state(id, HookState::Waiting, Some(format!("{e:#}")))
                    .await;
                entry.previous = None;
            }
        }

        let cadence = if cadence == u64::MAX { 200 } else { cadence };
        tokio::time::sleep(Duration::from_millis(cadence)).await;
    }
}

async fn sample_activity(
    hooks: &HookRegistry,
    id: &str,
    spec: &ActivitySpec,
    state: &mut ActivityState,
    windows: Option<&[WindowInfo]>,
    plane: Option<&WindowPlane>,
) -> Result<()> {
    // Resolve where to look. A window-scoped hook re-resolves every round,
    // so it follows the window rather than the patch of desktop it started on.
    let (region, area) = match &spec.target {
        Some(target) => {
            let windows = windows.ok_or_else(|| {
                anyhow!(
                    "this hook watches one window, but the window plane is not available — {}",
                    crate::windows::INSTALL_HINT
                )
            })?;
            let w = target
                .resolve(windows)
                .map_err(|e| anyhow!("{e}"))?;
            let region = w.stream_region.ok_or_else(|| {
                anyhow!(
                    "window {} is not on the monitor gdrd is streaming, so there are no \
                     pixels to watch — move it there or drop the window filter",
                    w.label()
                )
            })?;
            if w.minimized {
                return Err(anyhow!(
                    "window {} is minimized — nothing to watch until it is restored",
                    w.label()
                ));
            }
            (
                Some(region),
                ActivityScope::Window {
                    id: w.id,
                    title: w.title.clone(),
                    app_id: w.app_id.clone(),
                    region,
                },
            )
        }
        None => match spec.region {
            Some(r) => (Some(r), ActivityScope::Region { region: r }),
            None => (None, ActivityScope::Screen),
        },
    };

    let Some(frame) = capture::probe_grid(region, spec.grid)? else {
        return Err(anyhow!(
            "no capture stream yet — the hook starts reporting once the display session is up"
        ));
    };
    let now = Instant::now();
    let previous = state.previous.replace(frame.clone());
    state.area = Some(area.clone());

    let Some(previous) = previous else {
        // First sample is the baseline, not a detection.
        hooks.set_state(id, HookState::Watching, None).await;
        return Ok(());
    };
    let Some(diff) = frame.diff(&previous, spec.threshold) else {
        // The watched area moved or the stream was renegotiated; re-baseline.
        return Ok(());
    };

    let moving = diff.changed_cells >= spec.min_cells;
    if moving {
        let fraction = diff.fraction();
        match state.burst.as_mut() {
            Some(b) => b.extend(diff.bbox, fraction, now),
            None => {
                state.burst = Some(Burst {
                    started: now,
                    started_at: Utc::now(),
                    last_change: now,
                    bbox: diff.bbox,
                    samples: 1,
                    peak_fraction: fraction,
                })
            }
        }
    }

    let Some(burst) = state.burst.as_ref() else {
        hooks.set_state(id, HookState::Watching, None).await;
        return Ok(());
    };

    let quiet_for = now.duration_since(burst.last_change);
    let ran_for = now.duration_since(burst.started);
    let settled = quiet_for >= Duration::from_millis(spec.buffer_ms);
    let overran =
        spec.max_burst_ms > 0 && ran_for >= Duration::from_millis(spec.max_burst_ms);
    if !settled && !overran {
        return Ok(());
    }

    // Rate limit, if asked for. The burst is deliberately *not* taken: it
    // keeps growing while it is held back, so the event that eventually
    // comes out describes everything that happened in the meantime rather
    // than only the last twitch before the gate opened.
    if spec.min_interval_ms > 0 {
        if let Some(last) = state.last_emit {
            if now.duration_since(last) < Duration::from_millis(spec.min_interval_ms) {
                return Ok(());
            }
        }
    }

    let burst = state.burst.take().expect("checked above");
    let circle = circle_for(burst.bbox);
    if spec.max_radius > 0 && circle.radius > f64::from(spec.max_radius) {
        // Too big to be the thing anyone subscribed for — a workspace switch
        // or a video. Dropped rather than reported as "the screen changed".
        return Ok(());
    }
    let ended_at = Utc::now();
    // Asked once per event, not once per sample: the capture stream keeps
    // running across a lock, so without this an agent happily chases the
    // clock on a lock screen believing it is watching the desktop. One D-Bus
    // round trip per reported burst is cheap; per sample would not be.
    let session_locked = match plane {
        Some(p) => p.session_locked().await,
        None => None,
    };
    let report = ActivityReport {
        circle,
        bbox: burst.bbox,
        started_at: burst.started_at.to_rfc3339(),
        ended_at: ended_at.to_rfc3339(),
        duration_ms: burst
            .last_change
            .duration_since(burst.started)
            .as_millis() as u64,
        buffer_ms: spec.buffer_ms,
        samples: burst.samples,
        changed_fraction: burst.peak_fraction,
        area,
        stream_width: frame.native_width,
        stream_height: frame.native_height,
        settled,
        session_locked,
    };
    state.last_emit = Some(now);
    hooks
        .emit(
            id,
            "activity",
            ended_at,
            EventPayload {
                activity: Some(report),
                window: None,
            },
        )
        .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Window watcher
// ---------------------------------------------------------------------------

/// What the window watcher remembers about a window between rounds.
#[derive(Clone)]
struct WindowSnapshot {
    info: WindowInfo,
    /// Captured when the window is first seen, so a `closed` event can still
    /// say who owned it — by the time the window is gone, `/proc` usually is
    /// too.
    process: Option<ProcessInfo>,
}

/// A change waiting out its `buffer_ms` before being reported.
struct PendingWindow {
    kinds: BTreeSet<String>,
    before: WindowSnapshot,
    latest: WindowSnapshot,
    samples: u32,
    started: Instant,
    last_change: Instant,
}

#[derive(Default)]
struct WindowWatchState {
    pending: HashMap<u64, PendingWindow>,
}

/// Order events are emitted in when several coalesce for one window.
const KIND_ORDER: [&str; 9] = [
    "opened",
    "moved",
    "resized",
    "retitled",
    "workspace",
    "minimized",
    "unminimized",
    "focused",
    "closed",
];

pub async fn run_window_watcher(
    hooks: SharedHooks,
    plane: Arc<WindowPlane>,
    display: Option<SharedDisplay>,
) {
    let mut previous: Option<HashMap<u64, WindowSnapshot>> = None;
    let mut state: HashMap<String, WindowWatchState> = HashMap::new();

    loop {
        let work = hooks.wait_for_work(HookKind::Window).await;
        state.retain(|id, _| work.iter().any(|(w, _)| w == id));

        let connector = match &display {
            Some(d) => d.lock().await.capture_connector(),
            None => None,
        };
        let listed = plane
            .list(connector.as_deref(), capture::stream_size(), true)
            .await;

        let cadence = work
            .iter()
            .map(|(_, s)| s.poll_ms())
            .min()
            .unwrap_or(250);

        let list = match listed {
            Ok(l) => l,
            Err(e) => {
                // Enabled, and the last attempt errored — the extension is
                // gone, or the shell restarted. A caller polling `state`
                // must be able to tell this from a healthy hook with nothing
                // to report, which is what `watching` means.
                for (id, _) in &work {
                    hooks
                        .set_state(id, HookState::Failing, Some(format!("{e:#}")))
                        .await;
                }
                previous = None;
                tokio::time::sleep(Duration::from_millis(cadence.max(1000))).await;
                continue;
            }
        };

        let mut current: HashMap<u64, WindowSnapshot> = HashMap::new();
        for w in list.windows {
            let process = previous
                .as_ref()
                .and_then(|p| p.get(&w.id))
                .and_then(|s| s.process.clone())
                .or_else(|| Some(process_info(w.pid)));
            current.insert(w.id, WindowSnapshot { info: w, process });
        }

        if let Some(prev) = previous.as_ref() {
            let now = Instant::now();
            for (id, spec) in &work {
                let HookSpec::Window(spec) = spec else {
                    continue;
                };
                let entry = state.entry(id.clone()).or_default();
                diff_round(&hooks, id, spec, entry, prev, &current, now).await;
                hooks.set_state(id, HookState::Watching, None).await;
            }
        } else {
            for (id, _) in &work {
                hooks.set_state(id, HookState::Watching, None).await;
            }
        }

        previous = Some(current);
        tokio::time::sleep(Duration::from_millis(cadence)).await;
    }
}

/// Compare two window listings for one hook, buffering what it subscribes to.
async fn diff_round(
    hooks: &HookRegistry,
    id: &str,
    spec: &WindowHookSpec,
    state: &mut WindowWatchState,
    prev: &HashMap<u64, WindowSnapshot>,
    current: &HashMap<u64, WindowSnapshot>,
    now: Instant,
) {
    let matches = |s: &WindowSnapshot| -> bool {
        if !spec.include_skip_taskbar && s.info.skip_taskbar {
            return false;
        }
        match &spec.target {
            None => true,
            // `resolve` is for picking exactly one window; here we want
            // "does this one match", so a single-window slice is the honest
            // way to ask the same matcher.
            Some(t) => t.resolve(std::slice::from_ref(&s.info)).is_ok(),
        }
    };

    // Closed: emitted immediately. There is nothing left to settle, and a
    // buffered close would arrive after whatever the agent does next.
    for (wid, was) in prev {
        if current.contains_key(wid) || !matches(was) {
            continue;
        }
        // A window that closed while a change was still buffering: flush the
        // pending report first so the history reads in the order it happened.
        if let Some(p) = state.pending.remove(wid) {
            flush_pending(hooks, id, spec, p, false).await;
        }
        if spec.wants("closed") {
            let mut info = event_info(was, spec);
            info.samples = 1;
            emit_window(hooks, id, "closed", info).await;
        }
    }

    for (wid, now_snap) in current {
        if !matches(now_snap) {
            continue;
        }
        let Some(was) = prev.get(wid) else {
            if spec.wants("opened") {
                state.pending.insert(
                    *wid,
                    PendingWindow {
                        kinds: BTreeSet::from(["opened".to_string()]),
                        before: now_snap.clone(),
                        latest: now_snap.clone(),
                        samples: 1,
                        started: now,
                        last_change: now,
                    },
                );
            }
            continue;
        };

        let mut kinds: BTreeSet<String> = BTreeSet::new();
        let (a, b) = (&was.info, &now_snap.info);
        let moved = (a.frame_rect.x - b.frame_rect.x).unsigned_abs() >= spec.geometry_threshold
            || (a.frame_rect.y - b.frame_rect.y).unsigned_abs() >= spec.geometry_threshold;
        let resized = (a.frame_rect.width - b.frame_rect.width).unsigned_abs()
            >= spec.geometry_threshold
            || (a.frame_rect.height - b.frame_rect.height).unsigned_abs()
                >= spec.geometry_threshold;
        if resized && spec.wants("resized") {
            kinds.insert("resized".into());
        }
        if moved && spec.wants("moved") {
            kinds.insert("moved".into());
        }
        if a.title != b.title && spec.wants("retitled") {
            kinds.insert("retitled".into());
        }
        if a.workspace != b.workspace && spec.wants("workspace") {
            kinds.insert("workspace".into());
        }
        if !a.minimized && b.minimized && spec.wants("minimized") {
            kinds.insert("minimized".into());
        }
        if a.minimized && !b.minimized && spec.wants("unminimized") {
            kinds.insert("unminimized".into());
        }
        if !a.focus && b.focus && spec.wants("focused") {
            kinds.insert("focused".into());
        }

        match state.pending.get_mut(wid) {
            Some(p) => {
                p.latest = now_snap.clone();
                p.samples += 1;
                if !kinds.is_empty() {
                    // An `opened` that is still settling absorbs the window's
                    // own resizing: a window that appears and then finds its
                    // size did not "resize", it arrived.
                    if !p.kinds.contains("opened") {
                        p.kinds.extend(kinds);
                    }
                    p.last_change = now;
                }
            }
            None => {
                if !kinds.is_empty() {
                    state.pending.insert(
                        *wid,
                        PendingWindow {
                            kinds,
                            before: was.clone(),
                            latest: now_snap.clone(),
                            samples: 1,
                            started: now,
                            last_change: now,
                        },
                    );
                }
            }
        }
    }

    // Flush whatever has held still long enough.
    let buffer = Duration::from_millis(spec.buffer_ms);
    let max_burst = Duration::from_millis(spec.max_burst_ms);
    let ready: Vec<u64> = state
        .pending
        .iter()
        .filter(|(_, p)| {
            if spec.max_burst_ms > 0 && now.duration_since(p.started) >= max_burst {
                return true;
            }
            if !geometry_ready(&p.latest.info) {
                return false;
            }
            now.duration_since(p.last_change) >= buffer
        })
        .map(|(k, _)| *k)
        .collect();
    for wid in ready {
        let Some(p) = state.pending.remove(&wid) else {
            continue;
        };
        let settled =
            now.duration_since(p.last_change) >= buffer && geometry_ready(&p.latest.info);
        flush_pending(hooks, id, spec, p, settled).await;
    }
}

/// Whether a window has geometry worth reporting yet.
///
/// A window that the compositor is managing but has not placed yet has a
/// 0x0 frame — chromium sits like that for most of a second while it starts.
/// Reporting "chromium opened, 0x0" the instant the window appears is
/// technically true and useless; it is exactly what the buffer exists to
/// avoid, so an unplaced window is treated as still moving rather than as a
/// quiet one. `max_burst_ms` still bounds the wait, and what comes out then
/// is flagged `settled: false`.
fn geometry_ready(info: &WindowInfo) -> bool {
    info.frame_rect.width > 0 && info.frame_rect.height > 0
}

async fn flush_pending(
    hooks: &HookRegistry,
    id: &str,
    spec: &WindowHookSpec,
    pending: PendingWindow,
    settled: bool,
) {
    for kind in KIND_ORDER {
        if !pending.kinds.contains(kind) {
            continue;
        }
        let mut info = event_info(&pending.latest, spec);
        info.samples = pending.samples;
        info.settled = settled;
        info.previous = Some(change_from(&pending.before.info, &pending.latest.info, kind));
        emit_window(hooks, id, kind, info).await;
    }
}

fn change_from(before: &WindowInfo, after: &WindowInfo, kind: &str) -> WindowChange {
    let mut change = WindowChange {
        frame_rect: Some(before.frame_rect),
        title: before.title.clone(),
        minimized: Some(before.minimized),
        workspace: before.workspace,
        dw: None,
        dh: None,
        dx: None,
        dy: None,
    };
    match kind {
        "resized" => {
            change.dw = Some(after.frame_rect.width - before.frame_rect.width);
            change.dh = Some(after.frame_rect.height - before.frame_rect.height);
        }
        "moved" => {
            change.dx = Some(after.frame_rect.x - before.frame_rect.x);
            change.dy = Some(after.frame_rect.y - before.frame_rect.y);
        }
        _ => {}
    }
    change
}

fn event_info(snap: &WindowSnapshot, spec: &WindowHookSpec) -> WindowEventInfo {
    let w = &snap.info;
    WindowEventInfo {
        id: w.id,
        title: w.title.clone(),
        app_id: w.app_id.clone(),
        wm_class: w.wm_class.clone(),
        pid: w.pid,
        process: if spec.include_process {
            snap.process.clone()
        } else {
            None
        },
        window_type: w.window_type.clone(),
        frame_rect: w.frame_rect,
        stream_region: w.stream_region,
        monitor: w.monitor,
        workspace: w.workspace,
        minimized: w.minimized,
        focus: w.focus,
        maximized: w.maximized.clone(),
        fullscreen: w.fullscreen,
        previous: None,
        samples: 1,
        settled: true,
    }
}

async fn emit_window(hooks: &HookRegistry, id: &str, kind: &str, info: WindowEventInfo) {
    hooks
        .emit(
            id,
            kind,
            Utc::now(),
            EventPayload {
                activity: None,
                window: Some(info),
            },
        )
        .await;
}

// ---------------------------------------------------------------------------
// Process lookup
// ---------------------------------------------------------------------------

/// Who owns a window, read from `/proc`.
///
/// Best-effort by construction: the pid comes from the compositor, which
/// does not have one for every window (XWayland proxies, some portals), and
/// the process can exit between the listing and the read. Every failure
/// becomes `error` on the record rather than an absent record, so "we could
/// not tell" is distinguishable from "nobody asked".
pub fn process_info(pid: i32) -> ProcessInfo {
    let mut info = ProcessInfo {
        pid,
        ..Default::default()
    };
    if pid <= 0 {
        info.error = Some("the compositor did not report a pid for this window".into());
        return info;
    }
    let base = format!("/proc/{pid}");
    if !std::path::Path::new(&base).exists() {
        info.error = Some(format!("process {pid} is gone"));
        return info;
    }
    info.comm = std::fs::read_to_string(format!("{base}/comm"))
        .ok()
        .map(|s| s.trim().to_string());
    info.exe = std::fs::read_link(format!("{base}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    info.cmdline = std::fs::read(format!("{base}/cmdline")).ok().map(|raw| {
        let mut s = raw
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join(" ");
        // Chromium and friends carry a kilobyte of switches; the first
        // couple of hundred characters are the part that identifies it.
        if s.len() > 512 {
            s.truncate(509);
            s.push_str("...");
        }
        s
    });
    if let Ok(status) = std::fs::read_to_string(format!("{base}/status")) {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                info.uid = rest.split_whitespace().next().and_then(|v| v.parse().ok());
            } else if let Some(rest) = line.strip_prefix("PPid:") {
                info.ppid = rest.trim().parse().ok();
            }
        }
    }
    info.user = info.uid.and_then(user_for_uid);
    info
}

/// Resolve a uid to a login name without pulling in libc bindings.
fn user_for_uid(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in passwd.lines() {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _passwd = fields.next()?;
        let id: u32 = fields.next()?.parse().ok()?;
        if id == uid {
            return Some(name.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::hooks::WindowHookSpec;

    fn scopes(list: &str) -> ScopeSet {
        ScopeSet::parse_list(list).unwrap()
    }

    fn window_spec() -> HookSpec {
        HookSpec::Window(WindowHookSpec::default())
    }

    fn activity_spec() -> HookSpec {
        HookSpec::Activity(ActivitySpec::default())
    }

    #[test]
    fn circle_covers_the_whole_changed_box() {
        let c = circle_for(Region {
            x: 100,
            y: 100,
            width: 200,
            height: 200,
        });
        assert_eq!(c.x, 200.0);
        assert_eq!(c.y, 200.0);
        // Half-diagonal: every corner is inside.
        let dx = 300.0 - c.x;
        let dy = 300.0 - c.y;
        assert!(c.radius >= (dx * dx + dy * dy).sqrt() - 0.001);
    }

    #[test]
    fn union_of_bursts_grows_to_cover_both() {
        let a = Region { x: 0, y: 0, width: 10, height: 10 };
        let b = Region { x: 90, y: 90, width: 10, height: 10 };
        let u = union(a, b);
        assert_eq!((u.x, u.y, u.width, u.height), (0, 0, 100, 100));
        // An empty box is a non-event, not an origin-anchored one.
        let empty = Region { x: 50, y: 50, width: 0, height: 0 };
        assert_eq!(union(a, empty), a);
        assert_eq!(union(empty, b), b);
    }

    #[tokio::test]
    async fn a_window_hook_is_invisible_to_a_screenshot_only_token() {
        let reg = HookRegistry::new();
        reg.create(window_spec(), None, true, &scopes("window"), None)
            .await
            .unwrap();
        assert_eq!(reg.list(&scopes("window")).await.len(), 1);
        assert!(reg.list(&scopes("screenshot")).await.is_empty());
    }

    #[tokio::test]
    async fn creating_a_hook_requires_the_scope_it_watches_with() {
        let reg = HookRegistry::new();
        let err = reg
            .create(activity_spec(), None, true, &scopes("window"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("screenshot"), "{err}");
        // And the same token may create the window hook it does hold.
        assert!(reg
            .create(window_spec(), None, true, &scopes("window"), None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn toggling_off_keeps_the_hook_and_its_config() {
        let reg = HookRegistry::new();
        let created = reg
            .create(window_spec(), Some("watch".into()), true, &scopes("all"), None)
            .await
            .unwrap();
        let off = reg
            .update(&created.id, Some(false), None, None, &scopes("all"))
            .await
            .unwrap();
        assert!(!off.enabled);
        assert_eq!(off.state, HookState::Paused);
        assert_eq!(off.spec, created.spec);
        assert_eq!(off.label.as_deref(), Some("watch"));
        let on = reg
            .update(&created.id, Some(true), None, None, &scopes("all"))
            .await
            .unwrap();
        assert!(on.enabled);
        assert_eq!(on.id, created.id);
    }

    #[tokio::test]
    async fn a_hook_cannot_change_kind_under_its_own_id() {
        let reg = HookRegistry::new();
        let created = reg
            .create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        let err = reg
            .update(&created.id, None, None, Some(activity_spec()), &scopes("all"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("remove it"), "{err}");
    }

    #[tokio::test]
    async fn polling_returns_events_in_order_and_advances_the_cursor() {
        let reg = HookRegistry::new();
        let hook = reg
            .create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        for _ in 0..3 {
            reg.emit(
                &hook.id,
                "opened",
                Utc::now(),
                EventPayload::default(),
            )
            .await;
        }
        let first = reg.poll(None, 0, 2, 0, &scopes("all")).await.unwrap();
        assert_eq!(first.events.len(), 2);
        assert_eq!(first.next_seq, 2);
        let second = reg
            .poll(None, first.next_seq, 10, 0, &scopes("all"))
            .await
            .unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].seq, 3);
        // Draining twice does not replay.
        let third = reg
            .poll(None, second.next_seq, 10, 0, &scopes("all"))
            .await
            .unwrap();
        assert!(third.events.is_empty());
    }

    #[tokio::test]
    async fn a_quiet_unfiltered_poll_moves_the_cursor_to_the_end() {
        // Only when unfiltered. This test used to assert the same of a
        // *filtered* drain, which is precisely the bug below: a cursor that
        // jumps to the global end has silently consumed every other hook's
        // events.
        let reg = HookRegistry::new();
        let window = reg
            .create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        for _ in 0..3 {
            reg.emit(&window.id, "opened", Utc::now(), EventPayload::default())
                .await;
        }
        // A screenshot-only token cannot see the window hook at all, so
        // there is provably nothing left for it in that range.
        let quiet = reg.poll(None, 0, 10, 0, &scopes("screenshot")).await.unwrap();
        assert!(quiet.events.is_empty());
        assert_eq!(quiet.next_seq, 3);
        assert_eq!(quiet.cursor_scope, None);
    }

    #[tokio::test]
    async fn a_filtered_drain_says_its_cursor_is_filter_scoped() {
        // The failure this guards: drain one hook, reuse its cursor on an
        // unfiltered poll, and the other hook's events are gone for good. An
        // agent hit exactly that and concluded its window subscription was
        // broken. The cursor still has to advance, so the defence is that
        // the reply says so out loud.
        let reg = HookRegistry::new();
        let a = reg
            .create(activity_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        let b = reg
            .create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        reg.emit(&a.id, "activity", Utc::now(), EventPayload::default())
            .await; // seq 1
        reg.emit(&b.id, "opened", Utc::now(), EventPayload::default())
            .await; // seq 2
        reg.emit(&a.id, "activity", Utc::now(), EventPayload::default())
            .await; // seq 3

        let filtered = reg.poll(Some(&a.id), 0, 10, 0, &scopes("all")).await.unwrap();
        assert_eq!(filtered.events.len(), 2);
        assert_eq!(filtered.cursor_scope.as_deref(), Some(a.id.as_str()));
        assert_eq!(
            filtered.skipped_other_hooks, 1,
            "the caller is told an event of another hook sits inside this range"
        );

        // The cursor advances — anything else livelocks a filtered drain —
        // so what protects the caller is being told, in the same reply,
        // that this cursor is not the one to use for everything.
        assert_eq!(filtered.next_seq, 3);
        let unfiltered = reg.poll(None, 0, 10, 0, &scopes("all")).await.unwrap();
        assert_eq!(unfiltered.cursor_scope, None);
        assert_eq!(unfiltered.skipped_other_hooks, 0);
        assert_eq!(unfiltered.events.len(), 3, "the simple loop sees everything");
    }

    #[tokio::test]
    async fn an_empty_filtered_drain_leaves_the_cursor_alone() {
        let reg = HookRegistry::new();
        let a = reg
            .create(activity_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        let b = reg
            .create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        for _ in 0..3 {
            reg.emit(&b.id, "opened", Utc::now(), EventPayload::default())
                .await;
        }
        // Nothing for A. Advancing to the global end here would eat all of
        // B's events on the caller's next unfiltered poll.
        let quiet = reg.poll(Some(&a.id), 0, 10, 0, &scopes("all")).await.unwrap();
        assert!(quiet.events.is_empty());
        assert_eq!(quiet.next_seq, 0);
        let all = reg.poll(None, quiet.next_seq, 10, 0, &scopes("all")).await.unwrap();
        assert_eq!(all.events.len(), 3);
    }

    #[tokio::test]
    async fn a_poll_cannot_drain_a_hook_the_token_may_not_see() {
        let reg = HookRegistry::new();
        let hook = reg
            .create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        reg.emit(&hook.id, "opened", Utc::now(), EventPayload::default())
            .await;
        let visible = reg.poll(None, 0, 10, 0, &scopes("window")).await.unwrap();
        assert_eq!(visible.events.len(), 1);
        let denied = reg.poll(None, 0, 10, 0, &scopes("screenshot")).await.unwrap();
        assert!(denied.events.is_empty());
        let named = reg
            .poll(Some(&hook.id), 0, 10, 0, &scopes("screenshot"))
            .await
            .unwrap_err()
            .to_string();
        assert!(named.contains("permission denied"), "{named}");
    }

    #[tokio::test]
    async fn removing_a_hook_takes_its_buffered_events_with_it() {
        let reg = HookRegistry::new();
        let hook = reg
            .create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        reg.emit(&hook.id, "opened", Utc::now(), EventPayload::default())
            .await;
        let removed = reg.remove(&hook.id, &scopes("all")).await.unwrap();
        assert_eq!(removed.id, hook.id);
        let after = reg.poll(None, 0, 10, 0, &scopes("all")).await.unwrap();
        assert!(after.events.is_empty());
        assert!(after.hooks.is_empty());
    }

    #[tokio::test]
    async fn a_blocked_poll_wakes_when_an_event_lands() {
        let reg = Arc::new(HookRegistry::new());
        let hook = reg
            .create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        let writer = reg.clone();
        let id = hook.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            writer
                .emit(&id, "opened", Utc::now(), EventPayload::default())
                .await;
        });
        let started = Instant::now();
        let result = reg.poll(None, 0, 10, 5_000, &scopes("all")).await.unwrap();
        assert_eq!(result.events.len(), 1);
        assert!(
            started.elapsed() < Duration::from_millis(4_000),
            "poll should return on the event, not on the timeout"
        );
    }

    #[tokio::test]
    async fn a_blocked_poll_gives_up_at_the_deadline() {
        let reg = HookRegistry::new();
        reg.create(window_spec(), None, true, &scopes("all"), None)
            .await
            .unwrap();
        let started = Instant::now();
        let result = reg.poll(None, 0, 10, 120, &scopes("all")).await.unwrap();
        assert!(result.events.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn process_info_says_why_it_could_not_answer() {
        let none = process_info(0);
        assert!(none.error.as_deref().unwrap().contains("did not report"));
        let gone = process_info(999_999_21);
        assert!(gone.error.is_some());
        // Our own pid is always answerable, and is the happy path.
        let me = process_info(std::process::id() as i32);
        assert!(me.error.is_none(), "{me:?}");
        assert!(me.comm.is_some());
        assert!(me.uid.is_some());
    }
}
