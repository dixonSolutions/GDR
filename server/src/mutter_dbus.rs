//! Proxies for Mutter's *private*, unstable D-Bus APIs. Field/method names
//! here were taken directly from Mutter's own interface XML
//! (org.gnome.Mutter.RemoteDesktop.xml / org.gnome.Mutter.ScreenCast.xml).
//!
//! IMPORTANT: this API is explicitly documented upstream as
//! "private and not intended to be used outside of the integrated system
//! that uses libmutter. No compatibility between versions are promised."
//! Before deploying against a specific GNOME/Mutter version, sanity check
//! with:
//!   busctl --user introspect org.gnome.Mutter.RemoteDesktop /org/gnome/Mutter/RemoteDesktop
//!   busctl --user introspect org.gnome.Mutter.ScreenCast /org/gnome/Mutter/ScreenCast
//! and adjust signatures below if a newer Mutter changed something.

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use std::time::Duration;
use zbus::{proxy, zvariant::OwnedObjectPath, Connection};

const RD_DEST: &str = "org.gnome.Mutter.RemoteDesktop";
const SC_DEST: &str = "org.gnome.Mutter.ScreenCast";

/// GNOME's screensaver. `GetActive` is true while the greeter/lock shield is
/// up, which is exactly when Mutter refuses to hand a session to us.
#[proxy(
    interface = "org.gnome.ScreenSaver",
    default_service = "org.gnome.ScreenSaver",
    default_path = "/org/gnome/ScreenSaver"
)]
pub trait ScreenSaver {
    fn get_active(&self) -> zbus::Result<bool>;
}

/// logind's view of our own session (`.../session/auto`). Used as the
/// fallback lock probe and to name the session in the remedy we print.
#[proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
pub trait LogindSession {
    #[zbus(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}

#[proxy(
    interface = "org.gnome.Mutter.RemoteDesktop",
    default_service = "org.gnome.Mutter.RemoteDesktop",
    default_path = "/org/gnome/Mutter/RemoteDesktop"
)]
pub trait RemoteDesktop {
    fn create_session(&self) -> zbus::Result<OwnedObjectPath>;

    #[zbus(property)]
    fn supported_device_types(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<i32>;
}

#[proxy(interface = "org.gnome.Mutter.RemoteDesktop.Session")]
pub trait RemoteDesktopSession {
    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;

    fn notify_keyboard_keycode(&self, keycode: u32, state: bool) -> zbus::Result<()>;
    fn notify_keyboard_keysym(&self, keysym: u32, state: bool) -> zbus::Result<()>;

    fn notify_pointer_button(&self, button: i32, state: bool) -> zbus::Result<()>;
    fn notify_pointer_axis(&self, dx: f64, dy: f64, flags: u32) -> zbus::Result<()>;
    fn notify_pointer_axis_discrete(&self, axis: u32, steps: i32) -> zbus::Result<()>;
    fn notify_pointer_motion_relative(&self, dx: f64, dy: f64) -> zbus::Result<()>;
    // Note: `stream` here is a stream identifier string, per Mutter's XML
    // (not an object path type) -- matches org.gnome.Mutter.RemoteDesktop.xml.
    fn notify_pointer_motion_absolute(&self, stream: &str, x: f64, y: f64) -> zbus::Result<()>;

    #[zbus(property)]
    fn session_id(&self) -> zbus::Result<String>;

    #[zbus(signal)]
    fn closed(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.gnome.Mutter.ScreenCast",
    default_service = "org.gnome.Mutter.ScreenCast",
    default_path = "/org/gnome/Mutter/ScreenCast"
)]
pub trait ScreenCast {
    fn create_session(
        &self,
        properties: std::collections::HashMap<String, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(interface = "org.gnome.Mutter.ScreenCast.Session")]
pub trait ScreenCastSession {
    fn record_monitor(
        &self,
        connector: &str,
        properties: std::collections::HashMap<String, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn record_virtual(
        &self,
        properties: std::collections::HashMap<String, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn start(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;
}

#[proxy(interface = "org.gnome.Mutter.ScreenCast.Stream")]
pub trait ScreenCastStream {
    fn start(&self) -> zbus::Result<()>;

    #[zbus(signal, name = "PipeWireStreamAdded")]
    fn pipewire_stream_added(&self, node_id: u32) -> zbus::Result<()>;

    #[zbus(property)]
    fn parameters(
        &self,
    ) -> zbus::Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>;
}

/// Bundles the connections/proxies needed to open one remote-desktop +
/// screencast session pair, mirroring what gnome-remote-desktop itself does.
pub struct MutterSession {
    pub conn: Connection,
    pub rd_session: RemoteDesktopSessionProxy<'static>,
    pub sc_session: ScreenCastSessionProxy<'static>,
    pub sc_stream: ScreenCastStreamProxy<'static>,
    /// Object path of the screencast stream, as a string - this is what
    /// gets passed as the `stream` argument to NotifyPointerMotionAbsolute
    /// so Mutter knows which monitor's coordinate space the x/y are in.
    pub stream_id: String,
    /// Cached PipeWire node id once we've seen PipeWireStreamAdded.
    pipewire_node: std::sync::Mutex<Option<u32>>,
}

impl MutterSession {
    /// Opens a RemoteDesktop session (for input injection) plus a linked
    /// ScreenCast session. Prefer [`Self::open_with_info`] when the caller
    /// needs to know whether a platform virtual monitor was used.
    pub async fn open(connector: Option<&str>) -> Result<Self> {
        Ok(Self::open_with_info(connector).await?.0)
    }

    /// Like [`Self::open`], also returning whether we fell back to
    /// `RecordVirtual { is-platform }` (headless / no physical monitor).
    pub async fn open_with_info(connector: Option<&str>) -> Result<(Self, bool)> {
        let conn = Connection::session().await?;

        let rd = RemoteDesktopProxy::new(&conn).await?;
        // The one failure that is not a bug in this code: Mutter refuses
        // every unprivileged session while the screen is locked. Translate
        // it here rather than letting "Session creation inhibited" reach a
        // caller who will go looking for a permissions problem.
        let rd_session_path = match rd.create_session().await {
            Ok(path) => path,
            Err(e) => {
                return Err(session_failure(&conn, "RemoteDesktop.CreateSession", e).await)
            }
        };
        // zbus ProxyBuilder requires an explicit destination when the
        // object path is not the trait's default_path — otherwise you get
        // MissingParameter("destination").
        let rd_session = RemoteDesktopSessionProxy::builder(&conn)
            .destination(RD_DEST)?
            .path(rd_session_path.clone())?
            .build()
            .await?;
        let session_id = rd_session.session_id().await?;

        let sc = ScreenCastProxy::new(&conn).await?;
        let mut sc_props = std::collections::HashMap::new();
        sc_props.insert(
            "remote-desktop-session-id".to_string(),
            zbus::zvariant::Value::from(session_id),
        );
        let sc_session_path = sc.create_session(sc_props).await?;
        let sc_session = ScreenCastSessionProxy::builder(&conn)
            .destination(SC_DEST)?
            .path(sc_session_path)?
            .build()
            .await?;

        let (stream_path, used_platform_virtual) =
            pick_stream(&sc_session, connector).await?;
        let stream_id = stream_path.to_string();
        let sc_stream = ScreenCastStreamProxy::builder(&conn)
            .destination(SC_DEST)?
            .path(stream_path)?
            .build()
            .await?;

        // Subscribe BEFORE Start so we cannot miss PipeWireStreamAdded.
        let mut signals = sc_stream.receive_pipewire_stream_added().await?;

        // When the ScreenCast session is linked to a RemoteDesktop session,
        // Mutter requires RemoteDesktop.Session.Start first; the stream is
        // often auto-started. Calling ScreenCast.Session.Start fails with
        // "Must be started from remote desktop session".
        // Same translation for Start: the screen can lock between
        // CreateSession and here.
        if let Err(e) = rd_session.start().await {
            return Err(session_failure(&conn, "RemoteDesktop.Session.Start", e).await);
        }

        match sc_stream.start().await {
            Ok(()) => {}
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("already started") && !msg.contains("Already started") {
                    tracing::warn!("ScreenCast.Stream.Start after RD.Start: {msg}");
                }
            }
        }

        let node = tokio::time::timeout(Duration::from_secs(5), signals.next())
            .await
            .ok()
            .flatten();
        let pipewire_node: Option<u32> = if let Some(signal) = node {
            let args = signal.args()?;
            Some(*args.node_id())
        } else {
            read_node_from_parameters(&sc_stream).await
        };

        if pipewire_node.is_none() {
            tracing::warn!("no PipeWire node id yet; will retry on first screenshot");
        } else {
            tracing::info!(
                "PipeWire node id={:?} virtual={}",
                pipewire_node,
                used_platform_virtual
            );
        }

        Ok((
            Self {
                conn,
                rd_session,
                sc_session,
                sc_stream,
                stream_id,
                pipewire_node: std::sync::Mutex::new(pipewire_node),
            },
            used_platform_virtual,
        ))
    }

    /// Stop RemoteDesktop + ScreenCast sessions (ends PipeWire export).
    pub async fn shutdown(self) {
        if let Err(e) = self.rd_session.stop().await {
            tracing::debug!("RemoteDesktop.Session.Stop: {e}");
        }
        if let Err(e) = self.sc_session.stop().await {
            tracing::debug!("ScreenCast.Session.Stop: {e}");
        }
    }

    /// Returns the PipeWire node id for this stream, waiting if needed.
    pub async fn wait_for_pipewire_node(&self) -> Result<u32> {
        if let Some(id) = *self.pipewire_node.lock().unwrap() {
            return Ok(id);
        }
        if let Some(id) = read_node_from_parameters(&self.sc_stream).await {
            *self.pipewire_node.lock().unwrap() = Some(id);
            return Ok(id);
        }
        let mut signals = self.sc_stream.receive_pipewire_stream_added().await?;
        let signal = tokio::time::timeout(Duration::from_secs(5), signals.next())
            .await
            .map_err(|_| anyhow!("timed out waiting for PipeWireStreamAdded"))?
            .ok_or_else(|| anyhow!("stream closed before PipeWireStreamAdded"))?;
        let id = *signal.args()?.node_id();
        *self.pipewire_node.lock().unwrap() = Some(id);
        Ok(id)
    }
}

async fn pick_stream(
    sc_session: &ScreenCastSessionProxy<'_>,
    connector: Option<&str>,
) -> Result<(OwnedObjectPath, bool)> {
    if let Some(c) = connector {
        let path = sc_session
            .record_monitor(c, stream_props())
            .await
            .with_context(|| format!("record_monitor({c})"))?;
        return Ok((path, false));
    }

    let connectors = discover_connectors().await.unwrap_or_else(|e| {
        tracing::warn!("discover_connectors failed: {e:#}");
        Vec::new()
    });
    // Prefer real DRM outputs; Meta-/Virtual- are our own headless fallbacks.
    let mut ordered = connectors.clone();
    ordered.sort_by_key(|c| is_virtual_connector(c) as u8);
    for c in &ordered {
        if is_virtual_connector(c) {
            continue;
        }
        match sc_session.record_monitor(c, stream_props()).await {
            Ok(path) => {
                tracing::info!("recording monitor connector={c}");
                return Ok((path, false));
            }
            Err(e) => tracing::warn!("record_monitor({c}): {e}"),
        }
    }

    // No physical monitors (common on headless / undocked machines).
    // `is-platform` asks Mutter to create a virtual remote monitor
    // (shows up as Meta-N in DisplayConfig) — same approach as
    // gnome-remote-desktop's headless path.
    tracing::warn!(
        "no usable physical monitor connectors ({:?}); using RecordVirtual is-platform 1920x1080",
        connectors
    );
    let mut props = stream_props();
    props.insert(
        "is-platform".into(),
        zbus::zvariant::Value::from(true),
    );
    props.insert("width".into(), zbus::zvariant::Value::from(1920i32));
    props.insert("height".into(), zbus::zvariant::Value::from(1080i32));
    let path = sc_session
        .record_virtual(props)
        .await
        .context("record_virtual(is-platform)")?;
    Ok((path, true))
}

fn stream_props() -> std::collections::HashMap<String, zbus::zvariant::Value<'static>> {
    let mut props = std::collections::HashMap::new();
    // 1 = metadata cursor (drawn by client); 2 = embedded in video.
    props.insert(
        "cursor-mode".into(),
        zbus::zvariant::Value::from(1u32),
    );
    props
}

async fn read_node_from_parameters(stream: &ScreenCastStreamProxy<'_>) -> Option<u32> {
    let params = stream.parameters().await.ok()?;
    for key in ["pipewire-node-id", "node-id", "PipeWireNodeId"] {
        if let Some(val) = params.get(key) {
            if let Ok(id) = u32::try_from(val) {
                return Some(id);
            }
            if let Ok(s) = <&str>::try_from(val) {
                if let Ok(id) = s.parse() {
                    return Some(id);
                }
            }
        }
    }
    None
}

async fn discover_connectors() -> Result<Vec<String>> {
    let conn = Connection::session().await?;
    let reply = conn
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .await
        .context("GetCurrentState")?;

    // Mutter GetCurrentState:
    //   (u, a((ssss)a(siiddada{sv})a{sv}), a(iiduba(ssss)a{sv}), a{sv})
    type Mode = (
        String,
        i32,
        i32,
        f64,
        f64,
        Vec<f64>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type Spec = (String, String, String, String);
    type Monitor = (
        Spec,
        Vec<Mode>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );
    type Logical = (
        i32,
        i32,
        f64,
        u32,
        bool,
        Vec<Spec>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    );

    let (_serial, monitors, _logical, _props): (
        u32,
        Vec<Monitor>,
        Vec<Logical>,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) = reply
        .body()
        .deserialize()
        .context("deserialize GetCurrentState")?;

    let out: Vec<String> = monitors.into_iter().map(|m| m.0 .0).collect();
    if out.is_empty() {
        Err(anyhow!("zero connectors from DisplayConfig"))
    } else {
        Ok(out)
    }
}

fn is_virtual_connector(name: &str) -> bool {
    name.starts_with("Meta-") || name.starts_with("Virtual-")
}

/// What we could learn about the session's lock state on the error path.
/// `locked` is `None` when neither probe answered — we then say nothing
/// about the lock rather than guessing.
#[derive(Debug, Default, PartialEq)]
pub struct LockState {
    pub locked: Option<bool>,
    pub session_id: Option<String>,
}

/// Probe whether the GNOME session is locked.
///
/// Two independent sources, because either can be missing: GNOME's
/// `org.gnome.ScreenSaver.GetActive` on the session bus (absent under a
/// non-GNOME shell), and logind's `LockedHint` on the system bus (absent in
/// containers without a seat). logind also gives us the session id, which is
/// what `loginctl unlock-session` wants.
pub async fn probe_lock_state(session_conn: &Connection) -> LockState {
    let mut state = LockState::default();

    if let Ok(ss) = ScreenSaverProxy::new(session_conn).await {
        if let Ok(active) = ss.get_active().await {
            state.locked = Some(active);
        }
    }

    if let Ok(system) = Connection::system().await {
        if let Ok(sess) = LogindSessionProxy::new(&system).await {
            if let Ok(id) = sess.id().await {
                state.session_id = Some(id);
            }
            if state.locked.is_none() {
                if let Ok(hint) = sess.locked_hint().await {
                    state.locked = Some(hint);
                }
            }
        }
    }

    state
}

/// Mutter's refusal while the lock shield is up. It is deliberately generic
/// upstream — `meta-dbus-session-manager` rejects every unprivileged
/// CreateSession with this one string, and never mentions the screen lock.
fn is_session_inhibited(dbus_msg: &str) -> bool {
    dbus_msg.contains("Session creation inhibited")
}

/// Turn a Mutter session-creation failure into something a human (or an
/// agent) can act on. Nothing here can un-inhibit Mutter; the whole point is
/// that the caller stops hunting for a permission bug that does not exist.
pub(crate) fn explain_session_failure(op: &str, dbus_msg: &str, lock: &LockState) -> String {
    if !is_session_inhibited(dbus_msg) {
        return format!("{op}: {dbus_msg}");
    }

    let unlock = match lock.session_id.as_deref() {
        Some(id) => format!("loginctl unlock-session {id}"),
        None => "loginctl unlock-session <id>  (loginctl list-sessions)".to_string(),
    };

    match lock.locked {
        Some(true) => format!(
            "{op}: the GNOME session is locked. Mutter refuses ScreenCast and \
             RemoteDesktop to unprivileged clients while the lock shield is up, \
             and no permission or portal change reaches it. Unlock the session \
             (`{unlock}`) and retry — gdrd opens the display lazily, so the next \
             screenshot or input request succeeds with no restart. \
             (Mutter said: {dbus_msg})"
        ),
        Some(false) => format!(
            "{op}: Mutter inhibited session creation, but the session does not \
             report as locked. Another compositor-level inhibitor is active — \
             check that gdrd runs inside the graphical session \
             (`systemctl --user status gdr`) and that a shell is up. \
             (Mutter said: {dbus_msg})"
        ),
        None => format!(
            "{op}: Mutter inhibited session creation. This is almost always a \
             locked screen — neither org.gnome.ScreenSaver nor logind answered, \
             so confirm with `loginctl list-sessions` and unlock \
             (`{unlock}`), then retry. (Mutter said: {dbus_msg})"
        ),
    }
}

/// Wrap a Mutter D-Bus failure with lock-state context. Only called on the
/// error path, so the extra round trips cost nothing in the happy case.
async fn session_failure(conn: &Connection, op: &str, err: zbus::Error) -> anyhow::Error {
    let msg = err.to_string();
    let lock = probe_lock_state(conn).await;
    anyhow!(explain_session_failure(op, &msg, &lock))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locked(id: Option<&str>) -> LockState {
        LockState {
            locked: Some(true),
            session_id: id.map(str::to_string),
        }
    }

    const INHIBITED: &str = "org.freedesktop.DBus.Error.Failed: Session creation inhibited";

    #[test]
    fn unrelated_errors_pass_through_untouched() {
        let msg = explain_session_failure(
            "RemoteDesktop.CreateSession",
            "org.freedesktop.DBus.Error.ServiceUnknown: no such name",
            &LockState::default(),
        );
        assert_eq!(
            msg,
            "RemoteDesktop.CreateSession: org.freedesktop.DBus.Error.ServiceUnknown: no such name"
        );
    }

    #[test]
    fn locked_session_names_the_unlock_command() {
        let msg = explain_session_failure("RemoteDesktop.CreateSession", INHIBITED, &locked(Some("3")));
        assert!(msg.contains("the GNOME session is locked"), "{msg}");
        assert!(msg.contains("loginctl unlock-session 3"), "{msg}");
        // The raw D-Bus text stays in the message so logs remain greppable.
        assert!(msg.contains("Session creation inhibited"), "{msg}");
    }

    #[test]
    fn locked_without_a_session_id_still_explains_how_to_find_it() {
        let msg = explain_session_failure("RemoteDesktop.CreateSession", INHIBITED, &locked(None));
        assert!(msg.contains("loginctl list-sessions"), "{msg}");
    }

    #[test]
    fn inhibited_but_unlocked_does_not_claim_a_lock() {
        let state = LockState {
            locked: Some(false),
            session_id: Some("3".into()),
        };
        let msg = explain_session_failure("RemoteDesktop.CreateSession", INHIBITED, &state);
        assert!(msg.contains("does not report as locked"), "{msg}");
        assert!(!msg.contains("the GNOME session is locked"), "{msg}");
    }

    #[test]
    fn unknown_lock_state_hedges_instead_of_asserting() {
        let msg = explain_session_failure("RemoteDesktop.CreateSession", INHIBITED, &LockState::default());
        assert!(msg.contains("almost always a locked screen"), "{msg}");
        assert!(msg.contains("loginctl list-sessions"), "{msg}");
    }
}
