/**
 * Screenshot sizing policy and click-coordinate remapping.
 *
 * gdrd crops, resizes and encodes in a single pass from the raw PipeWire
 * buffer, so this module no longer touches pixels. Its job is to decide
 * *what to ask for* (the sizing profile) and to map coordinates the agent
 * read off an image back to Mutter stream pixels.
 *
 * The one invariant that matters: **agents always give coordinates in the
 * space of the most recent image they were shown for that device.** Full
 * screenshot or zoom, the rule does not change; the crop origin and scale
 * are folded into the remap here. A second coordinate system would be the
 * fastest way to reintroduce the drift this file exists to prevent.
 */

import type { Frame, Region } from "./gdrClient.js";

export type LayoutMode = "raw" | "agent";

/**
 * Vision models bill images in 28×28 patches: an image costs
 * `ceil(w/28) * ceil(h/28)` visual tokens. Past the per-tier cap the model
 * API **silently downscales server-side**, and coordinates then come back
 * in a space we never saw — systematic click drift.
 *
 * Sizing in pixels cannot see that cliff. The previous 1440×900 fit box
 * looked conservative but cost 52×33 = 1716 tokens against a 1568 cap on a
 * 16:10 panel; 16:9 sources landed at 1440×810 = 1508 and stayed under it
 * purely by luck of aspect ratio, which is why this only broke on some
 * machines. The budget is now expressed in patches and applied by gdrd,
 * which is the only place the native aspect ratio is known.
 */
export const PATCH = 28;

export interface SizingProfile {
  /** Patch budget, or null when the target has no such cap. */
  maxPatches: number | null;
  /** Long-edge cap in pixels, or null. */
  maxLongEdge: number | null;
  /** Explicit pixel box, for targets that specify one. */
  maxWidth?: number;
  maxHeight?: number;
  describe: string;
}

/**
 * Per-call because the vendors genuinely disagree: Anthropic enforces a
 * patch budget, OpenAI removed its resize ceiling and asks for fidelity.
 */
export const SIZING_PROFILES = {
  claude: {
    maxPatches: 1568,
    maxLongEdge: 1568,
    describe: "Claude standard tier (≤1568 visual tokens)",
  },
  "claude-hires": {
    maxPatches: 4784,
    maxLongEdge: 2576,
    describe: "Claude 4.7+ high-resolution tier (≤4784 visual tokens)",
  },
  openai: {
    maxPatches: null,
    maxLongEdge: null,
    maxWidth: 1440,
    maxHeight: 900,
    describe: "OpenAI computer-use viewport (1440×900, no patch cap)",
  },
  /**
   * For agents whose image viewer caps images at 1024px on the long edge.
   *
   * Such a viewer silently resizes anything larger, so the picture the agent
   * measures against stops matching the click space it was told to use — the
   * same silent-downscale trap as an over-budget capture, arriving from the
   * client side instead. Staying at or under 1024 means what the agent sees is
   * exactly what it clicks.
   */
  compact: {
    maxPatches: null,
    maxLongEdge: 1024,
    describe: "≤1024px long edge, for viewers that resize anything larger",
  },
  raw: {
    maxPatches: null,
    maxLongEdge: null,
    describe: "native resolution, 1:1",
  },
} as const satisfies Record<string, SizingProfile>;

export type ProfileName = keyof typeof SIZING_PROFILES;
export const DEFAULT_PROFILE: ProfileName = "claude";
export const PROFILE_NAMES = Object.keys(SIZING_PROFILES) as [ProfileName, ...ProfileName[]];

/** Visual-token cost of an image, as billed. */
export function visualTokens(width: number, height: number): number {
  return Math.ceil(width / PATCH) * Math.ceil(height / PATCH);
}

/** Sizing constraints to send with a `CaptureFrame` request. */
export function profileRequest(profile: ProfileName = DEFAULT_PROFILE): {
  max_width?: number;
  max_height?: number;
  max_long_edge?: number;
  max_patches?: number;
  patch_size?: number;
} {
  const p: SizingProfile = SIZING_PROFILES[profile];
  const out: ReturnType<typeof profileRequest> = {};
  if (p.maxWidth) out.max_width = p.maxWidth;
  if (p.maxHeight) out.max_height = p.maxHeight;
  if (p.maxLongEdge) out.max_long_edge = p.maxLongEdge;
  if (p.maxPatches) {
    out.max_patches = p.maxPatches;
    out.patch_size = PATCH;
  }
  return out;
}

/** `layout` is the older, coarser knob; map it onto a profile. */
export function profileForLayout(
  layout: LayoutMode | undefined,
  profile: ProfileName | undefined
): ProfileName {
  if (profile) return profile;
  return layout === "raw" ? "raw" : DEFAULT_PROFILE;
}

export interface FrameGeometry {
  native_width: number;
  native_height: number;
  image_width: number;
  image_height: number;
  /** Portion of the desktop this image shows, in native pixels. */
  region: Region;
}

/** Thrown when click/move remapping runs before any screenshot for that device. */
export class NoFrameGeometryError extends Error {
  readonly key: string;

  constructor(key: string) {
    super(
      `no screenshot geometry for device "${key}": these coordinates are in the ` +
        `last screenshot's image space, and no screenshot has been taken. Either call ` +
        `gdr_screenshot first, or — if you are following coordinates from gdr_windows ` +
        `or gdr_hook_events, which report capture-stream pixels — pass space:"stream" ` +
        `to gdr_zoom, which needs no prior screenshot.`
    );
    this.name = "NoFrameGeometryError";
    this.key = key;
  }
}

/** Thrown when coordinates fall outside the image they claim to come from. */
export class OutOfFrameError extends Error {
  constructor(x: number, y: number, frame: FrameGeometry) {
    super(
      `(${x}, ${y}) is outside the last image for this device ` +
        `(${frame.image_width}×${frame.image_height}). ` +
        (isZoom(frame)
          ? "That image was a zoom into " +
            `${frame.region.width}×${frame.region.height}+${frame.region.x},${frame.region.y}; ` +
            "coordinates must be read off the zoom, or take a full gdr_screenshot first."
          : "Take a fresh gdr_screenshot and re-read the coordinates.")
    );
    this.name = "OutOfFrameError";
  }
}

export function isZoom(frame: FrameGeometry): boolean {
  return (
    frame.region.x !== 0 ||
    frame.region.y !== 0 ||
    frame.region.width !== frame.native_width ||
    frame.region.height !== frame.native_height
  );
}

/**
 * Map native stream pixels back into the last screenshot's image space.
 *
 * The inverse of {@link toStreamCoords}, and the reason hook reports are
 * clickable: gdrd measures screen activity in stream pixels, while
 * `gdr_click` takes coordinates in the space of the image the agent actually
 * looked at. Returns `null` rather than a clamped guess when the point falls
 * outside the frame — a circle on a part of the desktop this screenshot does
 * not show cannot be pointed at with these coordinates, and saying so is the
 * only honest answer.
 */
export function toImageCoords(
  streamX: number,
  streamY: number,
  frame: FrameGeometry
): { x: number; y: number } | null {
  if (
    frame.native_width <= 0 ||
    frame.native_height <= 0 ||
    frame.image_width <= 0 ||
    frame.image_height <= 0
  ) {
    return null;
  }
  if (!Number.isFinite(streamX) || !Number.isFinite(streamY)) return null;
  const dx = streamX - frame.region.x;
  const dy = streamY - frame.region.y;
  if (dx < 0 || dy < 0 || dx > frame.region.width || dy > frame.region.height) {
    return null;
  }
  const scaleX = frame.image_width / frame.region.width;
  const scaleY = frame.image_height / frame.region.height;
  return {
    x: Math.round(dx * scaleX * 10) / 10,
    y: Math.round(dy * scaleY * 10) / 10,
  };
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
 *
 * Composes the crop origin with the downscale factor, so full screenshots
 * and zooms use the same rule. Coordinates outside the image are rejected
 * rather than extrapolated: an agent passing full-desktop coordinates
 * against a zoom frame is a real and silent failure mode, and guessing
 * would put the click somewhere plausible but wrong.
 */
export function toStreamCoords(
  x: number,
  y: number,
  frame: FrameGeometry
): { stream_x: number; stream_y: number } {
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
  if (!Number.isFinite(x) || !Number.isFinite(y)) {
    throw new Error(`non-finite coordinates (${x}, ${y})`);
  }
  // Half a pixel of slack for detectors that report an exclusive edge.
  if (x < -0.5 || y < -0.5 || x > frame.image_width + 0.5 || y > frame.image_height + 0.5) {
    throw new OutOfFrameError(x, y, frame);
  }

  const scaleX = frame.region.width / frame.image_width;
  const scaleY = frame.region.height / frame.image_height;
  return {
    stream_x: clampStreamAxis(frame.region.x + x * scaleX, frame.native_width),
    stream_y: clampStreamAxis(frame.region.y + y * scaleY, frame.native_height),
  };
}

export interface ScreenshotMeta extends FrameGeometry {
  profile: ProfileName;
  format: string;
  visual_tokens: number;
  /** False when the screen never went quiet before the settle timeout. */
  settled: boolean;
  capture_ms: number;
  zoom?: number;
  note: string;
}

function note(frame: FrameGeometry, settled: boolean): string {
  const base = isZoom(frame)
    ? `ZOOM: this image shows ${frame.region.width}×${frame.region.height} of the desktop at ` +
      `+${frame.region.x},${frame.region.y}. gdr_click/gdr_move x,y are in this zoom's ` +
      `${frame.image_width}×${frame.image_height} space — to click elsewhere, take a full gdr_screenshot first`
    : `gdr_click/gdr_move x,y are in image_width×image_height for this device ` +
      `until the next gdr_screenshot (re-screenshot after resize/workspace/monitor change)`;
  if (settled) return base;
  // Fires for benign reasons too — a spinner or video anywhere on the
  // desktop keeps the whole stream busy, since damage is not tracked per
  // region. Stated as information, not an alarm: the frame is still the
  // newest one available, and treating this as an error would train agents
  // to ignore it.
  return (
    `${base}. Note: something on screen was still animating, so this is the ` +
    `newest frame rather than a settled one — re-capture if it looks mid-transition`
  );
}

/** Build the geometry + metadata an agent needs from a daemon frame. */
export function frameMeta(frame: Frame, profile: ProfileName): ScreenshotMeta {
  const geo: FrameGeometry = {
    native_width: frame.native_width,
    native_height: frame.native_height,
    image_width: frame.image_width,
    image_height: frame.image_height,
    region: frame.region,
  };
  const meta: ScreenshotMeta = {
    ...geo,
    profile,
    format: frame.format,
    visual_tokens: visualTokens(frame.image_width, frame.image_height),
    settled: frame.settled,
    capture_ms: frame.capture_ms,
    note: note(geo, frame.settled),
  };
  if (isZoom(geo)) {
    meta.zoom =
      Math.round((frame.image_width / Math.max(1, frame.region.width)) * 100) / 100;
  }
  return meta;
}

/** Per-device last-frame geometry for click remapping. */
export class FrameStateStore {
  private frames = new Map<string, FrameGeometry>();
  private hashes = new Map<string, string>();

  set(key: string, geo: FrameGeometry): void {
    this.frames.set(key, { ...geo, region: { ...geo.region } });
  }

  get(key: string): FrameGeometry | undefined {
    return this.frames.get(key);
  }

  /** Last content hash seen for this device, for `if_none_match`. */
  lastHash(key: string): string | undefined {
    return this.hashes.get(key);
  }

  setHash(key: string, hash: string): void {
    this.hashes.set(key, hash);
  }

  /**
   * Remap tool (x,y) to stream pixels using the last frame for `key`.
   * Throws {@link NoFrameGeometryError} if no screenshot has been taken yet.
   */
  toStream(
    key: string,
    x: number,
    y: number
  ): { stream_x: number; stream_y: number; frame: FrameGeometry } {
    const frame = this.frames.get(key);
    if (!frame) {
      throw new NoFrameGeometryError(key);
    }
    return { ...toStreamCoords(x, y, frame), frame };
  }
}
