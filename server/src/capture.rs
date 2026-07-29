//! PipeWire capture via GStreamer `pipewiresrc`.
//!
//! For `RecordVirtual` / platform virtual monitors, Mutter sizes the
//! monitor from **PipeWire format negotiation**. We keep one long-lived
//! appsink consumer negotiated at 1920×1080 so gdrd *is* the display;
//! screenshots pull from that same appsink (a second `pipewiresrc` on
//! the same node will stall).

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use std::sync::Mutex;

pub const DEFAULT_WIDTH: i32 = 1920;
pub const DEFAULT_HEIGHT: i32 = 1080;

pub fn init() -> Result<()> {
    gst::init().context("gst::init failed - is gstreamer installed on this system?")
}

/// Long-lived PipeWire consumer: holds 1920×1080 negotiation open and
/// exposes an appsink for screenshot pulls.
pub struct KeepaliveConsumer {
    pipeline: gst::Pipeline,
    appsink: AppSink,
    node_id: u32,
}

impl KeepaliveConsumer {
    pub fn start(node_id: u32, width: i32, height: i32) -> Result<Self> {
        // Same shape as the proven Python probe:
        //   pipewiresrc ! videoconvert ! appsink(caps=RGBA,WxH)
        let src = gst::ElementFactory::make("pipewiresrc")
            .name("src")
            .property("path", format!("{node_id}"))
            .property("do-timestamp", true)
            .build()
            .context("make pipewiresrc")?;
        let conv = gst::ElementFactory::make("videoconvert")
            .name("conv")
            .build()?;
        let sink = gst::ElementFactory::make("appsink")
            .name("sink")
            .property("sync", false)
            .property("max-buffers", 4u32)
            .property("drop", true)
            .property("emit-signals", false)
            .build()?;
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", width)
            .field("height", height)
            .build();
        sink.set_property("caps", &caps);

        let pipeline = gst::Pipeline::default();
        pipeline.add_many([&src, &conv, &sink])?;
        gst::Element::link_many([&src, &conv, &sink]).context("link capture pipeline")?;

        let appsink = sink
            .downcast::<AppSink>()
            .map_err(|_| anyhow!("appsink downcast"))?;

        pipeline
            .set_state(gst::State::Playing)
            .context("keepalive Playing")?;

        let src_el = pipeline
            .by_name("src")
            .ok_or_else(|| anyhow!("src missing"))?;
        let pad = src_el
            .static_pad("src")
            .ok_or_else(|| anyhow!("src pad"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(c) = pad.current_caps() {
                if let Some(s) = c.structure(0) {
                    let w: i32 = s.get("width").unwrap_or(0);
                    let h: i32 = s.get("height").unwrap_or(0);
                    if w >= 640 && h >= 480 {
                        tracing::info!(
                            "keepalive negotiated {w}x{h} on pipewire node {node_id}"
                        );
                        break;
                    }
                }
            }
            if std::time::Instant::now() > deadline {
                let _ = pipeline.set_state(gst::State::Null);
                return Err(anyhow!(
                    "keepalive did not negotiate >=640x480 within 5s (node {node_id})"
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // Preroll one frame so we know the sink is fed.
        if appsink
            .try_pull_sample(gst::ClockTime::from_seconds(3))
            .is_none()
        {
            tracing::warn!("keepalive preroll: no sample yet (will retry on capture)");
        }

        Ok(Self {
            pipeline,
            appsink,
            node_id,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub fn capture_png(&self) -> Result<Vec<u8>> {
        // Drain stale buffers, then wait for a fresh one.
        while self.appsink.try_pull_sample(gst::ClockTime::ZERO).is_some() {}
        let sample = self
            .appsink
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .ok_or_else(|| anyhow!("no frame from keepalive appsink within 5s"))?;
        sample_to_png(&sample)
    }
}

impl Drop for KeepaliveConsumer {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

unsafe impl Send for KeepaliveConsumer {}

#[derive(Clone, Copy, Debug)]
pub struct CaptureSize {
    pub width: i32,
    pub height: i32,
}

impl Default for CaptureSize {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        }
    }
}

static KEEPALIVE: Mutex<Option<KeepaliveConsumer>> = Mutex::new(None);

pub fn install_keepalive(consumer: KeepaliveConsumer) {
    *KEEPALIVE.lock().unwrap() = Some(consumer);
}

/// Drop the long-lived PipeWire consumer (stops GStreamer pipeline).
pub fn clear_keepalive() {
    *KEEPALIVE.lock().unwrap() = None;
}

pub fn keepalive_node() -> Option<u32> {
    KEEPALIVE.lock().unwrap().as_ref().map(|k| k.node_id())
}

pub fn capture_single_frame_png(node_id: u32, width: i32, height: i32) -> Result<Vec<u8>> {
    {
        let guard = KEEPALIVE.lock().unwrap();
        if let Some(k) = guard.as_ref() {
            if k.node_id() == node_id {
                return k.capture_png();
            }
        }
    }
    oneshot_capture(node_id, width, height)
}

fn oneshot_capture(node_id: u32, width: i32, height: i32) -> Result<Vec<u8>> {
    let consumer = KeepaliveConsumer::start(node_id, width, height)?;
    consumer.capture_png()
}

fn sample_to_png(sample: &gst::Sample) -> Result<Vec<u8>> {
    let caps = sample.caps().ok_or_else(|| anyhow!("sample has no caps"))?;
    let s = caps.structure(0).ok_or_else(|| anyhow!("no caps structure"))?;
    let width: i32 = s.get("width")?;
    let height: i32 = s.get("height")?;

    let buffer = sample.buffer().ok_or_else(|| anyhow!("sample has no buffer"))?;
    let map = buffer.map_readable().context("failed to map buffer")?;

    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| anyhow!("frame dimensions overflow"))?;
    if map.as_slice().len() < expected {
        return Err(anyhow!(
            "frame buffer too small: got {} bytes, need {expected} for {width}x{height}",
            map.as_slice().len()
        ));
    }

    let format: String = s.get::<String>("format").unwrap_or_else(|_| "RGBA".into());
    let rgba = match format.as_str() {
        "RGBA" | "RGBx" => map.as_slice()[..expected].to_vec(),
        "BGRA" | "BGRx" => {
            let mut out = map.as_slice()[..expected].to_vec();
            for px in out.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            out
        }
        other => {
            return Err(anyhow!(
                "unsupported pixel format {other:?} (expected RGBA/BGRA)"
            ));
        }
    };

    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba)
        .ok_or_else(|| anyhow!("frame size mismatch building image buffer"))?;

    let mut png_bytes = Vec::new();
    {
        use image::codecs::png::PngEncoder;
        use image::ExtendedColorType;
        use image::ImageEncoder;
        PngEncoder::new(&mut png_bytes).write_image(
            img.as_raw(),
            width as u32,
            height as u32,
            ExtendedColorType::Rgba8,
        )?;
    }
    Ok(png_bytes)
}
