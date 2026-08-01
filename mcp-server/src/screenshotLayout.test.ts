import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { describe, it } from "node:test";
import sharp from "sharp";
import {
  AGENT_MAX_HEIGHT,
  AGENT_MAX_WIDTH,
  applyScreenshotLayout,
  fitInside,
  FrameStateStore,
  NoFrameGeometryError,
  parsePngSize,
  toStreamCoords,
} from "./screenshotLayout.js";

/** Minimal valid-enough PNG for IHDR parsing (CRC not validated by parsePngSize). */
function fakePng(width: number, height: number): Buffer {
  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(25);
  ihdr.writeUInt32BE(13, 0);
  ihdr.write("IHDR", 4, 4, "ascii");
  ihdr.writeUInt32BE(width, 8);
  ihdr.writeUInt32BE(height, 12);
  ihdr[16] = 8; // bit depth
  ihdr[17] = 2; // color type RGB
  ihdr[18] = 0;
  ihdr[19] = 0;
  ihdr[20] = 0;
  ihdr.writeUInt32BE(0, 21); // fake CRC
  return Buffer.concat([sig, ihdr]);
}

describe("parsePngSize", () => {
  it("reads IHDR width/height", () => {
    const size = parsePngSize(fakePng(1920, 1080));
    assert.deepEqual(size, { width: 1920, height: 1080 });
  });

  it("rejects non-PNG", () => {
    assert.throws(
      () => parsePngSize(Buffer.alloc(32, 0x41)),
      /not a PNG/
    );
  });
});

describe("fitInside", () => {
  it("no-ops when already within the box", () => {
    assert.deepEqual(fitInside(1280, 720), { width: 1280, height: 720 });
    assert.deepEqual(fitInside(1440, 900), { width: 1440, height: 900 });
  });

  it("scales 1920×1080 to fit 1440×900", () => {
    const fitted = fitInside(1920, 1080);
    assert.equal(fitted.width, 1440);
    assert.equal(fitted.height, 810);
    assert.ok(fitted.width <= AGENT_MAX_WIDTH);
    assert.ok(fitted.height <= AGENT_MAX_HEIGHT);
  });

  it("scales tall displays by height", () => {
    const fitted = fitInside(1080, 1920);
    assert.equal(fitted.height, 900);
    assert.ok(fitted.width <= AGENT_MAX_WIDTH);
  });
});

describe("toStreamCoords", () => {
  const downscaled = {
    native_width: 1920,
    native_height: 1080,
    image_width: 1440,
    image_height: 810,
    click_space: "image" as const,
  };

  it("scales image space to native (1920×1080 from 1440×810)", () => {
    const mapped = toStreamCoords(720, 405, downscaled);
    assert.equal(mapped.stream_x, 960);
    assert.equal(mapped.stream_y, 540);
    assert.equal(mapped.click_space, "image");
  });

  it("clamps image-edge / off-by-one into native bounds", () => {
    const mapped = toStreamCoords(1440, 810, downscaled);
    assert.equal(mapped.stream_x, 1919);
    assert.equal(mapped.stream_y, 1079);
  });

  it("clamps negative coords to 0", () => {
    const mapped = toStreamCoords(-1, -5, downscaled);
    assert.equal(mapped.stream_x, 0);
    assert.equal(mapped.stream_y, 0);
  });

  it("identity when sizes match, still clamped", () => {
    const frame = {
      native_width: 100,
      native_height: 50,
      image_width: 100,
      image_height: 50,
      click_space: "native" as const,
    };
    assert.deepEqual(toStreamCoords(10, 20, frame), {
      stream_x: 10,
      stream_y: 20,
      click_space: "native",
    });
    assert.deepEqual(toStreamCoords(100, 50, frame), {
      stream_x: 99,
      stream_y: 49,
      click_space: "native",
    });
  });

  it("rejects invalid geometry", () => {
    assert.throws(
      () =>
        toStreamCoords(1, 1, {
          native_width: 0,
          native_height: 1080,
          image_width: 1440,
          image_height: 810,
          click_space: "image",
        }),
      /invalid frame geometry/
    );
  });

  it("round-trips center of agent image", () => {
    const nativeW = 1920;
    const nativeH = 1080;
    const fitted = fitInside(nativeW, nativeH);
    const frame = {
      native_width: nativeW,
      native_height: nativeH,
      image_width: fitted.width,
      image_height: fitted.height,
      click_space: "image" as const,
    };
    const cx = fitted.width / 2;
    const cy = fitted.height / 2;
    const mapped = toStreamCoords(cx, cy, frame);
    assert.ok(Math.abs(mapped.stream_x - nativeW / 2) < 1e-9);
    assert.ok(Math.abs(mapped.stream_y - nativeH / 2) < 1e-9);
  });
});

describe("FrameStateStore", () => {
  it("throws before set; remaps after set", () => {
    const store = new FrameStateStore();
    assert.throws(() => store.toStream("dev", 10, 20), (err: unknown) => {
      assert.ok(err instanceof NoFrameGeometryError);
      assert.equal(err.key, "dev");
      assert.match(err.message, /gdr_screenshot/);
      return true;
    });

    store.set("dev", {
      native_width: 1920,
      native_height: 1080,
      image_width: 1440,
      image_height: 810,
      layout: "agent",
      click_space: "image",
    });
    const after = store.toStream("dev", 720, 405);
    assert.equal(after.stream_x, 960);
    assert.equal(after.stream_y, 540);
    assert.equal(after.click_space, "image");
  });
});

describe("applyScreenshotLayout", () => {
  it("raw passes through bytes and reports native click_space", async () => {
    const png = await sharp({
      create: {
        width: 100,
        height: 80,
        channels: 3,
        background: { r: 10, g: 20, b: 30 },
      },
    })
      .png()
      .toBuffer();
    const b64 = png.toString("base64");
    const applied = await applyScreenshotLayout(b64, "raw");
    assert.equal(applied.png_base64, b64);
    assert.equal(applied.meta.layout, "raw");
    assert.equal(applied.meta.click_space, "native");
    assert.equal(applied.meta.image_width, 100);
    assert.equal(applied.meta.native_width, 100);
  });

  it("agent no-ops under 1440×900", async () => {
    const png = await sharp({
      create: {
        width: 800,
        height: 600,
        channels: 3,
        background: { r: 1, g: 2, b: 3 },
      },
    })
      .png()
      .toBuffer();
    const applied = await applyScreenshotLayout(png.toString("base64"), "agent");
    assert.equal(applied.meta.image_width, 800);
    assert.equal(applied.meta.image_height, 600);
    assert.equal(applied.meta.click_space, "native");
  });

  it("agent downscales 1920×1080 to 1440×810", async () => {
    const png = await sharp({
      create: {
        width: 1920,
        height: 1080,
        channels: 3,
        background: { r: 40, g: 50, b: 60 },
      },
    })
      .png()
      .toBuffer();
    const beforeHash = createHash("sha256").update(png).digest("hex");
    const applied = await applyScreenshotLayout(png.toString("base64"), "agent");
    const out = Buffer.from(applied.png_base64, "base64");
    const afterHash = createHash("sha256").update(out).digest("hex");
    assert.notEqual(beforeHash, afterHash);
    assert.deepEqual(parsePngSize(out), { width: 1440, height: 810 });
    assert.equal(applied.meta.native_width, 1920);
    assert.equal(applied.meta.native_height, 1080);
    assert.equal(applied.meta.image_width, 1440);
    assert.equal(applied.meta.image_height, 810);
    assert.equal(applied.meta.click_space, "image");
    assert.equal(applied.meta.layout, "agent");
    assert.match(applied.meta.note, /re-screenshot/);
  });
});
