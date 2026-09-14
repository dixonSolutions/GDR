//! PipeWire capture via GStreamer `pipewiresrc`.
//!
//! For `RecordVirtual` / platform virtual monitors, Mutter sizes the
//! monitor from **PipeWire format negotiation**. We keep one long-lived
//! appsink consumer negotiated at 1920×1080 so gdrd *is* the display;
//! screenshots pull from that same appsink (a second `pipewiresrc` on
//! the same node will stall).

use anyhow::{anyhow, Context, Result};
use common::{ImageFormat, Region, Settle};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_WIDTH: i32 = 1920;
pub const DEFAULT_HEIGHT: i32 = 1080;

/// How often to re-check for new PipeWire buffers while waiting for the
/// screen to go quiet. Well under the 120 ms default quiet window, so
/// settle latency is dominated by the real repaint, not the poll.
const SETTLE_POLL: Duration = Duration::from_millis(10);

/// How long to wait for the stream's first frame before declaring the
/// consumer dead. Measured attach times are 30–400 ms, so this is roughly
/// 4x headroom over the slowest healthy start observed.
const PREROLL_WAIT: gst::ClockTime = gst::ClockTime::from_mseconds(1500);

pub const DEFAULT_JPEG_QUALITY: u8 = 85;

/// What to capture and how to encode it.
#[derive(Clone, Debug, Default)]
pub struct FrameOptions {
    pub region: Option<Region>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub max_long_edge: Option<u32>,
    /// Vision-model tiling budget in `patch_size` cells. See [`fit_inside`].
    pub max_patches: Option<u32>,
    pub patch_size: Option<u32>,
    pub format: ImageFormat,
    pub quality: Option<u8>,
    pub settle: Option<Settle>,
}

/// A cropped, resized, encoded frame plus the geometry needed to map
/// image-space coordinates back to Mutter stream pixels.
#[derive(Clone, Debug)]
pub struct CapturedFrame {
    pub data: Vec<u8>,
    pub format: ImageFormat,
    pub native_width: u32,
    pub native_height: u32,
    pub region: Region,
    pub image_width: u32,
    pub image_height: u32,
    pub hash: String,
    /// False when a requested settle timed out instead of going quiet.
    pub settled: bool,
}

pub fn init() -> Result<()> {
    gst::init().context("gst::init failed - is gstreamer installed on this system?")
}

/// Long-lived PipeWire consumer: holds 1920×1080 negotiation open and
/// exposes an appsink for screenshot pulls.
pub struct KeepaliveConsumer {
    pipeline: gst::Pipeline,
    appsink: AppSink,
    node_id: u32,
    prerolled: bool,
}

impl KeepaliveConsumer {
    /// Start a long-lived PipeWire consumer.
    ///
    /// When `width`/`height` are both `> 0`, appsink caps pin that size
    /// (needed for headless Meta-* negotiation). When either is `0`, only
    /// `format=RGBA` is required so physical panels can keep native size.
    pub fn start(node_id: u32, width: i32, height: i32) -> Result<Self> {
        // Same shape as the proven Python probe:
        //   pipewiresrc ! videoconvert ! appsink(caps=RGBA[,WxH])
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
            // Keep the most recent frame even when the stream goes idle
            // (static headless Meta-* often stops pushing).
            .property("enable-last-sample", true)
            .build()?;
        let caps = if width > 0 && height > 0 {
            gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .field("width", width)
                .field("height", height)
                .build()
        } else {
            gst::Caps::builder("video/x-raw")
                .field("format", "RGBA")
                .build()
        };
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
                        note_stream_size(w as u32, h as u32);
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

        // Preroll one frame so we know the sink is actually fed.
        //
        // This is pass/fail rather than slow/fast: when the stream attaches
        // cleanly the first buffer lands in well under a second, and when it
        // loses the startup race against Mutter no buffer ever arrives. So a
        // short budget costs nothing on the happy path and reaches the
        // caller's retry sooner on the unhappy one.
        let prerolled = appsink.try_pull_sample(PREROLL_WAIT).is_some() && !fault_preroll();
        if !prerolled {
            tracing::warn!("keepalive attached to node {node_id} but received no frames");
        }

        Ok(Self {
            pipeline,
            appsink,
            node_id,
            prerolled,
        })
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Whether the stream delivered a frame at startup.
    ///
    /// `false` means the consumer is attached but dead: reconnecting to the
    /// same node will not revive it, so the whole Mutter session has to go.
    pub fn prerolled(&self) -> bool {
        self.prerolled
    }

    /// Wait for the stream to stop emitting damage.
    ///
    /// Returns the newest sample seen while waiting (so the caller does not
    /// have to re-pull one we already drained) and whether the screen
    /// actually went quiet before `timeout_ms`.
    fn wait_settled(&self, settle: Settle) -> (Option<gst::Sample>, bool) {
        let start = Instant::now();
        let quiet = Duration::from_millis(settle.quiet_ms);
        let timeout = Duration::from_millis(settle.timeout_ms);
        let mut newest = None;
        let mut last_change = start;
        loop {
            let mut saw_frame = false;
            while let Some(s) = self.appsink.try_pull_sample(gst::ClockTime::ZERO) {
                newest = Some(s);
                saw_frame = true;
            }
            let now = Instant::now();
            if saw_frame {
                last_change = now;
            }
            if now.duration_since(last_change) >= quiet {
                return (newest, true);
            }
            if now.duration_since(start) >= timeout {
                tracing::debug!(
                    "settle timed out after {}ms — screen still repainting",
                    settle.timeout_ms
                );
                return (newest, false);
            }
            std::thread::sleep(SETTLE_POLL);
        }
    }

    /// Newest available frame, preferring queued buffers over the retained
    /// `last-sample`.
    ///
    /// Damage-driven streams stop pushing entirely when the desktop is
    /// static, so `last-sample` is checked *before* any timed pull — waiting
    /// for a "new" frame that will never arrive used to cost 800 ms on every
    /// capture of an idle screen.
    fn newest_sample(&self, seed: Option<gst::Sample>) -> Result<gst::Sample> {
        let mut newest = seed;
        while let Some(s) = self.appsink.try_pull_sample(gst::ClockTime::ZERO) {
            newest = Some(s);
        }
        newest
            .or_else(|| self.appsink.property::<Option<gst::Sample>>("last-sample"))
            .or_else(|| {
                self.appsink
                    .try_pull_sample(gst::ClockTime::from_mseconds(800))
            })
            .or_else(|| {
                self.appsink
                    .try_pull_sample(gst::ClockTime::from_seconds(5))
            })
            .ok_or_else(|| anyhow!(NoFrame))
    }

    pub fn capture_frame(&self, opts: &FrameOptions) -> Result<CapturedFrame> {
        let (seed, settled) = match opts.settle {
            Some(s) => self.wait_settled(s),
            None => (None, true),
        };
        let sample = self.newest_sample(seed)?;
        let mut frame = sample_to_frame(&sample, opts)?;
        frame.settled = settled;
        Ok(frame)
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

/// Fault injection seam: report the next `GDR_FAULT_PREROLL` stream startups
/// as dead.
///
/// The failure this guards against depends on Mutter losing a startup race,
/// which cannot be provoked on demand — so without a seam the recovery path
/// would ship untested. Absent env var means zero, so production is unaffected.
fn fault_preroll() -> bool {
    static REMAINING: std::sync::OnceLock<AtomicUsize> = std::sync::OnceLock::new();
    let remaining = REMAINING.get_or_init(|| {
        AtomicUsize::new(
            std::env::var("GDR_FAULT_PREROLL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
        )
    });
    remaining
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            n.checked_sub(1)
        })
        .is_ok()
}

/// A consumer that negotiated caps but never delivered a buffer.
///
/// Distinguished from other capture failures because it is the one kind that
/// a pipeline rebuild can plausibly fix, and the only one worth retrying.
#[derive(Debug)]
pub struct NoFrame;

impl std::fmt::Display for NoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no frame from the capture stream")
    }
}

impl std::error::Error for NoFrame {}

static KEEPALIVE: Mutex<Option<KeepaliveConsumer>> = Mutex::new(None);

/// Size of the frames the capture stream is actually producing.
///
/// The window plane needs this to turn a window's logical rect into a crop
/// region, and a *measured* size is worth more than the compositor's declared
/// scale factor: on a 1920x1200 panel at 125% the stage is 1536x960, and only
/// the ratio of the two gets a crop onto the right pixels. Recorded at caps
/// negotiation so it is known before the first capture, then kept current by
/// every frame.
static STREAM_SIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

pub fn note_stream_size(width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    *STREAM_SIZE.lock().unwrap() = Some((width, height));
}

/// Last observed capture size, or `None` before the stream negotiated.
pub fn stream_size() -> Option<(u32, u32)> {
    *STREAM_SIZE.lock().unwrap()
}

pub fn install_keepalive(consumer: KeepaliveConsumer) {
    *KEEPALIVE.lock().unwrap() = Some(consumer);
}

/// Drop the long-lived PipeWire consumer (stops GStreamer pipeline).
pub fn clear_keepalive() {
    *STREAM_SIZE.lock().unwrap() = None;
    *KEEPALIVE.lock().unwrap() = None;
}

pub fn keepalive_node() -> Option<u32> {
    KEEPALIVE.lock().unwrap().as_ref().map(|k| k.node_id())
}

/// Whether the installed consumer saw a frame at startup.
pub fn keepalive_prerolled() -> bool {
    KEEPALIVE
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|k| k.prerolled())
}

pub fn capture_single_frame(
    node_id: u32,
    width: i32,
    height: i32,
    opts: &FrameOptions,
) -> Result<CapturedFrame> {
    {
        let guard = KEEPALIVE.lock().unwrap();
        if let Some(k) = guard.as_ref() {
            if k.node_id() == node_id {
                return k.capture_frame(opts);
            }
        }
    }
    KeepaliveConsumer::start(node_id, width, height)?.capture_frame(opts)
}

/// Intersect a requested crop with the frame.
///
/// Returns an error rather than silently falling back to the full frame: a
/// crop that misses the screen entirely is a caller bug, and quietly
/// returning the whole desktop would look like a working zoom.
fn clamp_region(region: Option<Region>, width: u32, height: u32) -> Result<Region> {
    let full = Region {
        x: 0,
        y: 0,
        width,
        height,
    };
    let Some(r) = region else { return Ok(full) };
    if r.width == 0 || r.height == 0 {
        return Err(anyhow!("empty region {}x{}", r.width, r.height));
    }
    // An origin outside the frame is rejected, not clamped. Clamping it
    // would yield a 1px sliver of the edge, which reads as a successful
    // capture of the wrong thing.
    if r.x >= width || r.y >= height {
        return Err(anyhow!(
            "region origin {},{} is outside the {width}x{height} frame",
            r.x,
            r.y
        ));
    }
    Ok(Region {
        x: r.x,
        y: r.y,
        width: r.width.min(width - r.x),
        height: r.height.min(height - r.y),
    })
}

/// Largest size satisfying every requested limit, aspect preserved,
/// never upscaling.
///
/// `max_patches` is the vision-model tiling budget: images are billed as
/// `ceil(w/patch) * ceil(h/patch)` cells, and exceeding the budget makes the
/// model API silently downscale — after which any coordinate the model
/// returns is in a space the caller never saw. A pixel box cannot express
/// that constraint (it depends on aspect ratio), so it is applied here,
/// where the native size is actually known.
fn fit_inside(width: u32, height: u32, opts: &FrameOptions) -> (u32, u32) {
    let mut scale: f64 = 1.0;
    let mut apply = |limit: Option<u32>, extent: u32| {
        if let Some(l) = limit.filter(|v| *v > 0) {
            scale = scale.min(l as f64 / extent as f64);
        }
    };
    apply(opts.max_width, width);
    apply(opts.max_height, height);
    apply(opts.max_long_edge, width.max(height));

    let Some(max_patches) = opts.max_patches.filter(|v| *v > 0) else {
        if scale >= 1.0 {
            return (width, height);
        }
        return (
            ((width as f64 * scale).round() as u32).max(1),
            ((height as f64 * scale).round() as u32).max(1),
        );
    };

    let patch = opts.patch_size.filter(|v| *v > 0).unwrap_or(28);
    // A source that already fits every limit is passed through untouched.
    // Snapping it down to a cell boundary would discard up to a patch of
    // real content to save tokens we were never going to spend.
    if scale >= 1.0 && width.div_ceil(patch) * height.div_ceil(patch) <= max_patches {
        return (width, height);
    }
    // Snap down to whole cells: a partially filled row costs a full row.
    let snap = |s: f64| -> (u32, u32) {
        (
            ((width as f64 * s / patch as f64).floor() as u32).max(1) * patch,
            ((height as f64 * s / patch as f64).floor() as u32).max(1) * patch,
        )
    };
    let cells = |w: u32, h: u32| -> u32 { w.div_ceil(patch) * h.div_ceil(patch) };

    let (mut w, mut h) = snap(scale);
    while cells(w, h) > max_patches && (w > patch || h > patch) {
        scale *= 0.98;
        let (nw, nh) = snap(scale);
        (w, h) = if nw == w && nh == h {
            (w.saturating_sub(patch).max(patch), h.saturating_sub(patch).max(patch))
        } else {
            (nw, nh)
        };
    }
    // Snapping up to a cell boundary must never enlarge the source.
    if w >= width && h >= height {
        return (width, height);
    }
    (w, h)
}

/// Non-cryptographic content hash for change detection.
///
/// Only ever compared against a hash this daemon produced, so speed matters
/// and collision resistance does not — this must not become an integrity
/// check. Salted with geometry and format so that changing any capture
/// parameter can never be mistaken for "screen unchanged".
fn frame_hash(pixels: &[u8], salt: &[u64]) -> String {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mix = |v: u64, h: &mut u64| {
        *h = (*h ^ v).wrapping_mul(K).rotate_left(31);
    };
    for s in salt {
        mix(*s, &mut h);
    }
    let mut chunks = pixels.chunks_exact(8);
    for c in &mut chunks {
        mix(u64::from_le_bytes(c.try_into().expect("8 bytes")), &mut h);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut buf = [0u8; 8];
        buf[..rest.len()].copy_from_slice(rest);
        mix(u64::from_le_bytes(buf), &mut h);
    }
    format!("{h:016x}")
}

/// Crop + convert to RGB in one pass.
///
/// Alpha is dropped rather than carried: ScreenCast frames are opaque, and
/// three channels halve the work for both the resizer and the encoder.
fn crop_to_rgb(
    src: &[u8],
    stride: usize,
    swap_rb: bool,
    region: Region,
) -> Result<image::RgbImage> {
    let w = region.width as usize;
    let h = region.height as usize;
    let mut out = Vec::with_capacity(w * h * 3);
    for row in 0..h {
        let start = (region.y as usize + row) * stride + region.x as usize * 4;
        let line = src
            .get(start..start + w * 4)
            .ok_or_else(|| anyhow!("frame buffer short at row {row}"))?;
        for px in line.chunks_exact(4) {
            if swap_rb {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            } else {
                out.extend_from_slice(&[px[0], px[1], px[2]]);
            }
        }
    }
    image::RgbImage::from_raw(region.width, region.height, out)
        .ok_or_else(|| anyhow!("failed to build {w}x{h} image buffer"))
}

/// Downscale RGB with SIMD.
///
/// `image::imageops::resize` has no vectorised path and costs ~73 ms for a
/// 1920×1200 → 1400×868 downscale, which was more than the rest of the
/// capture pipeline put together. This is ~11 ms for the same work.
/// CatmullRom keeps UI text sharp without the edge ringing Lanczos3 leaves
/// behind, which JPEG then has to spend bits on.
fn resize_rgb(src: image::RgbImage, width: u32, height: u32) -> Result<image::RgbImage> {
    use fast_image_resize::images::Image as FirImage;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let (sw, sh) = (src.width(), src.height());
    let mut raw = src.into_raw();
    let source = FirImage::from_slice_u8(sw, sh, &mut raw, PixelType::U8x3)
        .map_err(|e| anyhow!("resize source: {e}"))?;
    let mut dst = FirImage::new(width, height, PixelType::U8x3);
    Resizer::new()
        .resize(
            &source,
            &mut dst,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom)),
        )
        .map_err(|e| anyhow!("resize {sw}x{sh} -> {width}x{height}: {e}"))?;
    image::RgbImage::from_raw(width, height, dst.into_vec())
        .ok_or_else(|| anyhow!("resized buffer size mismatch"))
}

fn encode(img: &image::RgbImage, format: ImageFormat, quality: u8) -> Result<Vec<u8>> {
    use image::codecs::{jpeg::JpegEncoder, png::PngEncoder};
    use image::{ExtendedColorType, ImageEncoder};

    let mut out = Vec::new();
    match format {
        ImageFormat::Png => {
            PngEncoder::new(&mut out).write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                ExtendedColorType::Rgb8,
            )?;
        }
        ImageFormat::Jpeg => {
            // 4:4:4 (no chroma subsampling): costs ~15% more bytes and
            // removes the colour fringing 4:2:0 puts around sharp text.
            // Screenshots are read for their text.
            JpegEncoder::new_with_quality(&mut out, quality.clamp(1, 100))
                .encode_image(img)
                .context("jpeg encode")?;
        }
    }
    Ok(out)
}

fn sample_to_frame(sample: &gst::Sample, opts: &FrameOptions) -> Result<CapturedFrame> {
    let caps = sample.caps().ok_or_else(|| anyhow!("sample has no caps"))?;
    let s = caps.structure(0).ok_or_else(|| anyhow!("no caps structure"))?;
    let width: i32 = s.get("width")?;
    let height: i32 = s.get("height")?;
    if width <= 0 || height <= 0 {
        return Err(anyhow!("invalid frame size {width}x{height}"));
    }
    let (width, height) = (width as u32, height as u32);
    note_stream_size(width, height);

    let buffer = sample.buffer().ok_or_else(|| anyhow!("sample has no buffer"))?;
    let map = buffer.map_readable().context("failed to map buffer")?;
    let bytes = map.as_slice();

    // GStreamer may pad rows. Derive the real stride from the buffer rather
    // than assuming width*4, which silently skews the image when padded.
    let tight = (width as usize)
        .checked_mul(4)
        .ok_or_else(|| anyhow!("frame width overflow"))?;
    let stride = if bytes.len() % height as usize == 0 && bytes.len() / height as usize >= tight {
        bytes.len() / height as usize
    } else {
        tight
    };
    let needed = stride
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("frame dimensions overflow"))?;
    if bytes.len() < needed {
        return Err(anyhow!(
            "frame buffer too small: got {} bytes, need {needed} for {width}x{height}",
            bytes.len()
        ));
    }

    let pixel_format: String = s.get::<String>("format").unwrap_or_else(|_| "RGBA".into());
    let swap_rb = match pixel_format.as_str() {
        "RGBA" | "RGBx" => false,
        "BGRA" | "BGRx" => true,
        other => {
            return Err(anyhow!(
                "unsupported pixel format {other:?} (expected RGBA/BGRA)"
            ))
        }
    };

    let region = clamp_region(opts.region, width, height)?;
    let mut img = crop_to_rgb(bytes, stride, swap_rb, region)?;

    let (fit_w, fit_h) = fit_inside(region.width, region.height, opts);
    if fit_w != region.width || fit_h != region.height {
        img = resize_rgb(img, fit_w, fit_h)?;
    }

    let quality = opts.quality.unwrap_or(DEFAULT_JPEG_QUALITY);
    let hash = frame_hash(
        img.as_raw(),
        &[
            u64::from(img.width()) << 32 | u64::from(img.height()),
            u64::from(region.x) << 32 | u64::from(region.y),
            opts.format as u64,
            u64::from(quality),
        ],
    );
    let data = encode(&img, opts.format, quality)?;

    Ok(CapturedFrame {
        data,
        format: opts.format,
        native_width: width,
        native_height: height,
        region,
        image_width: img.width(),
        image_height: img.height(),
        hash,
        settled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: u32, y: u32, width: u32, height: u32) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn no_region_means_full_frame() {
        assert_eq!(clamp_region(None, 1920, 1200).unwrap(), region(0, 0, 1920, 1200));
    }

    #[test]
    fn region_is_clamped_to_frame_bounds() {
        // Overhangs the right/bottom edge: keep the origin, shrink the extent.
        assert_eq!(
            clamp_region(Some(region(1800, 1100, 400, 400)), 1920, 1200).unwrap(),
            region(1800, 1100, 120, 100)
        );
    }

    #[test]
    fn region_fully_outside_frame_is_an_error() {
        // Must not silently degrade to a full-desktop capture — that would
        // look like a working zoom while showing the wrong thing.
        assert!(clamp_region(Some(region(5000, 5000, 100, 100)), 1920, 1200).is_err());
        assert!(clamp_region(Some(region(0, 0, 0, 100)), 1920, 1200).is_err());
    }

    fn box_opts(w: u32, h: u32) -> FrameOptions {
        FrameOptions {
            max_width: Some(w),
            max_height: Some(h),
            ..Default::default()
        }
    }

    /// Claude standard tier: 1568 cells of 28px, long edge 1568.
    fn claude_opts() -> FrameOptions {
        FrameOptions {
            max_long_edge: Some(1568),
            max_patches: Some(1568),
            patch_size: Some(28),
            ..Default::default()
        }
    }

    fn cells(w: u32, h: u32) -> u32 {
        w.div_ceil(28) * h.div_ceil(28)
    }

    #[test]
    fn fit_preserves_aspect_and_never_upscales() {
        assert_eq!(fit_inside(1920, 1200, &box_opts(1440, 900)), (1440, 900));
        assert_eq!(fit_inside(1920, 1080, &box_opts(1440, 900)), (1440, 810));
        // Already smaller than the box: untouched.
        assert_eq!(fit_inside(800, 600, &box_opts(1440, 900)), (800, 600));
        // No limits at all: native.
        assert_eq!(fit_inside(1920, 1200, &FrameOptions::default()), (1920, 1200));
    }

    #[test]
    fn fit_result_always_fits_inside_the_box() {
        for (w, h) in [(1919u32, 1201u32), (3441, 1441), (1366, 769), (2257, 1505)] {
            let (fw, fh) = fit_inside(w, h, &box_opts(1344, 840));
            assert!(fw <= 1344 && fh <= 840, "{w}x{h} -> {fw}x{fh}");
        }
    }

    #[test]
    fn patch_budget_is_respected_for_every_aspect_ratio() {
        // The bug this exists to prevent: 1920x1200 fitted to a 1440x900
        // pixel box costs 52*33 = 1716 cells, over the 1568 budget, so the
        // model API downscales again behind our back and every click is
        // remapped from a size the model never saw.
        assert!(cells(1440, 900) > 1568);

        for (w, h) in [
            (1920u32, 1200u32), // 16:10 — the case that was broken
            (1920, 1080),       // 16:9
            (3840, 2160),       // 4K
            (3440, 1440),       // ultrawide
            (2256, 1504),       // 3:2 laptop
            (2880, 1800),       // retina-class 16:10
            (1024, 768),        // 4:3, already small
        ] {
            let (fw, fh) = fit_inside(w, h, &claude_opts());
            assert!(
                cells(fw, fh) <= 1568,
                "{w}x{h} -> {fw}x{fh} = {} cells",
                cells(fw, fh)
            );
            assert!(fw <= w && fh <= h, "{w}x{h} upscaled to {fw}x{fh}");
            // Aspect ratio must survive, or clicks land in the wrong place.
            let drift = (fw as f64 / fh as f64) - (w as f64 / h as f64);
            assert!(drift.abs() < 0.05, "{w}x{h} -> {fw}x{fh} aspect drift {drift}");
        }
    }

    #[test]
    fn patch_budget_uses_most_of_the_allowance() {
        // Fitting is only useful if it doesn't leave half the budget unused.
        let (w, h) = fit_inside(1920, 1200, &claude_opts());
        assert!(cells(w, h) > 1200, "{w}x{h} = {} cells is wasteful", cells(w, h));
    }

    #[test]
    fn small_source_is_not_upscaled_to_fill_the_budget() {
        assert_eq!(fit_inside(640, 480, &claude_opts()), (640, 480));
    }

    #[test]
    fn hash_detects_a_single_changed_pixel() {
        let a = vec![7u8; 4096];
        let mut b = a.clone();
        b[2048] = 8;
        assert_eq!(frame_hash(&a, &[1]), frame_hash(&a, &[1]));
        assert_ne!(frame_hash(&a, &[1]), frame_hash(&b, &[1]));
    }

    #[test]
    fn hash_is_salted_by_capture_parameters() {
        // Same pixels at a different size/format must not report "unchanged".
        let px = vec![3u8; 512];
        assert_ne!(frame_hash(&px, &[1, 2]), frame_hash(&px, &[1, 3]));
    }

    #[test]
    fn crop_respects_row_stride_and_channel_order() {
        // 3x2 BGRA frame with 4 bytes of row padding, cropped to the
        // bottom-right 2x1. Padded strides are the classic source of
        // diagonally-skewed captures.
        let width = 3usize;
        let stride = width * 4 + 4;
        let mut buf = vec![0u8; stride * 2];
        for y in 0..2usize {
            for x in 0..width {
                let p = y * stride + x * 4;
                buf[p] = 10 * (x as u8 + 1); // B
                buf[p + 1] = 100 + y as u8; // G
                buf[p + 2] = 200 + x as u8; // R
                buf[p + 3] = 255;
            }
        }
        let img = crop_to_rgb(&buf, stride, true, region(1, 1, 2, 1)).unwrap();
        assert_eq!(img.dimensions(), (2, 1));
        // swap_rb → stored as R,G,B taken from B,G,R source bytes.
        assert_eq!(img.get_pixel(0, 0).0, [201, 101, 20]);
        assert_eq!(img.get_pixel(1, 0).0, [202, 101, 30]);
    }

    #[test]
    fn jpeg_encodes_smaller_than_png_for_the_same_frame() {
        let img = image::RgbImage::from_fn(320, 200, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let png = encode(&img, ImageFormat::Png, 85).unwrap();
        let jpeg = encode(&img, ImageFormat::Jpeg, 85).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        assert!(jpeg.len() < png.len(), "jpeg {} png {}", jpeg.len(), png.len());
    }
}
