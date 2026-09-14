#!/usr/bin/env node
/**
 * MCP server for gdr — exposes screenshot / mouse / keyboard tools plus
 * gdr_get_password for retrieving stored host credentials.
 *
 * Devices (id / label / alias), tokens, and sudo passwords live in
 * ~/.config/gdr/config.json. Select a device with tool args host= or dev=,
 * or bind a default via `gdr-mcp --dev "home computer"` (Cursor mcp.json args).
 * Chat shorthand: `@gdr -dev="home computer"` → pass `dev: "home computer"`.
 */
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, GdrClientPool } from "./gdrClient.js";
import {
  configPath,
  devicePublicInfo,
  getPasswordMessage,
  listDevicesPayload,
  removeDevice,
  resolveToolDevice,
  setDefaultDevice,
  upsertDevice,
} from "./config.js";
import { parseServerArgv, getDefaultDevice } from "./cliArgs.js";
import {
  chord,
  hotkey,
  runSequence,
  runSequenceDetailed,
  scroll,
  tapKey,
  type InputStep,
} from "./input.js";
import { resolveKey } from "./keys.js";
import {
  frameMeta,
  FrameStateStore,
  PROFILE_NAMES,
  profileForLayout,
  profileRequest,
  type LayoutMode,
  type ProfileName,
} from "./screenshotLayout.js";
import { screenLockFields } from "./lockHint.js";
import type { WindowInfo, WindowOp, WindowTarget } from "./gdrClient.js";
import {
  captureBlocker,
  chooseTarget,
  cleanTarget,
  describeTarget,
  isEmptyTarget,
  resolvePin,
  resolveWindow,
  WINDOW_ACTIONS,
  WindowSelectionError,
  windowSummary,
} from "./windows.js";
import {
  appendWindowLog,
  clearPin,
  deviceIdFor,
  getPin,
  pinToTarget,
  readWindowLog,
  setPin,
  windowLogPath,
  type PinnedWindow,
} from "./windowPin.js";

parseServerArgv();
if (getDefaultDevice()) {
  console.error(`gdr-mcp: default device = ${getDefaultDevice()}`);
}

const pool = new GdrClientPool();
const frames = new FrameStateStore();

function deviceKey(host?: string | null, dev?: string | null): string {
  const resolved = resolveToolDevice({ host, dev });
  return resolved.name ?? `__env__:${resolved.address}:${resolved.port}`;
}

function clientFor(host?: string | null, dev?: string | null) {
  const resolved = resolveToolDevice({ host, dev });
  const key = resolved.name ?? `__env__:${resolved.address}:${resolved.port}`;
  return pool.get(key, {
    host: resolved.address,
    port: resolved.port,
    token: resolved.token,
    pinnedFingerprint: resolved.pin,
  });
}

/** Remap image-space coords using the last screenshot for this device. */
function mapPointer(host: string | undefined, dev: string | undefined, x: number, y: number) {
  const key = deviceKey(host, dev);
  const { stream_x, stream_y, frame } = frames.toStream(key, x, y);
  // Echo the space the coordinates were read in, so a mis-scaled click is
  // visible in the transcript rather than something to reverse-engineer.
  return {
    key,
    x,
    y,
    stream_x,
    stream_y,
    click_space: `${frame.image_width}x${frame.image_height}`,
  };
}

function remapInputSteps(
  host: string | undefined,
  dev: string | undefined,
  steps: InputStep[]
): InputStep[] {
  const key = deviceKey(host, dev);
  return steps.map((step) => {
    if ("move" in step) {
      const { stream_x, stream_y } = frames.toStream(key, step.move.x, step.move.y);
      return { move: { x: stream_x, y: stream_y } };
    }
    if ("click" in step) {
      const { stream_x, stream_y } = frames.toStream(key, step.click.x, step.click.y);
      return {
        click: {
          ...step.click,
          x: stream_x,
          y: stream_y,
        },
      };
    }
    if ("scroll" in step && step.scroll.at) {
      const { stream_x, stream_y } = frames.toStream(key, step.scroll.at.x, step.scroll.at.y);
      return { scroll: { ...step.scroll, at: { x: stream_x, y: stream_y } } };
    }
    return step;
  });
}

interface CaptureOpts {
  profile: ProfileName;
  region?: { x: number; y: number; width: number; height: number };
  /** Wait for the screen to stop repainting before grabbing the frame. */
  settle?: boolean;
  quiet_ms?: number;
  timeout_ms?: number;
  /** Skip the image when the screen is byte-identical to the last one. */
  skipUnchanged?: boolean;
}

const DEFAULT_QUIET_MS = 120;
/**
 * Bounded by the Doherty threshold: a capture that blows past ~400 ms stops
 * feeling interactive, and waiting longer does not help the case that actually
 * costs us. Damage is stream-wide, so one blinking terminal cursor anywhere on
 * the desktop keeps the whole screen "busy" forever — measured on a working
 * developer desktop, the frame changed in 11 of 11 samples taken 200 ms apart.
 * Such a screen can never go quiet, so the wait is a pure tax that ends in
 * `settled: false` regardless of how patient we are.
 */
const DEFAULT_SETTLE_TIMEOUT_MS = 400;

/**
 * Capture, store geometry for click remapping, and return an MCP result.
 *
 * gdrd sizes and encodes the image, so nothing is decoded here — the
 * screenshot arrives ready to hand to the model.
 */
async function captureResult(
  host: string | undefined,
  dev: string | undefined,
  opts: CaptureOpts
) {
  const key = deviceKey(host, dev);
  const client = clientFor(host, dev);
  const raw = opts.profile === "raw";
  const frame = await client.captureFrame({
    ...profileRequest(opts.profile),
    region: opts.region ?? null,
    format: raw ? "png" : "jpeg",
    settle: opts.settle
      ? {
          quiet_ms: opts.quiet_ms ?? DEFAULT_QUIET_MS,
          timeout_ms: opts.timeout_ms ?? DEFAULT_SETTLE_TIMEOUT_MS,
        }
      : null,
    if_none_match: opts.skipUnchanged ? (frames.lastHash(key) ?? null) : null,
  });

  const meta = frameMeta(frame, opts.profile);
  frames.set(key, {
    native_width: meta.native_width,
    native_height: meta.native_height,
    image_width: meta.image_width,
    image_height: meta.image_height,
    region: meta.region,
  });
  frames.setHash(key, frame.hash);

  // Identical screen: report it instead of re-sending the same pixels. The
  // agent learns its action had no visible effect, and the message prefix
  // stays byte-identical so prompt caching keeps hitting.
  if (frame.unchanged) {
    return textResult({
      ...meta,
      unchanged: true,
      note:
        "Screen is identical to the previous screenshot for this device — the last " +
        "action had no visible effect. Coordinates from that screenshot are still valid.",
    });
  }

  return {
    content: [
      { type: "text" as const, text: JSON.stringify(meta) },
      {
        type: "image" as const,
        data: frame.data_base64,
        mimeType: frame.format === "jpeg" ? ("image/jpeg" as const) : ("image/png" as const),
      },
    ],
  };
}

function btn(button?: "left" | "right" | "middle") {
  return button === "right" ? BTN_RIGHT : button === "middle" ? BTN_MIDDLE : BTN_LEFT;
}

function textResult(payload: unknown, isError = false) {
  return {
    content: [{ type: "text" as const, text: JSON.stringify(payload) }],
    isError,
  };
}

function mapError(e: unknown) {
  const error = e instanceof Error ? e.message : String(e);
  // A locked screen is not a gdr misconfiguration and not worth a retry —
  // flag it so the agent asks for an unlock instead.
  return textResult({ error, ...screenLockFields(error) }, true);
}

const layoutProp = z
  .enum(["raw", "agent"])
  .default("agent")
  .describe(
    'Coarse sizing: "agent" (default) sizes for the model and returns geometry ' +
      'text; clicks use image pixel space. "raw" keeps native PNG 1:1. ' +
      "Superseded by `profile` — set that instead when you care."
  );

const profileProp = z
  .enum(PROFILE_NAMES)
  .optional()
  .describe(
    "Sizing target. claude (default) fits the ≤1568 visual-token budget; " +
      "claude-hires uses the ≤4784 tier; openai fits 1440×900; raw is native 1:1. " +
      "Sizing over a model's token budget makes its API silently downscale again, " +
      "which offsets every click — leave this alone unless you know the target."
  );

/**
 * Off by default for a plain look at the screen.
 *
 * Settling is only worth paying for when a capture races a transition the
 * caller just started — which is what `gdr_act` handles. On its own,
 * `gdr_screenshot` returns the newest frame either way, so waiting buys at
 * most one frame of freshness (~16 ms) while costing the whole settle budget
 * on any desktop with a blinking cursor. Measured: 9 ms without, ~650 ms with.
 */
const settleProp = z
  .boolean()
  .default(false)
  .describe(
    "Wait for the desktop to stop repainting before capturing (default false). " +
      "Off is the right choice for simply looking at the screen: the newest " +
      "frame is returned regardless. Turn it on when capturing right after an " +
      "action you performed yourself, to avoid catching a half-drawn UI — " +
      "though gdr_act already does this for you."
  );

/**
 * On by default, because here the capture deliberately follows an action and
 * would otherwise be prone to catching a half-drawn window.
 */
const settleAfterActionProp = z
  .boolean()
  .default(true)
  .describe(
    "Wait for the UI to stop repainting after the steps before capturing " +
      "(default true). Uses compositor damage events rather than a fixed sleep."
  );

const skipUnchangedProp = z
  .boolean()
  .default(false)
  .describe(
    "When the screen is identical to the previous capture for this device, " +
      "return a short 'unchanged' note instead of the image. Cheap way to check " +
      "whether an action had any visible effect."
  );

const hostProp = z
  .string()
  .optional()
  .describe(
    "Device id from ~/.config/gdr/config.json (same as `dev`). " +
      "Also matches label/alias (e.g. \"home computer\"). " +
      "Each device has its own token (+ optional sudo). " +
      "Use gdr_list_devices. Defaults: server --dev, then default_host."
  );

const devProp = z
  .string()
  .optional()
  .describe(
    'Preferred device selector — id, label, or alias. Example: "home computer". ' +
      'Chat: @gdr -dev="home computer" → set this field. Overrides host= when both set.'
  );

/** Shared host/dev args for every control tool. */
const deviceArgs = { host: hostProp, dev: devProp };

const keyRef = z
  .union([z.string(), z.number().int()])
  .describe(
    "Key name (Super, Alt, Ctrl, Shift, F4, PageDown, Enter, a–z, 0–9, …) or raw Linux evdev keycode integer."
  );

const inputStep = z.union([
  z.object({ tap: keyRef }).describe("Press and release one key"),
  z.object({ down: keyRef }).describe("Key down (hold)"),
  z.object({ up: keyRef }).describe("Key up (release)"),
  z
    .object({ chord: z.array(keyRef).min(1) })
    .describe("Hold all but last, tap last, release mods — e.g. [\"Super\",\"PageDown\"]"),
  z
    .object({ hotkey: z.string() })
    .describe('Chord string — e.g. "Alt+F4", "Super+PageDown", "Ctrl+Alt+t"'),
  z.object({ type: z.string() }).describe("Type ASCII text"),
  z.object({ delay_ms: z.number().nonnegative() }).describe("Wait before next step"),
  z.object({ move: z.object({ x: z.number(), y: z.number() }) }),
  z
    .object({
      scroll: z.object({
        dx: z.number().optional(),
        dy: z.number().optional().describe("Positive scrolls down, negative up"),
        at: z
          .object({ x: z.number(), y: z.number() })
          .optional()
          .describe("Point at this first, in screenshot image space"),
      }),
    })
    .describe("Wheel scroll, optionally after pointing somewhere"),
  z.object({
    click: z.object({
      x: z.number(),
      y: z.number(),
      button: z.enum(["left", "right", "middle"]).optional(),
      clicks: z.number().int().min(1).max(10).optional(),
    }),
  }),
]);

const server = new McpServer({ name: "gdr", version: "0.3.0" });

const screenshotArgs = {
  ...deviceArgs,
  layout: layoutProp,
  profile: profileProp,
  settle: settleProp,
  skip_unchanged: skipUnchangedProp,
};

type ScreenshotArgs = {
  host?: string;
  dev?: string;
  layout: LayoutMode;
  profile?: ProfileName;
  settle: boolean;
  skip_unchanged: boolean;
};

async function screenshotTool(a: ScreenshotArgs) {
  try {
    return await captureResult(a.host, a.dev, {
      profile: profileForLayout(a.layout, a.profile),
      settle: a.settle,
      skipUnchanged: a.skip_unchanged,
    });
  } catch (e) {
    return mapError(e);
  }
}

server.tool(
  "gdr_screenshot",
  "Screenshot the remote GNOME/Wayland desktop. Returns geometry JSON " +
    "(native/image size, visual_tokens, region) then the image. Pass x,y to " +
    "gdr_click/gdr_move in that image's pixel space. Waits for the screen to " +
    "stop repainting by default, so you rarely need a delay before capturing. " +
    "For anything smaller than ~20px, use gdr_zoom instead of guessing.",
  screenshotArgs,
  screenshotTool
);

server.tool("gnome_screenshot", "Alias for gdr_screenshot.", screenshotArgs, screenshotTool);

server.tool(
  "gdr_zoom",
  "Screenshot a rectangle of the desktop at full native resolution. Use this " +
    "for small targets — tray icons, checkboxes, dropdown arrows, tight menu " +
    "rows — where a full screenshot does not have the pixels to aim reliably. " +
    "x,y,width,height are in the pixel space of the last full gdr_screenshot. " +
    "Afterwards, gdr_click coordinates are read off the ZOOM image; take a " +
    "full gdr_screenshot again before clicking anywhere outside it.",
  {
    ...deviceArgs,
    x: z.number().describe("Left edge, in last screenshot image pixels"),
    y: z.number().describe("Top edge, in last screenshot image pixels"),
    width: z.number().positive().describe("Region width, in image pixels"),
    height: z.number().positive().describe("Region height, in image pixels"),
    settle: settleProp,
  },
  async ({ host, dev, x, y, width, height, settle }) => {
    try {
      const key = deviceKey(host, dev);
      // The region arrives in the previous image's space; convert both
      // corners through the same remap the clicks use, so a zoom taken from
      // a downscaled screenshot lands on the pixels the agent pointed at.
      const topLeft = frames.toStream(key, x, y);
      const bottomRight = frames.toStream(key, x + width, y + height);
      return await captureResult(host, dev, {
        profile: "raw",
        settle,
        region: {
          x: Math.round(topLeft.stream_x),
          y: Math.round(topLeft.stream_y),
          width: Math.max(1, Math.round(bottomRight.stream_x - topLeft.stream_x)),
          height: Math.max(1, Math.round(bottomRight.stream_y - topLeft.stream_y)),
        },
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_click",
  "Move the mouse to (x, y) and click. Coordinates are in the pixel space of the " +
    "most recent gdr_screenshot for this device (image space when layout=agent " +
    "downscaled; native when layout=raw). Requires a prior gdr_screenshot. " +
    "Set clicks=2 for double-click.",
  {
    ...deviceArgs,
    x: z.number().describe("X in latest screenshot image pixels"),
    y: z.number().describe("Y in latest screenshot image pixels"),
    button: z.enum(["left", "right", "middle"]).default("left"),
    clicks: z
      .number()
      .int()
      .min(1)
      .max(10)
      .default(1)
      .describe("1 = single click, 2 = double-click, etc."),
  },
  async ({ host, dev, x, y, button, clicks }) => {
    try {
      const mapped = mapPointer(host, dev, x, y);
      const client = clientFor(host, dev);
      const resp = await client.multiClick(
        mapped.stream_x,
        mapped.stream_y,
        btn(button),
        clicks
      );
      return textResult({
        ...resp,
        x,
        y,
        stream_x: mapped.stream_x,
        stream_y: mapped.stream_y,
        click_space: mapped.click_space,
        button,
        clicks,
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gnome_click",
  "Alias for gdr_click.",
  {
    ...deviceArgs,
    x: z.number(),
    y: z.number(),
    button: z.enum(["left", "right", "middle"]).default("left"),
    clicks: z.number().int().min(1).max(10).default(1),
  },
  async ({ host, dev, x, y, button, clicks }) => {
    try {
      const mapped = mapPointer(host, dev, x, y);
      const client = clientFor(host, dev);
      const resp = await client.multiClick(
        mapped.stream_x,
        mapped.stream_y,
        btn(button),
        clicks
      );
      return textResult({
        ...resp,
        x,
        y,
        stream_x: mapped.stream_x,
        stream_y: mapped.stream_y,
        click_space: mapped.click_space,
        button,
        clicks,
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_double_click",
  "Double-click at (x, y) in the latest screenshot image pixel space.",
  {
    ...deviceArgs,
    x: z.number(),
    y: z.number(),
    button: z.enum(["left", "right", "middle"]).default("left"),
  },
  async ({ host, dev, x, y, button }) => {
    try {
      const mapped = mapPointer(host, dev, x, y);
      const client = clientFor(host, dev);
      const resp = await client.multiClick(mapped.stream_x, mapped.stream_y, btn(button), 2);
      return textResult({
        ...resp,
        x,
        y,
        stream_x: mapped.stream_x,
        stream_y: mapped.stream_y,
        click_space: mapped.click_space,
        button,
        clicks: 2,
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_move",
  "Move the mouse to (x, y) without clicking. Coordinates use the latest " +
    "screenshot image pixel space (same as gdr_click). Updates tracked cursor.",
  { ...deviceArgs, x: z.number(), y: z.number() },
  async ({ host, dev, x, y }) => {
    try {
      const mapped = mapPointer(host, dev, x, y);
      const client = clientFor(host, dev);
      const resp = await client.request({
        type: "MouseMove",
        x: mapped.stream_x,
        y: mapped.stream_y,
      });
      return textResult({
        ...resp,
        x,
        y,
        stream_x: mapped.stream_x,
        stream_y: mapped.stream_y,
        click_space: mapped.click_space,
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_scroll",
  "Scroll the wheel. Positive dy scrolls down, negative up; one notch is " +
    "about 1.0. Pass x,y to point at the pane you mean first — the compositor " +
    "sends wheel events to whatever is under the pointer, so without it you " +
    "may scroll a different window. Coordinates use the latest screenshot " +
    "image pixel space (same as gdr_click).",
  {
    ...deviceArgs,
    dy: z.number().default(3).describe("Vertical notches; positive is down"),
    dx: z.number().default(0).describe("Horizontal notches; positive is right"),
    x: z.number().optional().describe("Point here first (image space)"),
    y: z.number().optional().describe("Point here first (image space)"),
  },
  async ({ host, dev, dx, dy, x, y }) => {
    try {
      const client = clientFor(host, dev);
      let at: { x: number; y: number } | undefined;
      if (x !== undefined && y !== undefined) {
        const mapped = mapPointer(host, dev, x, y);
        at = { x: mapped.stream_x, y: mapped.stream_y };
      }
      const resp = await scroll(client, { dx, dy, at });
      return textResult({ ...resp, dx, dy, pointed_at: at ?? null });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gnome_move",
  "Alias for gdr_move.",
  { ...deviceArgs, x: z.number(), y: z.number() },
  async ({ host, dev, x, y }) => {
    try {
      const mapped = mapPointer(host, dev, x, y);
      const client = clientFor(host, dev);
      const resp = await client.request({
        type: "MouseMove",
        x: mapped.stream_x,
        y: mapped.stream_y,
      });
      return textResult({
        ...resp,
        x,
        y,
        stream_x: mapped.stream_x,
        stream_y: mapped.stream_y,
        click_space: mapped.click_space,
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_cursor",
  "Return the last known absolute cursor position (set by gdr_move / gdr_click). " +
    "Mutter RemoteDesktop cannot query the live OS pointer; known=false until the first move/click from gdr.",
  { ...deviceArgs },
  async ({ host, dev }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "GetCursor" });
    if (resp.type !== "CursorPosition") return textResult(resp, true);
    return textResult({
      x: resp.x,
      y: resp.y,
      known: resp.known,
      note: resp.known
        ? "last position injected by gdr"
        : "no MouseMove yet — call gdr_move or gdr_click first",
    });
  }
);

server.tool(
  "gdr_key",
  "Tap a single key (press+release). Prefer gdr_hotkey / gdr_input for chords and sequences. " +
    "Accepts a key name (Enter, Super, a) or evdev keycode. Optional modifiers are held during the tap.",
  {
    ...deviceArgs,
    key: keyRef.optional().describe("Preferred: key name or keycode"),
    keycode: z.number().int().optional().describe("Legacy: raw evdev keycode"),
    modifiers: z
      .array(keyRef)
      .optional()
      .describe('Modifiers to hold — e.g. ["Super"] with key=PageDown'),
  },
  async ({ host, dev, key, keycode, modifiers }) => {
    const client = clientFor(host, dev);
    const ref = key ?? keycode;
    if (ref === undefined) {
      return textResult({ error: "pass key or keycode" }, true);
    }
    try {
      const mods = modifiers ?? [];
      const resp =
        mods.length === 0 ? await tapKey(client, ref) : await chord(client, mods, ref);
      return textResult({
        ...resp,
        key: typeof ref === "string" ? ref : undefined,
        keycode: resolveKey(ref),
        modifiers: mods,
      });
    } catch (e) {
      return textResult({ error: e instanceof Error ? e.message : String(e) }, true);
    }
  }
);

server.tool(
  "gnome_key",
  "Alias for gdr_key (legacy keycode-only still accepted).",
  { ...deviceArgs, keycode: z.number().int() },
  async ({ host, dev, keycode }) => {
    const client = clientFor(host, dev);
    const resp = await tapKey(client, keycode);
    return textResult(resp);
  }
);

server.tool(
  "gdr_hotkey",
  'Fire a modifier chord in one call. Examples: "Alt+F4", "Super+PageDown", "Ctrl+Alt+t", "Super+Shift+s".',
  {
    ...deviceArgs,
    keys: z
      .string()
      .describe('Hotkey string with + separators, e.g. "Super+PageDown" or "Alt+F4"'),
  },
  async ({ host, dev, keys }) => {
    const client = clientFor(host, dev);
    try {
      const resp = await hotkey(client, keys);
      return textResult({ ...resp, hotkey: keys });
    } catch (e) {
      return textResult({ error: e instanceof Error ? e.message : String(e) }, true);
    }
  }
);

server.tool(
  "gdr_input",
  "Run a flexible ordered sequence of keyboard/mouse steps on one connection. " +
    "Supports tap/down/up, chords, hotkey strings, type, delay_ms, move, and click (with clicks for double-click). " +
    "move/click x,y use the latest screenshot image pixel space (same as gdr_click). " +
    'Example: [{hotkey:"Super+PageDown"},{delay_ms:500},{hotkey:"Super"},{type:"lutris"},{tap:"Enter"}]',
  {
    ...deviceArgs,
    steps: z.array(inputStep).min(1).describe("Ordered input steps"),
  },
  async ({ host, dev, steps }) => {
    const client = clientFor(host, dev);
    try {
      const result = await runSequence(client, remapInputSteps(host, dev, steps));
      return textResult(result);
    } catch (e) {
      return mapError(e);
    }
  }
);

/**
 * Cheap "did anything change?" probe.
 *
 * Captures at a deliberately tiny size so the encode is negligible; we only
 * ever look at the hash. Both probes must use identical parameters, since
 * the hash is salted with them.
 */
const PROBE = { max_width: 320, max_height: 320, format: "jpeg" as const, quality: 60 };

server.tool(
  "gdr_act",
  "Run a sequence of actions and return the resulting screenshot in ONE call. " +
    "This is the preferred way to drive the desktop: it replaces the " +
    "act → screenshot → act → screenshot round trips that dominate task time. " +
    "Steps use the same shapes as gdr_input (tap/down/up/chord/hotkey/type/" +
    "delay_ms/move/click); move and click x,y are in the latest screenshot image " +
    "space. Waits for the UI to settle, then captures. " +
    "Stops at the first failing step and still returns a screenshot of the real " +
    "state, so you can see exactly where things diverged. " +
    "Best for self-contained sequences (form fills, keyboard chains, clicking a " +
    "known target). For exploratory navigation, observe between steps instead.",
  {
    ...deviceArgs,
    steps: z.array(inputStep).min(1).describe("Ordered actions to run before capturing"),
    expect_change: z
      .boolean()
      .default(false)
      .describe(
        "Verify the sequence actually changed the screen, and report an error " +
          "if it did not. Turns a silently missed click into a reported failure."
      ),
    profile: profileProp,
    settle: settleAfterActionProp,
    screenshot: z
      .boolean()
      .default(true)
      .describe("Capture after the steps (default true). Set false for fire-and-forget."),
  },
  async ({ host, dev, steps, expect_change, profile, settle, screenshot }) => {
    try {
      const client = clientFor(host, dev);
      const before = expect_change ? (await client.captureFrame(PROBE)).hash : null;

      const outcome = await runSequenceDetailed(client, remapInputSteps(host, dev, steps));

      let changed: boolean | undefined;
      if (expect_change && before !== null) {
        const after = await client.captureFrame({
          ...PROBE,
          settle: { quiet_ms: DEFAULT_QUIET_MS, timeout_ms: DEFAULT_SETTLE_TIMEOUT_MS },
          if_none_match: before,
        });
        changed = !after.unchanged;
      }

      const failed = !outcome.ok || changed === false;
      const summary = {
        ...outcome,
        ...(changed === undefined ? {} : { changed }),
        ...(changed === false
          ? {
              error:
                outcome.error ??
                "all steps ran but the screen did not change — the action probably missed its target",
            }
          : {}),
      };

      if (!screenshot) return textResult(summary, failed);

      // Always show real state, especially on failure: "step 3 of 7 failed"
      // plus a picture of what actually happened is recoverable; a bare
      // error leaves the agent guessing which half of its plan landed.
      const shot = await captureResult(host, dev, {
        profile: profileForLayout(undefined, profile),
        settle,
      });
      return {
        ...shot,
        content: [{ type: "text" as const, text: JSON.stringify(summary) }, ...shot.content],
        isError: failed,
      };
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_type",
  "Type a string of text on the remote desktop (ASCII letters/digits/punctuation).",
  { ...deviceArgs, text: z.string() },
  async ({ host, dev, text }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "TypeText", text });
    return textResult(resp);
  }
);

server.tool(
  "gnome_type",
  "Alias for gdr_type.",
  { ...deviceArgs, text: z.string() },
  async ({ host, dev, text }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "TypeText", text });
    return textResult(resp);
  }
);

server.tool(
  "gdr_ping",
  "Check that gdrd is reachable and authenticated.",
  { ...deviceArgs },
  async ({ host, dev }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "Ping" });
    return textResult(resp);
  }
);

server.tool(
  "gdr_status",
  "Resolve a device, verify the stored token against gdrd (Ping), and return " +
    "public device metadata (no secrets). Use when the user writes " +
    '`@gdr -dev="…"` before controlling the desktop. Pair with gdr_screenshot ' +
    "to see the live screen.",
  { ...deviceArgs },
  async ({ host, dev }) => {
    const query = dev || host || null;
    let device: Record<string, unknown>;
    try {
      device = devicePublicInfo(query);
    } catch (e) {
      return textResult(
        { ok: false, auth: "unknown", error: String(e) },
        true
      );
    }
    try {
      const client = clientFor(host, dev);
      const resp = await client.request({ type: "Ping" });
      const authOk = resp.type === "Pong";
      return textResult({
        ok: authOk,
        auth: authOk ? "valid" : "failed",
        ping: resp,
        device,
        hint: 'Pass the same dev= on every gdr_* tool. Chat: @gdr -dev="home computer".',
      });
    } catch (e) {
      return textResult(
        {
          ok: false,
          auth: "failed",
          error: String(e),
          device,
          hint: "Token may be wrong/revoked, gdrd down, or pin mismatch.",
        },
        true
      );
    }
  }
);

server.tool(
  "gnome_ping",
  "Alias for gdr_ping.",
  { ...deviceArgs },
  async ({ host, dev }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "Ping" });
    return textResult(resp);
  }
);

server.tool(
  "gdr_get_password",
  "Retrieve a stored sudo or user password for a device from ~/.config/gdr/config.json. " +
    "Returns a clear 'not set' message if none was saved. " +
    "SECURITY: the returned plaintext enters the model context / transcript.",
  {
    ...deviceArgs,
    kind: z.enum(["sudo", "user"]).describe("Which password to retrieve"),
  },
  async ({ host, dev, kind }) => {
    const { ok, message } = getPasswordMessage(kind, dev || host);
    return {
      content: [{ type: "text", text: message }],
      isError: !ok,
    };
  }
);

server.tool(
  "gdr_list_devices",
  "List configured devices (id, label, aliases, endpoint flags — no tokens/passwords). " +
    'Use id/label/alias with tool arg host= or dev=. Chat: @gdr -dev="home computer".',
  {},
  async () => textResult(listDevicesPayload())
);

server.tool(
  "gdr_list_hosts",
  "Alias for gdr_list_devices (legacy name).",
  {},
  async () => textResult(listDevicesPayload())
);

server.tool(
  "gdr_device_add",
  "Add or update a device in ~/.config/gdr/config.json (same store as `gdr device add`). " +
    "Merges with an existing id when present. Secrets are written to disk (chmod 600) " +
    "but never echoed back in the tool result.",
  {
    id: z.string().describe('Canonical id, e.g. "local" or "desktop"'),
    address: z
      .string()
      .optional()
      .describe("Host/IP, or omit when local=true / updating existing"),
    local: z
      .boolean()
      .optional()
      .describe("Same-machine loopback (stores address=localhost)"),
    port: z.number().int().positive().optional(),
    token: z.string().optional().describe("Bearer token (required for new devices)"),
    pin: z.string().optional().describe("TLS cert SHA-256 hex pin"),
    ssh: z.string().optional().describe("SSH admin plane, user@host"),
    label: z.string().optional().describe('Friendly name, e.g. "home computer"'),
    aliases: z.array(z.string()).optional(),
    sudo_password: z.string().optional(),
    user_password: z.string().optional(),
    default: z.boolean().optional().describe("Make this default_host"),
  },
  async (args) => {
    try {
      const result = upsertDevice(args);
      const device = devicePublicInfo(result.id);
      return textResult({
        ok: true,
        created: result.created,
        id: result.id,
        config: result.path,
        device,
      });
    } catch (e) {
      return textResult({ ok: false, error: String(e) }, true);
    }
  }
);

server.tool(
  "gdr_device_remove",
  "Remove a device profile from config (by id, label, or alias).",
  {
    dev: z.string().describe("Device id, label, or alias to remove"),
  },
  async ({ dev }) => {
    try {
      const result = removeDevice(dev);
      return textResult({ ok: true, removed: result.id, config: result.path });
    } catch (e) {
      return textResult({ ok: false, error: String(e) }, true);
    }
  }
);

server.tool(
  "gdr_device_default",
  "Set default_host in config (used when tools omit host=/dev=).",
  {
    dev: z.string().describe("Device id, label, or alias"),
  },
  async ({ dev }) => {
    try {
      const result = setDefaultDevice(dev);
      return textResult({
        ok: true,
        default_host: result.id,
        config: result.path,
        device: devicePublicInfo(result.id),
      });
    } catch (e) {
      return textResult({ ok: false, error: String(e) }, true);
    }
  }
);


// --------------------------------------------------------------------------
// Window plane
//
// Everything below needs the gdr-windows GNOME Shell extension on the target
// (GNOME 50 refuses org.gnome.Shell.Introspect to unprivileged callers). gdrd
// returns a one-line install hint when it is missing, and these tools pass it
// through verbatim rather than inventing their own wording.
// --------------------------------------------------------------------------

const windowSelectorArgs = {
  id: z
    .number()
    .int()
    .optional()
    .describe("Exact window id from gdr_windows. Fastest and unambiguous, but ids die with the window."),
  app_id: z
    .string()
    .optional()
    .describe('Desktop-file id, e.g. "org.gnome.Nautilus" (the .desktop suffix is optional). Exact, case-insensitive.'),
  wm_class: z.string().optional().describe("WM_CLASS substring, case-insensitive."),
  title: z.string().optional().describe("Window title substring, case-insensitive."),
  pid: z.number().int().optional().describe("Process id owning the window."),
  focused: z.boolean().optional().describe("Match whatever currently has keyboard focus."),
};

type SelectorArgs = {
  id?: number;
  app_id?: string;
  wm_class?: string;
  title?: string;
  pid?: number;
  focused?: boolean;
};

function targetFromArgs(a: SelectorArgs): WindowTarget {
  return cleanTarget({
    id: a.id,
    app_id: a.app_id,
    wm_class: a.wm_class,
    title: a.title,
    pid: a.pid,
    focused: a.focused,
  });
}

/**
 * Find the window a call means: explicit selector, else the pin, else focus.
 *
 * Returns the live window plus the full list, because callers almost always
 * need both — the list to report candidates on failure, the window to act on.
 * The pin is resolved through {@link resolvePin} so it survives the app
 * restarting, and a pin whose id went stale is quietly rewritten here rather
 * than making the user re-pin.
 */
async function pickWindow(
  host: string | undefined,
  dev: string | undefined,
  selector: SelectorArgs,
  opts: { allowFocusedFallback?: boolean } = {}
): Promise<{
  window: WindowInfo;
  windows: WindowInfo[];
  list: Awaited<ReturnType<GdrClientType["listWindows"]>>;
  source: "explicit" | "pin" | "focused";
  deviceId: string | null;
}> {
  const client = clientFor(host, dev);
  const list = await client.listWindows(true);

  let deviceId: string | null = null;
  let pin: PinnedWindow | null = null;
  try {
    deviceId = deviceIdFor(dev || host);
    pin = getPin(deviceId);
  } catch {
    // Env-configured device with no config entry: no pin, still usable.
  }

  const explicit = targetFromArgs(selector);
  if (!isEmptyTarget(explicit)) {
    return {
      window: resolveWindow(explicit, list.windows),
      windows: list.windows,
      list,
      source: "explicit",
      deviceId,
    };
  }
  if (pin) {
    const resolved = resolvePin(pinToTarget(pin) as WindowTarget, list.windows);
    if (resolved.stale_id && deviceId) {
      // Self-healing: the app restarted, the durable selector still found it,
      // so refresh the recorded id instead of leaving a pin that takes the
      // slow path (and misleading output) forever after.
      setPin(deviceId, { ...pin, id: resolved.window.id, pinned_at: pin.pinned_at });
    }
    return {
      window: resolved.window,
      windows: list.windows,
      list,
      source: "pin",
      deviceId,
    };
  }
  const chosen = chooseTarget(null, null, opts.allowFocusedFallback ?? true);
  return {
    window: resolveWindow(chosen.target, list.windows),
    windows: list.windows,
    list,
    source: "focused",
    deviceId,
  };
}

type GdrClientType = ReturnType<typeof clientFor>;

/** Turn a selection failure into a result the agent can act on. */
function windowError(e: unknown, windows?: WindowInfo[]) {
  if (e instanceof WindowSelectionError) {
    return textResult(
      {
        error: e.message,
        candidates: (e.candidates.length ? e.candidates : (windows ?? [])).map(
          windowSummary
        ),
      },
      true
    );
  }
  return mapError(e);
}

/**
 * Put a window on screen so a capture can actually see it.
 *
 * Wayland gives gdrd the composited screen, not a per-window buffer, so a
 * minimized or buried window simply is not in the pixels. Activating is
 * therefore part of "view this window", not an optional extra — but it is
 * still visible to the user, so the result always says whether it happened.
 */
async function ensureOnScreen(
  client: GdrClientType,
  window: WindowInfo,
  activate: boolean
): Promise<{ window: WindowInfo; activated: boolean; blocker: string | null }> {
  const blocker = captureBlocker(window);
  if (!blocker) return { window, activated: false, blocker: null };
  if (!activate) return { window, activated: false, blocker };

  await client.windowAction({ id: window.id }, { action: "activate" });
  // Re-list rather than trusting the action's echo: activating can switch
  // workspace and move the window between monitors, and only a fresh listing
  // carries a stream_region computed against the live capture.
  const after = await client.listWindows(true);
  const fresh = after.windows.find((w) => w.id === window.id);
  if (!fresh) {
    return { window, activated: true, blocker: "window disappeared while activating" };
  }
  return { window: fresh, activated: true, blocker: captureBlocker(fresh) };
}

server.tool(
  "gdr_windows",
  "List every window on the remote desktop: id, app, title, geometry, workspace, " +
    "and whether it can be screenshotted right now. Start here — the ids and " +
    "app_ids feed every other gdr_window_* tool. Also reports which monitor gdrd " +
    "is capturing, since windows elsewhere cannot be captured without moving them.",
  {
    ...deviceArgs,
    filter: z
      .string()
      .optional()
      .describe("Case-insensitive substring; matches app_id, wm_class or title."),
    include_skip_taskbar: z
      .boolean()
      .default(false)
      .describe("Include panels, docks and notification popups (normally noise)."),
    log: z
      .boolean()
      .default(false)
      .describe("Append this snapshot to the local window log (see gdr_window_log)."),
  },
  async ({ host, dev, filter, include_skip_taskbar, log }) => {
    try {
      const client = clientFor(host, dev);
      const list = await client.listWindows(include_skip_taskbar);
      const needle = filter?.trim().toLowerCase();
      const windows = needle
        ? list.windows.filter((w) =>
            [w.app_id, w.wm_class, w.title].some((v) =>
              (v ?? "").toLowerCase().includes(needle)
            )
          )
        : list.windows;

      let deviceId: string | null = null;
      let pinned: PinnedWindow | null = null;
      try {
        deviceId = deviceIdFor(dev || host);
        pinned = getPin(deviceId);
      } catch {
        /* env device: no pin store */
      }

      if (log) {
        appendWindowLog(
          windows.map((w) => ({
            ts: new Date().toISOString(),
            device: deviceId,
            kind: "window_event" as const,
            event: "snapshot",
            window: windowSummary(w),
          }))
        );
      }

      return textResult({
        backend: list.backend,
        capture_connector: list.capture_connector,
        active_workspace: list.active_workspace,
        n_workspaces: list.n_workspaces,
        focus_window: list.focus_window,
        seq: list.seq,
        pinned_window: pinned,
        monitors: list.monitors,
        count: windows.length,
        windows: windows.map(windowSummary),
        note:
          "Pass id= (exact) or app_id/title (durable) to the other gdr_window_* tools, " +
          "or pin one with gdr_window_pin to stop repeating the selector. " +
          "`seq` is the starting point for gdr_window_events.",
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_window_info",
  "Resolve one window and report its full state — including which window the " +
    "pin currently points at when no selector is given. Use it to check a pin " +
    "still resolves, or to get a window's geometry before moving it.",
  { ...deviceArgs, ...windowSelectorArgs },
  async ({ host, dev, ...selector }) => {
    try {
      const picked = await pickWindow(host, dev, selector);
      return textResult({
        source: picked.source,
        window: windowSummary(picked.window),
        raw: picked.window,
        capture_blocker: captureBlocker(picked.window),
        capture_connector: picked.list.capture_connector,
      });
    } catch (e) {
      return windowError(e);
    }
  }
);

server.tool(
  "gdr_window_control",
  "Act on a specific window: activate (raise it, unminimize it, switch to its " +
    "workspace), minimize, maximize, move, resize, send to another workspace, or " +
    "close it. With no selector this acts on the pinned window. " +
    "`activate` is the one that makes a window that is not currently on screen " +
    "visible to gdr_screenshot — Wayland streams the composited desktop, so a " +
    "buried window is genuinely not in the pixels.",
  {
    ...deviceArgs,
    ...windowSelectorArgs,
    action: z.enum(WINDOW_ACTIONS).describe("What to do to the window."),
    x: z.number().int().optional().describe("move/move_resize: left edge, logical pixels."),
    y: z.number().int().optional().describe("move/move_resize: top edge, logical pixels."),
    width: z.number().int().optional().describe("resize/move_resize: width, logical pixels."),
    height: z.number().int().optional().describe("resize/move_resize: height, logical pixels."),
    index: z.number().int().optional().describe("workspace: target workspace index (0-based)."),
  },
  async ({ host, dev, action, x, y, width, height, index, ...selector }) => {
    let windows: WindowInfo[] | undefined;
    try {
      const picked = await pickWindow(host, dev, selector);
      windows = picked.windows;

      let op: WindowOp;
      switch (action) {
        case "move":
          if (x == null || y == null) throw new Error("move needs x and y");
          op = { action, x, y };
          break;
        case "resize":
          if (width == null || height == null) {
            throw new Error("resize needs width and height");
          }
          op = { action, width, height };
          break;
        case "move_resize":
          if (x == null || y == null || width == null || height == null) {
            throw new Error("move_resize needs x, y, width and height");
          }
          op = { action, x, y, width, height };
          break;
        case "workspace":
          if (index == null) throw new Error("workspace needs index");
          op = { action, index };
          break;
        default:
          op = { action } as WindowOp;
      }

      const client = clientFor(host, dev);
      const result = await client.windowAction({ id: picked.window.id }, op);
      appendWindowLog([
        {
          ts: new Date().toISOString(),
          device: picked.deviceId,
          kind: "window_action",
          action,
          selector: describeTarget(targetFromArgs(selector)),
          source: picked.source,
          window: windowSummary(picked.window),
        },
      ]);
      return textResult({
        ok: true,
        action: result.action,
        source: picked.source,
        window: result.window ? windowSummary(result.window) : null,
        was: windowSummary(picked.window),
        note:
          action === "activate"
            ? "The window is now on screen; gdr_screenshot or gdr_window_screenshot will show it."
            : undefined,
      });
    } catch (e) {
      return windowError(e, windows);
    }
  }
);

server.tool(
  "gdr_window_screenshot",
  "Screenshot ONE window instead of the whole desktop — fewer visual tokens and " +
    "no surrounding clutter. Activates the window first by default, because a " +
    "minimized or buried window is not present in the compositor's stream at all. " +
    "Afterwards gdr_click / gdr_act coordinates are read off THIS image; take a " +
    "full gdr_screenshot before clicking anything outside the window.",
  {
    ...deviceArgs,
    ...windowSelectorArgs,
    activate: z
      .boolean()
      .default(true)
      .describe(
        "Raise and unminimize the window first (default true). Set false to " +
          "capture without disturbing what the user is doing — but a window " +
          "that is not on screen then cannot be captured at all."
      ),
    profile: profileProp,
    settle: settleProp,
  },
  async ({ host, dev, activate, profile, settle, ...selector }) => {
    let windows: WindowInfo[] | undefined;
    try {
      const picked = await pickWindow(host, dev, selector);
      windows = picked.windows;
      const client = clientFor(host, dev);
      const ready = await ensureOnScreen(client, picked.window, activate);

      if (ready.blocker || !ready.window.stream_region) {
        return textResult(
          {
            error: `cannot capture this window: ${ready.blocker ?? "no capture geometry"}`,
            window: windowSummary(ready.window),
            hint: activate
              ? "Activating did not bring it onto the captured monitor. Move it there with " +
                "gdr_window_control action=move, or capture the whole desktop."
              : "Retry with activate=true.",
          },
          true
        );
      }

      const shot = await captureResult(host, dev, {
        profile: profileForLayout(undefined, profile),
        // Settling matters more here than for a plain screenshot: we may have
        // just triggered a raise/workspace animation ourselves.
        settle: settle || ready.activated,
        region: ready.window.stream_region,
      });
      return {
        ...shot,
        content: [
          {
            type: "text" as const,
            text: JSON.stringify({
              window: windowSummary(ready.window),
              source: picked.source,
              activated: ready.activated,
            }),
          },
          ...shot.content,
        ],
      };
    } catch (e) {
      return windowError(e, windows);
    }
  }
);

server.tool(
  "gdr_window_act",
  "Activate a specific window, run a sequence of input steps, and return a " +
    "screenshot of that window — the window-scoped counterpart to gdr_act, in " +
    "one round trip. Activating first is the point: it guarantees the keystrokes " +
    "and clicks land in the window you named rather than whatever happened to " +
    "have focus. move/click x,y are in the image space of the most recent " +
    "screenshot for this device (usually the previous gdr_window_screenshot).",
  {
    ...deviceArgs,
    ...windowSelectorArgs,
    steps: z.array(inputStep).min(1).describe("Ordered actions to run in the window"),
    activate: z
      .boolean()
      .default(true)
      .describe("Raise the window before running the steps (default true)."),
    screenshot: z
      .boolean()
      .default(true)
      .describe("Capture the window afterwards (default true)."),
    profile: profileProp,
    settle: settleAfterActionProp,
  },
  async ({ host, dev, steps, activate, screenshot, profile, settle, ...selector }) => {
    let windows: WindowInfo[] | undefined;
    try {
      const picked = await pickWindow(host, dev, selector);
      windows = picked.windows;
      const client = clientFor(host, dev);
      const ready = await ensureOnScreen(client, picked.window, activate);

      const outcome = await runSequenceDetailed(
        client,
        remapInputSteps(host, dev, steps)
      );
      const summary = {
        ...outcome,
        window: windowSummary(ready.window),
        source: picked.source,
        activated: ready.activated,
      };
      if (!screenshot || !ready.window.stream_region) {
        return textResult(
          ready.window.stream_region
            ? summary
            : { ...summary, note: `no window screenshot: ${ready.blocker}` },
          !outcome.ok
        );
      }

      // Re-read geometry: the steps may have moved or resized the window,
      // and cropping to where it *was* would show the wrong pixels.
      const after = await client.listWindows(true);
      const fresh = after.windows.find((w) => w.id === ready.window.id) ?? ready.window;
      const region = fresh.stream_region ?? ready.window.stream_region;

      const shot = await captureResult(host, dev, {
        profile: profileForLayout(undefined, profile),
        settle,
        region,
      });
      return {
        ...shot,
        content: [
          { type: "text" as const, text: JSON.stringify(summary) },
          ...shot.content,
        ],
        isError: !outcome.ok,
      };
    } catch (e) {
      return windowError(e, windows);
    }
  }
);

server.tool(
  "gdr_window_events",
  "Poll for windows opening, closing, gaining focus, or being (un)minimized. " +
    "Pass the previous call's next_seq as since= to continue without gaps; " +
    "start from the seq in gdr_windows. With wait_ms > 0 the call blocks until " +
    "something happens, so watching costs one call rather than a spin loop.",
  {
    ...deviceArgs,
    since: z
      .number()
      .int()
      .default(0)
      .describe("Resume after this sequence number. 0 returns the whole buffer."),
    limit: z.number().int().min(1).max(512).default(100),
    wait_ms: z
      .number()
      .int()
      .min(0)
      .max(30000)
      .default(0)
      .describe(
        "Block up to this many ms for the first event. 0 returns immediately. " +
          "Note this occupies the connection to this device for the duration."
      ),
    log: z
      .boolean()
      .default(true)
      .describe("Append the events to the local window log (default true)."),
  },
  async ({ host, dev, since, limit, wait_ms, log }) => {
    try {
      const client = clientFor(host, dev);
      const resp = await client.windowEvents(since, limit, wait_ms);
      let deviceId: string | null = null;
      try {
        deviceId = deviceIdFor(dev || host);
      } catch {
        /* env device */
      }
      if (log && resp.events.length) {
        appendWindowLog(
          resp.events.map((e) => ({
            ts: e.at,
            device: deviceId,
            kind: "window_event" as const,
            event: e.kind,
            seq: e.seq,
            id: e.id,
            app_id: e.app_id,
            wm_class: e.wm_class,
            title: e.title,
          }))
        );
      }
      return textResult({
        count: resp.events.length,
        next_seq: resp.next_seq,
        dropped: resp.dropped,
        reset: resp.reset,
        events: resp.events,
        note: resp.reset
          ? "The compositor or the extension restarted, so sequence numbers began again — " +
            "everything still buffered is above, but events from before the restart are gone."
          : resp.dropped
            ? "Polled too late: older events aged out of the ring buffer. Poll more often " +
              "or use wait_ms to block."
            : "Pass next_seq back as since= on the next call.",
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_window_pin",
  "Pin a window as the default target for every gdr_window_* tool on this " +
    "device, so you stop repeating the same selector (and stop risking a typo " +
    "acting on the wrong window). The pin is stored per device in " +
    "~/.config/gdr/config.json, survives restarts, and records both the window " +
    "id and its app/title — so it keeps working after the app is restarted. " +
    "Call with no selector to show the current pin, or clear=true to wipe it. " +
    "An explicit selector on any tool always overrides the pin.",
  {
    ...deviceArgs,
    ...windowSelectorArgs,
    clear: z.boolean().default(false).describe("Wipe the pin for this device."),
    label: z.string().optional().describe("Friendly name for logs and output."),
    note: z.string().optional().describe("Why this window is pinned."),
  },
  async ({ host, dev, clear, label, note, ...selector }) => {
    try {
      const deviceId = deviceIdFor(dev || host);

      if (clear) {
        const { path, previous } = clearPin(deviceId);
        return textResult({
          ok: true,
          device: deviceId,
          pinned_window: null,
          previous,
          config: path,
          log: windowLogPath(),
        });
      }

      const explicit = targetFromArgs(selector);
      const existing = getPin(deviceId);

      // No selector: report, don't guess. Pinning "whatever is focused right
      // now" from a bare call would be a surprising side effect of asking
      // what the pin is.
      if (isEmptyTarget(explicit)) {
        if (!existing) {
          return textResult({
            ok: true,
            device: deviceId,
            pinned_window: null,
            note:
              "No window pinned for this device. Pin one by passing id=, app_id= or title=.",
            log: windowLogPath(),
          });
        }
        let resolved: unknown = null;
        let error: string | null = null;
        try {
          const client = clientFor(host, dev);
          const list = await client.listWindows(true);
          const hit = resolvePin(pinToTarget(existing) as WindowTarget, list.windows);
          resolved = { ...windowSummary(hit.window), matched: hit.matched };
        } catch (e) {
          error = e instanceof Error ? e.message : String(e);
        }
        return textResult({
          ok: error == null,
          device: deviceId,
          pinned_window: existing,
          resolves_to: resolved,
          error,
          config: configPath(),
          log: windowLogPath(),
        });
      }

      // Resolve before storing: a pin that does not currently name exactly one
      // window is a bug the user wants to hear about now, not on the next
      // action against the wrong window.
      const client = clientFor(host, dev);
      const list = await client.listWindows(true);
      const window = resolveWindow(explicit, list.windows);

      const pin: PinnedWindow = {
        ...cleanTarget({
          id: window.id,
          // Store the durable identity alongside the id, so the pin outlives
          // the window: ids are recycled the moment the app restarts.
          app_id: explicit.app_id ?? window.app_id ?? undefined,
          wm_class: explicit.app_id ? undefined : (explicit.wm_class ?? window.wm_class ?? undefined),
          title: explicit.title ?? undefined,
        }),
        label: label ?? window.title ?? window.app_id ?? null,
        note: note ?? null,
        pinned_at: new Date().toISOString(),
        pinned_to: {
          id: window.id,
          title: window.title,
          app_id: window.app_id,
          wm_class: window.wm_class,
        },
      };
      const stored = setPin(deviceId, pin);
      return textResult({
        ok: true,
        device: deviceId,
        pinned_window: stored.pin,
        resolves_to: windowSummary(window),
        config: stored.path,
        log: windowLogPath(),
        note:
          "Every gdr_window_* tool now defaults to this window. Pass a selector to " +
          "override once, or gdr_window_pin({clear:true}) to wipe it.",
      });
    } catch (e) {
      return windowError(e);
    }
  }
);

server.tool(
  "gdr_window_log",
  "Read the local window log: pin changes, window actions you performed, and " +
    "window open/close events recorded by gdr_window_events. Answers 'what was " +
    "open at 14:30' and 'what was I pointed at' after the windows are gone.",
  {
    ...deviceArgs,
    tail: z.number().int().min(1).max(1000).default(50).describe("Most recent N entries."),
    kind: z
      .enum(["pin_set", "pin_cleared", "window_event", "window_action"])
      .optional()
      .describe("Only entries of this kind."),
    since: z.string().optional().describe("ISO-8601 timestamp; only entries at or after it."),
    all_devices: z
      .boolean()
      .default(false)
      .describe("Include other devices' entries (default: just this device)."),
  },
  async ({ host, dev, tail, kind, since, all_devices }) => {
    try {
      let device: string | null = null;
      if (!all_devices) {
        try {
          device = deviceIdFor(dev || host);
        } catch {
          /* env device: fall back to everything */
        }
      }
      const result = readWindowLog({ tail, kind, since, device });
      return textResult({
        path: result.path,
        device,
        matched: result.total,
        returned: result.entries.length,
        entries: result.entries,
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

server.tool(
  "gdr_app_launch",
  "Start an installed app, or raise it if it is already running — the way to " +
    "reach a window that is not currently open at all. Pass list=true with a " +
    "filter to discover app ids first. On GNOME an app id is its desktop-file " +
    'id, e.g. "org.gnome.Nautilus" or "firefox".',
  {
    ...deviceArgs,
    app_id: z.string().optional().describe("Desktop-file id to launch or raise."),
    list: z
      .boolean()
      .default(false)
      .describe("List installed apps matching `filter` instead of launching."),
    filter: z.string().optional().describe("Substring of app id or name, for list=true."),
  },
  async ({ host, dev, app_id, list, filter }) => {
    try {
      const client = clientFor(host, dev);
      if (list || !app_id) {
        const apps = await client.listApps(filter ?? null);
        return textResult({
          count: apps.length,
          filter: filter ?? null,
          apps: apps.slice(0, 200),
          note: app_id
            ? undefined
            : "Pass app_id= to launch one. Running apps are raised rather than started twice.",
        });
      }
      const resp = await client.launchApp(app_id);
      return textResult({
        ok: true,
        app_id: resp.app_id,
        name: resp.name,
        was_running: resp.was_running,
        action: resp.was_running ? "activated existing window" : "launched",
        note: resp.was_running
          ? "An existing window was raised; it is now on screen."
          : "The app was started. Give it a moment, then gdr_windows to find its window " +
            "(or gdr_window_events with wait_ms to be told when it appears).",
      });
    } catch (e) {
      return mapError(e);
    }
  }
);

const transport = new StdioServerTransport();
await server.connect(transport);
console.error(
  `gdr-mcp: connected${getDefaultDevice() ? ` (default device=${getDefaultDevice()})` : ""}, waiting for tool calls...`
);
