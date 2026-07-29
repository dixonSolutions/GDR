//! Long-lived "we are the display" provider for headless / no-monitor hosts.
//!
//! Starts a Mutter RemoteDesktop + ScreenCast session with
//! `RecordVirtual { is-platform }` and holds a PipeWire consumer that
//! negotiates a real resolution. That makes Mutter create a `Meta-*`
//! virtual monitor the GNOME session actually uses — same idea as
//! `gnome-remote-desktop --headless`.

use crate::capture::{self, CaptureSize, KeepaliveConsumer};
use crate::mutter_dbus::MutterSession;
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct DisplayProvider {
    session: MutterSession,
    size: CaptureSize,
    /// PipeWire node id for the (virtual or physical) stream.
    node_id: u32,
    is_virtual: bool,
}

impl DisplayProvider {
    /// Open the Mutter session and, for virtual monitors, start keepalive
    /// negotiation so Meta-* comes up at `size`.
    pub async fn start(connector: Option<&str>, size: CaptureSize) -> Result<Self> {
        let (session, is_virtual) = MutterSession::open_with_info(connector).await?;
        let node_id = session.wait_for_pipewire_node().await?;

        if is_virtual {
            tracing::info!(
                "no physical monitor — acting as display via platform virtual \
                 monitor, negotiating {}x{}",
                size.width,
                size.height
            );
            let w = size.width;
            let h = size.height;
            // GStreamer negotiation is blocking; run off the async runtime.
            let consumer = tokio::task::spawn_blocking(move || {
                KeepaliveConsumer::start(node_id, w, h)
            })
            .await
            .context("keepalive join")??;
            capture::install_keepalive(consumer);
        } else {
            tracing::info!("using physical/monitor stream pipewire node={node_id}");
        }

        Ok(Self {
            session,
            size,
            node_id,
            is_virtual,
        })
    }

    pub fn session(&self) -> &MutterSession {
        &self.session
    }

    pub fn node_id(&self) -> u32 {
        // Prefer keepalive node if installed (same id today, but kept explicit).
        capture::keepalive_node().unwrap_or(self.node_id)
    }

    pub fn size(&self) -> CaptureSize {
        CaptureSize {
            width: self.size.width,
            height: self.size.height,
        }
    }

    pub fn is_virtual(&self) -> bool {
        self.is_virtual
    }

    pub async fn capture_png(&self) -> Result<Vec<u8>> {
        let node = self.node_id();
        let w = self.size.width;
        let h = self.size.height;
        tokio::task::spawn_blocking(move || capture::capture_single_frame_png(node, w, h))
            .await
            .context("capture join")?
    }
}

pub type SharedDisplay = Arc<Mutex<DisplayProvider>>;
