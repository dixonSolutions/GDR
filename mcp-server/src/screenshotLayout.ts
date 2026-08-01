/**
 * Post-process gdrd PNG screenshots into an agent-friendly click layout.
 *
 * Capture stays native on the wire; MCP owns geometry metadata, optional
 * downscale (fit inside 1440×900), and remapping click/move coords from
 * image space back to Mutter stream pixels.
 *
 * Assumption (enforced in gdrd, not here): `native_width`/`native_height`
 * from the PNG IHDR equal the PipeWire buffer size for the ScreenCast
 * stream node passed to `NotifyPointerMotionAbsolute`. Today gdrd pulls
 * PNG frames from the same keepalive appsink bound to that stream, so the
 * sizes match by construction. If screenshot and stream ever diverge
 * (portal path, multi-monitor stitch vs single-stream node), remapping
 * stays internally consistent but is absolutely wrong — systematic drift.
 *
 * Frame geometry is only replaced on the next `gdr_screenshot` for a
 * device key. Resize / workspace / monitor changes are not observed here;
 * agents must re-screenshot before clicking after those.
 */

import sharp from "sharp";

export type LayoutMode = "raw" | "agent";

/** OpenAI CUA-recommended desktop band; also under common Claude long-edge budgets. */
export const AGENT_MAX_WIDTH = 1440;
export const AGENT_MAX_HEIGHT = 900;

export interface FrameGeometry {
  native_width: number;
  native_height: number;
  image_width: number;
  image_height: number;
  layout: LayoutMode;
  click_space: "image" | "native";
}

export interface ScreenshotMeta extends FrameGeometry {
  note: string;
}

export interface AppliedLayout {
  png_base64: string;
  meta: ScreenshotMeta;
}

/** Thrown when click/move remapping runs before any screenshot for that device. */
export class NoFrameGeometryError extends Error {
  readonly key: string;

  constructor(key: string) {
    super(
      `no screenshot geometry for device "${key}" — call gdr_screenshot before gdr_click/gdr_move`
    );
    this.name = "NoFrameGeometryError";
    this.key = key;
  }
}

const PNG_SIG = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/** Read width/height from PNG IHDR (no full decode). */
export function parsePngSize(buf: Buffer): { width: number; height: number } {
  if (buf.length < 24) {
    throw new Error("PNG too short to contain IHDR");
  }
  if (!buf.subarray(0, 8).equals(PNG_SIG)) {
    throw new Error("not a PNG (bad signature)");
  }
  const chunkLen = buf.readUInt32BE(8);
  const chunkType = buf.subarray(12, 16).toString("ascii");
  if (chunkType !== "IHDR" || chunkLen < 8) {
    throw new Error(`expected IHDR chunk, got ${chunkType}`);
  }
  const width = buf.readUInt32BE(16);
  const height = buf.readUInt32BE(20);
  if (width === 0 || height === 0) {
    throw new Error(`invalid PNG size ${width}x${height}`);
  }
  return { width, height };
}

/** Largest integer size that fits inside maxW×maxH preserving aspect ratio. */
export function fitInside(
  width: number,
  height: number,
  maxW: number = AGENT_MAX_WIDTH,
  maxH: number = AGENT_MAX_HEIGHT
): { width: number; height: number } {
  if (width <= maxW && height <= maxH) {
    return { width, height };
  }
  const scale = Math.min(maxW / width, maxH / height);
  let w = Math.max(1, Math.round(width * scale));
  let h = Math.max(1, Math.round(height * scale));
  // Guard rounding that can push one edge over the box by 1px.
  if (w > maxW) w = maxW;
  if (h > maxH) h = maxH;
  return { width: w, height: h };
}

/** Clamp to inclusive stream pixel range `[0, extent - 1]`. */
function clampStreamAxis(v: number, extent: number): number {
  if (extent <= 0) {
    throw new Error(`invalid stream extent ${extent}`);
  }
  return Math.min(Math.max(v, 0), extent - 1);
}

/**
 * Map image-space (x,y) to native stream pixels.
 * Requires a frame (no silent native fallback). Identity when sizes match.
 * Always clamps into `[0, native_* - 1]` so edge/off-by-one detector boxes
 * cannot produce out-of-range Mutter absolute coords.
 */
export function toStreamCoords(
  x: number,
  y: number,
  frame: Pick<
    FrameGeometry,
    "native_width" | "native_height" | "image_width" | "image_height" | "click_space"
  >
): { stream_x: number; stream_y: number; click_space: "image" | "native" } {
  if (
    frame.native_width <= 0 ||
    frame.native_height <= 0 ||
    frame.image_width <= 0 ||
    frame.image_height <= 0
  ) {
    throw new Error(
      `invalid frame geometry native=${frame.native_width}x${frame.native_height} ` +
        `image=${frame.image_width}x${frame.image_height}`
    );
  }

  let stream_x: number;
  let stream_y: number;
  if (
    frame.image_width === frame.native_width &&
    frame.image_height === frame.native_height
  ) {
    stream_x = x;
    stream_y = y;
  } else {
    stream_x = (x * frame.native_width) / frame.image_width;
    stream_y = (y * frame.native_height) / frame.image_height;
  }

  return {
    stream_x: clampStreamAxis(stream_x, frame.native_width),
    stream_y: clampStreamAxis(stream_y, frame.native_height),
    click_space: frame.click_space,
  };
}

function geometryNote(layout: LayoutMode, clickSpace: "image" | "native"): string {
  if (layout === "raw" || clickSpace === "native") {
    return "gdr_click/gdr_move x,y are native stream pixels (1:1 with this image)";
  }
  return (
    "gdr_click/gdr_move x,y are in image_width×image_height for this device " +
    "until the next gdr_screenshot (re-screenshot after resize/workspace/monitor change)"
  );
}

/**
 * Apply layout to a base64 PNG from gdrd.
 * `raw` — pass-through; `agent` — downscale to fit 1440×900 when larger.
 */
export async function applyScreenshotLayout(
  png_base64: string,
  layout: LayoutMode = "agent"
): Promise<AppliedLayout> {
  const buf = Buffer.from(png_base64, "base64");
  const native = parsePngSize(buf);

  if (layout === "raw") {
    const meta: ScreenshotMeta = {
      native_width: native.width,
      native_height: native.height,
      image_width: native.width,
      image_height: native.height,
      layout: "raw",
      click_space: "native",
      note: geometryNote("raw", "native"),
    };
    return { png_base64, meta };
  }

  const fitted = fitInside(native.width, native.height);
  if (fitted.width === native.width && fitted.height === native.height) {
    const meta: ScreenshotMeta = {
      native_width: native.width,
      native_height: native.height,
      image_width: native.width,
      image_height: native.height,
      layout: "agent",
      click_space: "native",
      note: geometryNote("agent", "native"),
    };
    return { png_base64, meta };
  }

  const out = await sharp(buf)
    .resize(fitted.width, fitted.height, { fit: "fill" })
    .png()
    .toBuffer();

  const meta: ScreenshotMeta = {
    native_width: native.width,
    native_height: native.height,
    image_width: fitted.width,
    image_height: fitted.height,
    layout: "agent",
    click_space: "image",
    note: geometryNote("agent", "image"),
  };
  return { png_base64: out.toString("base64"), meta };
}

/** Per-device last-screenshot geometry for click remapping. */
export class FrameStateStore {
  private frames = new Map<string, FrameGeometry>();

  set(key: string, geo: FrameGeometry): void {
    this.frames.set(key, { ...geo });
  }

  get(key: string): FrameGeometry | undefined {
    return this.frames.get(key);
  }

  /**
   * Remap tool (x,y) to stream pixels using the last screenshot for `key`.
   * Throws {@link NoFrameGeometryError} if no screenshot has been stored yet.
   */
  toStream(
    key: string,
    x: number,
    y: number
  ): {
    stream_x: number;
    stream_y: number;
    click_space: "image" | "native";
    frame: FrameGeometry;
  } {
    const frame = this.frames.get(key);
    if (!frame) {
      throw new NoFrameGeometryError(key);
    }
    const mapped = toStreamCoords(x, y, frame);
    return { ...mapped, frame };
  }
}
