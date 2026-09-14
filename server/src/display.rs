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

use crate::capture::{self, CaptureSize, CapturedFrame, FrameOptions, KeepaliveConsumer};
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
    /// Open a session, retrying once if the capture stream comes up dead.
    ///
    /// Mutter occasionally hands out a PipeWire node that never produces
    /// buffers. Reattaching to that node does not help and dropping the last
    /// consumer can invalidate it outright, so recovery means replacing the
    /// whole session — which yields a fresh node. Without this, the first
    /// screenshot after an idle teardown fails outright a fair fraction of
    /// the time.
    pub async fn start(connector: Option<&str>, size: CaptureSize) -> Result<Self> {
        let provider = Self::try_start(connector, size).await?;
        if capture::keepalive_prerolled() {
            return Ok(provider);
        }
        tracing::warn!("capture stream came up dead — restarting Mutter session");
        provider.shutdown().await;
        let provider = Self::try_start(connector, size).await?;
        if !capture::keepalive_prerolled() {
            // Both attempts produced nothing. Say so here rather than logging
            // "display provider ready" and letting the operator discover it
            // as an unexplained capture error minutes later — which is
            // exactly how this reads in the wild: a startup log that looks
            // clean, then every screenshot failing for no stated reason.
            tracing::warn!(
                "capture stream is silent after a session restart — the compositor is                  not painting this monitor, so screenshots will fail until it is.                  Common cause: a --devkit / mdk session whose viewer is not showing                  the monitor, or a virtual monitor with no consumer."
            );
        }
        Ok(provider)
    }

    async fn try_start(connector: Option<&str>, size: CaptureSize) -> Result<Self> {
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

    /// Connector being streamed, or `None` on a platform virtual monitor.
    pub fn connector(&self) -> Option<&str> {
        self.session.connector.as_deref()
    }

    pub async fn capture_png(&self) -> Result<Vec<u8>> {
        Ok(self.capture_frame(FrameOptions::default()).await?.data)
    }

    pub async fn capture_frame(&self, opts: FrameOptions) -> Result<CapturedFrame> {
        let node = self.node_id();
        let w = self.size.width;
        let h = self.size.height;
        // GStreamer pulls, resize and encode all block; keep them off the runtime.
        tokio::task::spawn_blocking(move || capture::capture_single_frame(node, w, h, &opts))
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
            // "ready" is a claim, so make it one that survives inspection:
            // a provider whose stream never prerolled is attached, not ready.
            tracing::info!(
                "display provider {} (virtual={}, node={}, {}x{})",
                if capture::keepalive_prerolled() {
                    "ready"
                } else {
                    "attached but producing no frames"
                },
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
        self.inner.as_ref().expect("ensure").capture_png().await
    }

    pub async fn capture_frame(&mut self, opts: FrameOptions) -> Result<CapturedFrame> {
        self.ensure().await?;
        self.inner
            .as_ref()
            .expect("ensure")
            .capture_frame(opts)
            .await
    }

    /// Connector currently being captured, *without* starting ScreenCast.
    ///
    /// The window plane needs to know which monitor a crop would come from,
    /// but listing windows must not be what opens a capture session — the
    /// whole point of lazy display is that gdrd is not broadcasting until
    /// someone asks for pixels. Before the first capture this falls back to
    /// the configured connector, which is `None` on autodetect.
    pub fn capture_connector(&self) -> Option<String> {
        self.inner
            .as_ref()
            .and_then(|p| p.connector().map(str::to_string))
            .or_else(|| self.cfg.connector.clone())
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
