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
  devicePublicInfo,
  getPasswordMessage,
  listDevicesPayload,
  removeDevice,
  resolveToolDevice,
  setDefaultDevice,
  upsertDevice,
} from "./config.js";
import { parseServerArgv, getDefaultDevice } from "./cliArgs.js";
import { chord, hotkey, runSequence, tapKey } from "./input.js";
import { resolveKey } from "./keys.js";

parseServerArgv();
if (getDefaultDevice()) {
  console.error(`gdr-mcp: default device = ${getDefaultDevice()}`);
}

const pool = new GdrClientPool();

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

function btn(button?: "left" | "right" | "middle") {
  return button === "right" ? BTN_RIGHT : button === "middle" ? BTN_MIDDLE : BTN_LEFT;
}

function textResult(payload: unknown, isError = false) {
  return {
    content: [{ type: "text" as const, text: JSON.stringify(payload) }],
    isError,
  };
}

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

server.tool(
  "gdr_screenshot",
  "Take a screenshot of the remote GNOME/Wayland desktop and return it as an image.",
  { ...deviceArgs },
  async ({ host, dev }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "Screenshot", connector: null });
    if (resp.type !== "Screenshot") return textResult(resp, true);
    return {
      content: [{ type: "image", data: resp.png_base64, mimeType: "image/png" }],
    };
  }
);

server.tool(
  "gnome_screenshot",
  "Alias for gdr_screenshot.",
  { ...deviceArgs },
  async ({ host, dev }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "Screenshot", connector: null });
    if (resp.type !== "Screenshot") return textResult(resp, true);
    return {
      content: [{ type: "image", data: resp.png_base64, mimeType: "image/png" }],
    };
  }
);

server.tool(
  "gdr_click",
  "Move the mouse to (x, y) and click. Set clicks=2 for double-click.",
  {
    ...deviceArgs,
    x: z.number().describe("X coordinate in pixels"),
    y: z.number().describe("Y coordinate in pixels"),
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
    const client = clientFor(host, dev);
    const resp = await client.multiClick(x, y, btn(button), clicks);
    return textResult({ ...resp, x, y, button, clicks });
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
    const client = clientFor(host, dev);
    const resp = await client.multiClick(x, y, btn(button), clicks);
    return textResult({ ...resp, x, y, button, clicks });
  }
);

server.tool(
  "gdr_double_click",
  "Double-click at (x, y).",
  {
    ...deviceArgs,
    x: z.number(),
    y: z.number(),
    button: z.enum(["left", "right", "middle"]).default("left"),
  },
  async ({ host, dev, x, y, button }) => {
    const client = clientFor(host, dev);
    const resp = await client.multiClick(x, y, btn(button), 2);
    return textResult({ ...resp, x, y, button, clicks: 2 });
  }
);

server.tool(
  "gdr_move",
  "Move the mouse to (x, y) without clicking. Updates the tracked cursor position.",
  { ...deviceArgs, x: z.number(), y: z.number() },
  async ({ host, dev, x, y }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "MouseMove", x, y });
    return textResult({ ...resp, x, y });
  }
);

server.tool(
  "gnome_move",
  "Alias for gdr_move.",
  { ...deviceArgs, x: z.number(), y: z.number() },
  async ({ host, dev, x, y }) => {
    const client = clientFor(host, dev);
    const resp = await client.request({ type: "MouseMove", x, y });
    return textResult({ ...resp, x, y });
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
    'Example: [{hotkey:"Super+PageDown"},{delay_ms:500},{hotkey:"Super"},{type:"lutris"},{tap:"Enter"}]',
  {
    ...deviceArgs,
    steps: z.array(inputStep).min(1).describe("Ordered input steps"),
  },
  async ({ host, dev, steps }) => {
    const client = clientFor(host, dev);
    try {
      const result = await runSequence(client, steps);
      return textResult(result);
    } catch (e) {
      return textResult({ error: e instanceof Error ? e.message : String(e) }, true);
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

const transport = new StdioServerTransport();
await server.connect(transport);
console.error(
  `gdr-mcp: connected${getDefaultDevice() ? ` (default device=${getDefaultDevice()})` : ""}, waiting for tool calls...`
);
