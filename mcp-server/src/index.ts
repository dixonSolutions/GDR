#!/usr/bin/env node
/**
 * MCP server for gdr — exposes screenshot / mouse / keyboard tools plus
 * gdr_get_password for retrieving stored host credentials.
 *
 * Connection targets come from ~/.config/gdr/config.json and/or env vars
 * (GDR_HOST, GDR_PORT, GDR_TOKEN, GDR_PIN). Tool arguments may select among
 * *saved* host profiles via `host`, but cannot invent arbitrary IPs.
 */
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";
import { BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, GdrClientPool } from "./gdrClient.js";
import { getPasswordMessage, resolveHost } from "./config.js";

const pool = new GdrClientPool();

function clientFor(host?: string | null) {
  const resolved = resolveHost(host);
  const key = resolved.name ?? `__env__:${resolved.address}:${resolved.port}`;
  return pool.get(key, {
    host: resolved.address,
    port: resolved.port,
    token: resolved.token,
    pinnedFingerprint: resolved.pin,
  });
}

const hostProp = z
  .string()
  .optional()
  .describe(
    "Optional saved host profile name from ~/.config/gdr/config.json. Defaults to default_host."
  );

const server = new McpServer({ name: "gdr", version: "0.1.0" });

server.tool(
  "gdr_screenshot",
  "Take a screenshot of the remote GNOME/Wayland desktop and return it as an image.",
  { host: hostProp },
  async ({ host }) => {
    const client = clientFor(host);
    const resp = await client.request({ type: "Screenshot", connector: null });
    if (resp.type !== "Screenshot") {
      return {
        content: [{ type: "text", text: `error: ${JSON.stringify(resp)}` }],
        isError: true,
      };
    }
    return {
      content: [{ type: "image", data: resp.png_base64, mimeType: "image/png" }],
    };
  }
);

// Back-compat aliases (older docs / prompts may still say gnome_*).
server.tool(
  "gnome_screenshot",
  "Alias for gdr_screenshot.",
  { host: hostProp },
  async ({ host }) => {
    const client = clientFor(host);
    const resp = await client.request({ type: "Screenshot", connector: null });
    if (resp.type !== "Screenshot") {
      return {
        content: [{ type: "text", text: `error: ${JSON.stringify(resp)}` }],
        isError: true,
      };
    }
    return {
      content: [{ type: "image", data: resp.png_base64, mimeType: "image/png" }],
    };
  }
);

server.tool(
  "gdr_click",
  "Move the mouse to (x, y) in screen pixel coordinates and click. Take a screenshot first to decide where.",
  {
    host: hostProp,
    x: z.number().describe("X coordinate in pixels"),
    y: z.number().describe("Y coordinate in pixels"),
    button: z.enum(["left", "right", "middle"]).default("left"),
  },
  async ({ host, x, y, button }) => {
    const client = clientFor(host);
    const code =
      button === "right" ? BTN_RIGHT : button === "middle" ? BTN_MIDDLE : BTN_LEFT;
    const resp = await client.click(x, y, code);
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gnome_click",
  "Alias for gdr_click.",
  {
    host: hostProp,
    x: z.number(),
    y: z.number(),
    button: z.enum(["left", "right", "middle"]).default("left"),
  },
  async ({ host, x, y, button }) => {
    const client = clientFor(host);
    const code =
      button === "right" ? BTN_RIGHT : button === "middle" ? BTN_MIDDLE : BTN_LEFT;
    const resp = await client.click(x, y, code);
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gdr_move",
  "Move the mouse to (x, y) without clicking.",
  { host: hostProp, x: z.number(), y: z.number() },
  async ({ host, x, y }) => {
    const client = clientFor(host);
    const resp = await client.request({ type: "MouseMove", x, y });
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gnome_move",
  "Alias for gdr_move.",
  { host: hostProp, x: z.number(), y: z.number() },
  async ({ host, x, y }) => {
    const client = clientFor(host);
    const resp = await client.request({ type: "MouseMove", x, y });
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gdr_key",
  "Press and release a single key by Linux evdev keycode (not X keysym).",
  { host: hostProp, keycode: z.number().int() },
  async ({ host, keycode }) => {
    const client = clientFor(host);
    await client.request({ type: "KeyEvent", keycode, pressed: true });
    const resp = await client.request({ type: "KeyEvent", keycode, pressed: false });
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gnome_key",
  "Alias for gdr_key.",
  { host: hostProp, keycode: z.number().int() },
  async ({ host, keycode }) => {
    const client = clientFor(host);
    await client.request({ type: "KeyEvent", keycode, pressed: true });
    const resp = await client.request({ type: "KeyEvent", keycode, pressed: false });
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gdr_type",
  "Type a string of text on the remote desktop (ASCII letters/digits/punctuation).",
  { host: hostProp, text: z.string() },
  async ({ host, text }) => {
    const client = clientFor(host);
    const resp = await client.request({ type: "TypeText", text });
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gnome_type",
  "Alias for gdr_type.",
  { host: hostProp, text: z.string() },
  async ({ host, text }) => {
    const client = clientFor(host);
    const resp = await client.request({ type: "TypeText", text });
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gdr_ping",
  "Check that gdrd is reachable and authenticated.",
  { host: hostProp },
  async ({ host }) => {
    const client = clientFor(host);
    const resp = await client.request({ type: "Ping" });
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gnome_ping",
  "Alias for gdr_ping.",
  { host: hostProp },
  async ({ host }) => {
    const client = clientFor(host);
    const resp = await client.request({ type: "Ping" });
    return { content: [{ type: "text", text: JSON.stringify(resp) }] };
  }
);

server.tool(
  "gdr_get_password",
  "Retrieve a stored sudo or user password for a remembered host from ~/.config/gdr/config.json. " +
    "Returns a clear 'not set' message if none was saved. " +
    "SECURITY: the returned plaintext enters the model context / transcript.",
  {
    host: hostProp,
    kind: z.enum(["sudo", "user"]).describe("Which password to retrieve"),
  },
  async ({ host, kind }) => {
    const { ok, message } = getPasswordMessage(kind, host);
    return {
      content: [{ type: "text", text: message }],
      isError: !ok,
    };
  }
);

server.tool(
  "gdr_list_hosts",
  "List remembered host profile names from ~/.config/gdr/config.json (no secrets).",
  {},
  async () => {
    const { loadConfig } = await import("./config.js");
    const cfg = loadConfig();
    const names = Object.keys(cfg.hosts);
    const summary = names.map((n) => {
      const p = cfg.hosts[n];
      return {
        name: n,
        address: `${p.address}:${p.port ?? 7337}`,
        default: cfg.default_host === n,
        has_sudo: Boolean(p.sudo_password),
        has_user: Boolean(p.user_password),
        has_pin: Boolean(p.pin),
      };
    });
    return {
      content: [{ type: "text", text: JSON.stringify({ default_host: cfg.default_host, hosts: summary }, null, 2) }],
    };
  }
);

const transport = new StdioServerTransport();
await server.connect(transport);
console.error("gdr-mcp: connected, waiting for tool calls...");
