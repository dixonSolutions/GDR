//! Grabs one frame from the PipeWire node Mutter's ScreenCast API hands us,
//! and returns it as PNG bytes. Uses GStreamer's `pipewiresrc` element,
//! which does all the SPA format negotiation for us instead of us
//! hand-rolling raw PipeWire buffer/format negotiation.
//!
//! Requires on the target machine:
//!   gstreamer1.0-pipewire gstreamer1.0-plugins-good gstreamer1.0-plugins-base

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;

pub fn init() -> Result<()> {
    gst::init().context("gst::init failed - is gstreamer installed on this system?")
}

/// Pulls exactly one video frame from the given PipeWire node id and
/// encodes it as PNG.
pub fn capture_single_frame_png(node_id: u32) -> Result<Vec<u8>> {
    let pipeline_desc = format!(
        "pipewiresrc path={node_id} do-timestamp=true ! \
         videoconvert ! video/x-raw,format=RGBA ! \
         appsink name=sink sync=false max-buffers=1 drop=true"
    );

    let pipeline = gst::parse::launch(&pipeline_desc)
        .context("failed to build gstreamer pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("pipeline downcast failed"))?;

    let appsink = pipeline
        .by_name("sink")
        .ok_or_else(|| anyhow!("appsink not found in pipeline"))?
        .downcast::<AppSink>()
        .map_err(|_| anyhow!("appsink downcast failed"))?;

    // Cap how long we wait for a frame so a dead PipeWire node doesn't
    // hang the connection forever.
    appsink.set_property("max-buffers", 1u32);
    appsink.set_property("drop", true);

    pipeline
        .set_state(gst::State::Playing)
        .context("failed to start capture pipeline")?;

    let sample = appsink
        .try_pull_sample(gst::ClockTime::from_seconds(5))
        .ok_or_else(|| anyhow!("no frame received from pipewiresrc within 5s - is the stream live?"))?;

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
        let _ = pipeline.set_state(gst::State::Null);
        return Err(anyhow!(
            "frame buffer too small: got {} bytes, need {expected} for {width}x{height}",
            map.as_slice().len()
        ));
    }

    let img = image::RgbaImage::from_raw(
        width as u32,
        height as u32,
        map.as_slice()[..expected].to_vec(),
    )
    .ok_or_else(|| anyhow!("frame size mismatch building image buffer"))?;

    let _ = pipeline.set_state(gst::State::Null);

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
