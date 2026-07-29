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
        let rd_session_path = rd.create_session().await?;
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
        rd_session
            .start()
            .await
            .context("RemoteDesktop.Session.Start")?;

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
