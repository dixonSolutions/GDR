#!/usr/bin/env node
// Agent-facing CLI for the GDR benchmark.
//
// Thin on purpose: it forwards to the harness, which owns the one long-lived
// MCP connection and all the bookkeeping. Spawning a fresh MCP server per
// command would add a few hundred milliseconds of startup to every measured
// call and make the latency numbers meaningless.
//
// Screenshots are written to disk and the path is printed. Agents read the
// image from that path, which keeps large base64 blobs out of the transcript.
//
// Usage:
//   node bench/gdrb.mjs shot [--profile claude|claude-hires|openai|raw]
//   node bench/gdrb.mjs zoom <x> <y> <w> <h>
//   node bench/gdrb.mjs click <x> <y> [--button left|right] [--clicks 2]
//   node bench/gdrb.mjs move <x> <y>
//   node bench/gdrb.mjs scroll <dy> [--at <x> <y>]
//   node bench/gdrb.mjs key <Key>            e.g. Return, Escape, Page_Down
//   node bench/gdrb.mjs hotkey <Combo>       e.g. Ctrl+a
//   node bench/gdrb.mjs type <text...>
//   node bench/gdrb.mjs act '<json steps>'   e.g. '[{"tap":"Page_Down"}]'
//   node bench/gdrb.mjs status

const BASE = process.env.GDR_BENCH_URL ?? "http://127.0.0.1:7788";

const argv = process.argv.slice(2);
const cmd = argv[0];

/** Read `--flag value`, removing it so positionals stay clean. */
function flag(name, fallback) {
  const i = argv.indexOf(`--${name}`);
  if (i < 0) return fallback;
  const value = argv[i + 1];
  argv.splice(i, 2);
  return value;
}

function has(name) {
  const i = argv.indexOf(`--${name}`);
  if (i < 0) return false;
  argv.splice(i, 1);
  return true;
}

const num = (v, what) => {
  const n = Number(v);
  if (!Number.isFinite(n)) fail(`expected a number for ${what}, got ${JSON.stringify(v)}`);
  return n;
};

function fail(msg) {
  console.error(`gdrb: ${msg}`);
  process.exit(2);
}

async function post(path, body) {
  const res = await fetch(`${BASE}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body ?? {}),
  });
  return res.json();
}

async function tool(name, args) {
  const out = await post("/api/tool", { name, args });
  // Print compactly: an agent re-reads this on every step, so noise here is
  // context spent on nothing.
  console.log(JSON.stringify(out, null, 2));
  if (out.ok === false) process.exitCode = 1;
}

const HELP = `gdrb — drive the desktop for a benchmark run

  shot [--profile P] [--settle]   capture the screen; prints an image path
  zoom <x> <y> <w> <h>            native-resolution crop, for small targets
  click <x> <y> [--clicks N]      click in the latest image's pixel space
  move <x> <y>                    move the pointer
  scroll <dy> [--at <x> <y>]      wheel scroll; positive dy scrolls down
  key <Key>                       tap a key, e.g. Return / Escape / Page_Down
  hotkey <Combo>                  e.g. Ctrl+a
  type <text...>                  type literal text
  act '<json>'                    batch steps, then screenshot
  status                          current run and its running totals

Coordinates are always in the pixel space of the most recent screenshot.`;

switch (cmd) {
  case "shot": {
    // `compact` by default: it keeps the saved image at or under 1024px on the
    // long edge, which is where most image viewers stop resizing. If the
    // viewer resized it, the coordinates an agent measures would no longer be
    // the coordinates it clicks.
    const profile = flag("profile", "compact");
    const settle = has("settle");
    await tool("gdr_screenshot", {
      profile,
      ...(settle ? { settle: true } : {}),
    });
    break;
  }

  case "zoom": {
    const [x, y, w, h] = argv.slice(1);
    await tool("gdr_zoom", {
      x: num(x, "x"),
      y: num(y, "y"),
      width: num(w, "width"),
      height: num(h, "height"),
    });
    break;
  }

  case "click":
  case "dblclick": {
    const button = flag("button");
    const clicks = flag("clicks");
    const [x, y] = argv.slice(1);
    await tool(cmd === "dblclick" ? "gdr_double_click" : "gdr_click", {
      x: num(x, "x"),
      y: num(y, "y"),
      ...(button ? { button } : {}),
      ...(clicks ? { clicks: num(clicks, "clicks") } : {}),
    });
    break;
  }

  case "move": {
    const [x, y] = argv.slice(1);
    await tool("gdr_move", { x: num(x, "x"), y: num(y, "y") });
    break;
  }

  case "scroll": {
    // `--at` takes two values, so it is read before positionals are sliced.
    const i = argv.indexOf("--at");
    let at;
    if (i >= 0) {
      at = { x: num(argv[i + 1], "at x"), y: num(argv[i + 2], "at y") };
      argv.splice(i, 3);
    }
    const dy = argv[1] === undefined ? 3 : num(argv[1], "dy");
    await tool("gdr_scroll", { dy, ...(at ? { x: at.x, y: at.y } : {}) });
    break;
  }

  case "key":
    if (!argv[1]) fail("key needs a key name, e.g. Return");
    await tool("gdr_key", { key: argv[1] });
    break;

  case "hotkey":
    if (!argv[1]) fail("hotkey needs a combo, e.g. Ctrl+a");
    await tool("gdr_hotkey", { hotkey: argv[1] });
    break;

  case "type": {
    const text = argv.slice(1).join(" ");
    if (!text) fail("type needs some text");
    await tool("gdr_type", { text });
    break;
  }

  case "act": {
    let steps;
    try {
      steps = JSON.parse(argv[1] ?? "");
    } catch {
      fail("act needs a JSON array of steps");
    }
    await tool("gdr_act", { steps });
    break;
  }

  case "status": {
    const res = await fetch(`${BASE}/api/run`);
    console.log(JSON.stringify(await res.json(), null, 2));
    break;
  }

  default:
    console.log(HELP);
    if (cmd) process.exitCode = 2;
}
