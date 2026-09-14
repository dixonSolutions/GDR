import assert from "node:assert/strict";
import { describe, it } from "node:test";
import type { Frame, Region } from "./gdrClient.js";
import {
  DEFAULT_PROFILE,
  FrameStateStore,
  NoFrameGeometryError,
  OutOfFrameError,
  PATCH,
  SIZING_PROFILES,
  frameMeta,
  isZoom,
  profileForLayout,
  profileRequest,
  toStreamCoords,
  visualTokens,
  type FrameGeometry,
} from "./screenshotLayout.js";

function region(x: number, y: number, width: number, height: number): Region {
  return { x, y, width, height };
}

function fullFrame(
  nativeW: number,
  nativeH: number,
  imageW: number,
  imageH: number
): FrameGeometry {
  return {
    native_width: nativeW,
    native_height: nativeH,
    image_width: imageW,
    image_height: imageH,
    region: region(0, 0, nativeW, nativeH),
  };
}

describe("visualTokens", () => {
  it("matches the published patch formula", () => {
    assert.equal(visualTokens(1092, 1092), 1521);
    assert.equal(visualTokens(200, 200), 64);
    assert.equal(visualTokens(1920, 1080), 2691);
  });

  it("shows why the old 1440x900 box was over budget", () => {
    // The regression this whole sizing change exists to prevent: it looks
    // conservative in pixels but costs 52*33 patches against a 1568 cap,
    // so the model API downscales again and every click is offset.
    assert.equal(visualTokens(1440, 900), 1716);
    assert.ok(visualTokens(1440, 900) > SIZING_PROFILES.claude.maxPatches);
    // 16:9 sources stayed under it purely by luck of aspect ratio.
    assert.ok(visualTokens(1440, 810) <= SIZING_PROFILES.claude.maxPatches);
  });
});

describe("profileRequest", () => {
  it("sends the patch budget for Claude tiers", () => {
    const req = profileRequest("claude");
    assert.equal(req.max_patches, 1568);
    assert.equal(req.patch_size, PATCH);
    assert.equal(req.max_long_edge, 1568);
    assert.equal(req.max_width, undefined);

    assert.equal(profileRequest("claude-hires").max_patches, 4784);
  });

  it("sends a pixel box for OpenAI, which has no patch cap", () => {
    const req = profileRequest("openai");
    assert.deepEqual(
      { w: req.max_width, h: req.max_height, patches: req.max_patches },
      { w: 1440, h: 900, patches: undefined }
    );
  });

  it("sends no constraints at all for raw", () => {
    assert.deepEqual(profileRequest("raw"), {});
  });
});

describe("profileForLayout", () => {
  it("prefers an explicit profile over the coarse layout knob", () => {
    assert.equal(profileForLayout("raw", "claude-hires"), "claude-hires");
  });

  it("maps the legacy layout values", () => {
    assert.equal(profileForLayout("raw", undefined), "raw");
    assert.equal(profileForLayout("agent", undefined), DEFAULT_PROFILE);
    assert.equal(profileForLayout(undefined, undefined), DEFAULT_PROFILE);
  });
});

describe("toStreamCoords", () => {
  it("scales image coords back to native pixels", () => {
    const frame = fullFrame(1920, 1080, 1440, 810);
    assert.deepEqual(toStreamCoords(720, 405, frame), { stream_x: 960, stream_y: 540 });
    assert.deepEqual(toStreamCoords(0, 0, frame), { stream_x: 0, stream_y: 0 });
  });

  it("is identity when the image is already native", () => {
    const frame = fullFrame(1920, 1080, 1920, 1080);
    assert.deepEqual(toStreamCoords(123, 456, frame), { stream_x: 123, stream_y: 456 });
  });

  it("clamps the far edge inside the stream", () => {
    const frame = fullFrame(1920, 1080, 1440, 810);
    const mapped = toStreamCoords(1440, 810, frame);
    assert.ok(mapped.stream_x <= 1919 && mapped.stream_y <= 1079);
  });

  it("folds a zoom's crop origin into the remap", () => {
    // 400x300 crop at +1000,+700, returned 1:1. Clicking the middle of the
    // zoom must land in the middle of that region on the real desktop.
    const zoom: FrameGeometry = {
      native_width: 1920,
      native_height: 1200,
      image_width: 400,
      image_height: 300,
      region: region(1000, 700, 400, 300),
    };
    assert.deepEqual(toStreamCoords(200, 150, zoom), { stream_x: 1200, stream_y: 850 });
    assert.deepEqual(toStreamCoords(0, 0, zoom), { stream_x: 1000, stream_y: 700 });
  });

  it("handles a zoom that was itself downscaled", () => {
    const zoom: FrameGeometry = {
      native_width: 1920,
      native_height: 1200,
      image_width: 200,
      image_height: 150,
      region: region(1000, 700, 400, 300),
    };
    assert.deepEqual(toStreamCoords(100, 75, zoom), { stream_x: 1200, stream_y: 850 });
  });

  it("rejects coordinates outside the image instead of extrapolating", () => {
    // The dangerous case: agent zooms in, then sends full-desktop coords.
    // Extrapolating would click somewhere plausible but wrong.
    const zoom: FrameGeometry = {
      native_width: 1920,
      native_height: 1200,
      image_width: 400,
      image_height: 300,
      region: region(1000, 700, 400, 300),
    };
    assert.throws(() => toStreamCoords(1500, 900, zoom), OutOfFrameError);
    assert.throws(() => toStreamCoords(-10, 5, zoom), OutOfFrameError);
    assert.throws(() => toStreamCoords(1500, 900, zoom), /zoom/);
  });

  it("rejects non-finite coordinates", () => {
    assert.throws(() => toStreamCoords(NaN, 0, fullFrame(100, 100, 100, 100)), /non-finite/);
  });
});

describe("isZoom", () => {
  it("is false for a full-desktop frame at any scale", () => {
    assert.equal(isZoom(fullFrame(1920, 1200, 1344, 840)), false);
  });

  it("is true when the region is a subset", () => {
    assert.equal(
      isZoom({
        native_width: 1920,
        native_height: 1200,
        image_width: 400,
        image_height: 300,
        region: region(10, 10, 400, 300),
      }),
      true
    );
  });
});

describe("frameMeta", () => {
  const base: Frame = {
    data_base64: "",
    format: "jpeg",
    native_width: 1920,
    native_height: 1200,
    region: region(0, 0, 1920, 1200),
    image_width: 1344,
    image_height: 840,
    hash: "abc",
    unchanged: false,
    settled: true,
    capture_ms: 42,
  };

  it("reports the billed token cost, under budget", () => {
    const meta = frameMeta(base, "claude");
    assert.equal(meta.visual_tokens, visualTokens(1344, 840));
    assert.ok(meta.visual_tokens <= SIZING_PROFILES.claude.maxPatches);
    assert.match(meta.note, /re-screenshot/);
    assert.equal(meta.zoom, undefined);
  });

  it("says so when the screen never settled, without crying wolf", () => {
    const meta = frameMeta({ ...base, settled: false }, "claude");
    assert.match(meta.note, /still animating/);
    assert.match(meta.note, /newest frame/);
    // A spinner elsewhere on screen is benign; this must not read as failure.
    assert.doesNotMatch(meta.note, /WARNING|ERROR/);
  });

  it("labels zoom frames and states their coordinate space", () => {
    const meta = frameMeta(
      { ...base, region: region(1000, 700, 400, 300), image_width: 400, image_height: 300 },
      "raw"
    );
    assert.match(meta.note, /^ZOOM/);
    assert.match(meta.note, /full gdr_screenshot/);
    assert.equal(meta.zoom, 1);
  });
});

describe("FrameStateStore", () => {
  it("throws a clear error before any screenshot", () => {
    const store = new FrameStateStore();
    assert.throws(() => store.toStream("local", 10, 10), NoFrameGeometryError);
    assert.throws(() => store.toStream("local", 10, 10), /call gdr_screenshot/);
  });

  it("remaps using the most recent frame for that device", () => {
    const store = new FrameStateStore();
    store.set("local", fullFrame(1920, 1080, 1440, 810));
    assert.deepEqual(
      { ...store.toStream("local", 720, 405), frame: undefined },
      { stream_x: 960, stream_y: 540, frame: undefined }
    );

    // A zoom replaces the geometry; later clicks use the zoom's space.
    store.set("local", {
      native_width: 1920,
      native_height: 1080,
      image_width: 400,
      image_height: 300,
      region: region(100, 200, 400, 300),
    });
    const after = store.toStream("local", 0, 0);
    assert.deepEqual({ x: after.stream_x, y: after.stream_y }, { x: 100, y: 200 });
  });

  it("keeps devices independent", () => {
    const store = new FrameStateStore();
    store.set("a", fullFrame(1920, 1080, 1920, 1080));
    store.set("b", fullFrame(1920, 1080, 960, 540));
    assert.equal(store.toStream("a", 100, 100).stream_x, 100);
    assert.equal(store.toStream("b", 100, 100).stream_x, 200);
  });

  it("does not alias stored geometry with the caller's object", () => {
    const store = new FrameStateStore();
    const geo = fullFrame(1920, 1080, 1440, 810);
    store.set("local", geo);
    geo.region.x = 999;
    assert.equal(store.get("local")!.region.x, 0);
  });

  it("tracks the last content hash per device for unchanged detection", () => {
    const store = new FrameStateStore();
    assert.equal(store.lastHash("local"), undefined);
    store.setHash("local", "deadbeef");
    assert.equal(store.lastHash("local"), "deadbeef");
    assert.equal(store.lastHash("other"), undefined);
  });
});
