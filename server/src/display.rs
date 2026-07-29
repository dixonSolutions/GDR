//! Display / ScreenCast lifecycle for gdrd.
//!
//! Mutter RemoteDesktop + ScreenCast is **not** held open forever by default.
//! Physical monitors are captured only while a client is actively using
//! screenshot/input; after an idle timeout the session is torn down so the
//! desktop is not broadcast 24/7.
//!
//! Platform virtual monitors (headless) stay up once started — tearing them
//! down would remove the session's only display. Use `--eager-display` on
//! headless hosts if you want Meta-* at boot.

use crate::capture::{self, CaptureSize, KeepaliveConsumer};
use crate::mutter_dbus::MutterSession;
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
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

        let (w, h) = if is_virtual {
            tracing::info!(
                "no physical monitor — acting as display via platform virtual \
                 monitor, negotiating {}x{}",
                size.width,
                size.height
            );
            (size.width, size.height)
        } else {
            tracing::info!(
                "using physical/monitor stream pipewire node={node_id} \
                 (native resolution; not forcing {}x{})",
                size.width,
                size.height
            );
            // 0,0 → RGBA-only caps so eDP panels keep their native size.
            (0, 0)
        };
        // GStreamer negotiation is blocking; run off the async runtime.
        let consumer = tokio::task::spawn_blocking(move || {
            KeepaliveConsumer::start(node_id, w, h)
        })
        .await
        .context("keepalive join")??;
        capture::install_keepalive(consumer);

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

    /// Stop PipeWire keepalive + Mutter RemoteDesktop/ScreenCast.
    pub async fn shutdown(self) {
        capture::clear_keepalive();
        self.session.shutdown().await;
    }
}

/// Tunables for when ScreenCast is allowed to run.
#[derive(Clone, Debug)]
pub struct DisplayConfig {
    pub connector: Option<String>,
    pub size: CaptureSize,
    /// Tear down physical capture after this many idle seconds.
    /// `0` means never idle-stop (once started, keep until process exit).
    pub idle_secs: u64,
    /// Never idle-stop platform virtual monitors (default true).
    pub keep_virtual: bool,
}

/// Lazy + idle-aware holder around [`DisplayProvider`].
pub struct DisplayManager {
    cfg: DisplayConfig,
    inner: Option<DisplayProvider>,
    last_used: Option<Instant>,
}

impl DisplayManager {
    pub fn new(cfg: DisplayConfig) -> Self {
        Self {
            cfg,
            inner: None,
            last_used: None,
        }
    }

    /// Start capture immediately (used with `--eager-display`).
    pub async fn warm_start(&mut self) -> Result<()> {
        self.ensure().await?;
        Ok(())
    }

    async fn ensure(&mut self) -> Result<()> {
        if self.inner.is_none() {
            tracing::info!("starting Mutter display session (on demand)");
            let provider = DisplayProvider::start(
                self.cfg.connector.as_deref(),
                CaptureSize {
                    width: self.cfg.size.width,
                    height: self.cfg.size.height,
                },
            )
            .await
            .context("start display provider")?;
            tracing::info!(
                "display provider ready (virtual={}, node={}, {}x{})",
                provider.is_virtual(),
                provider.node_id(),
                provider.size().width,
                provider.size().height
            );
            self.inner = Some(provider);
        }
        self.last_used = Some(Instant::now());
        Ok(())
    }

    pub async fn capture_png(&mut self) -> Result<Vec<u8>> {
        self.ensure().await?;
        self.inner
            .as_ref()
            .expect("ensure")
            .capture_png()
            .await
    }

    /// Ensure ScreenCast is up; returns the live Mutter session.
    pub async fn session(&mut self) -> Result<&MutterSession> {
        self.ensure().await?;
        Ok(self.inner.as_ref().expect("ensure").session())
    }

    /// Stop ScreenCast if idle long enough. No-op for virtual when
    /// `keep_virtual`, or when `idle_secs == 0`.
    pub async fn idle_reap(&mut self) {
        let Some(provider) = self.inner.as_ref() else {
            return;
        };
        if self.cfg.idle_secs == 0 {
            return;
        }
        if provider.is_virtual() && self.cfg.keep_virtual {
            return;
        }
        let Some(last) = self.last_used else {
            return;
        };
        if last.elapsed() < Duration::from_secs(self.cfg.idle_secs) {
            return;
        }
        tracing::info!(
            "display idle for {}s — tearing down Mutter ScreenCast",
            self.cfg.idle_secs
        );
        self.stop().await;
    }

    pub async fn stop(&mut self) {
        if let Some(provider) = self.inner.take() {
            provider.shutdown().await;
            self.last_used = None;
            tracing::info!("display provider stopped");
        }
    }
}

pub type SharedDisplay = Arc<Mutex<DisplayManager>>;
