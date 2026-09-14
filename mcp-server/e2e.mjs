// End-to-end exercise of the gdr MCP server over real stdio JSON-RPC,
// against the live desktop. Run: node e2e.mjs
import { spawn } from "node:child_process";

const proc = spawn("node", ["/usr/share/gdr/mcp-server/dist/index.js"], {
  stdio: ["pipe", "pipe", "inherit"],
});

let buf = "";
const pending = new Map();
proc.stdout.on("data", (d) => {
  buf += d.toString();
  let nl;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (!line) continue;
    const msg = JSON.parse(line);
    const resolve = pending.get(msg.id);
    if (resolve) {
      pending.delete(msg.id);
      resolve(msg);
    }
  }
});

let id = 0;
function rpc(method, params) {
  const myId = ++id;
  return new Promise((resolve) => {
    pending.set(myId, resolve);
    proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: myId, method, params }) + "\n");
  });
}

async function call(name, args) {
  const t0 = performance.now();
  const r = await rpc("tools/call", { name, arguments: { dev: "local", ...args } });
  const ms = performance.now() - t0;
  const content = r.result?.content ?? [];
  const text = content.find((c) => c.type === "text")?.text;
  const image = content.find((c) => c.type === "image");
  return {
    ms,
    isError: r.result?.isError ?? false,
    meta: text ? JSON.parse(text) : undefined,
    imageKb: image ? image.data.length / 1024 : 0,
    mime: image?.mimeType,
  };
}

function show(label, r) {
  const m = r.meta ?? {};
  const parts = [
    `${label.padEnd(34)}`,
    `${r.ms.toFixed(0).padStart(5)}ms`,
    r.imageKb ? `${r.imageKb.toFixed(0).padStart(4)}KB ${r.mime}` : "  no image      ",
  ];
  if (m.image_width) parts.push(`${m.image_width}x${m.image_height} ${m.visual_tokens}tok`);
  if (m.unchanged) parts.push("UNCHANGED");
  if (m.settled === false) parts.push("not-settled");
  if (r.isError) parts.push("ERROR");
  console.log(parts.join("  "));
  return m;
}

await rpc("initialize", {
  protocolVersion: "2024-11-05",
  capabilities: {},
  clientInfo: { name: "e2e", version: "1" },
});

console.log("--- sizing profiles ---");
const claude = show("gdr_screenshot (claude default)", await call("gdr_screenshot", {}));
show("gdr_screenshot profile=claude-hires", await call("gdr_screenshot", { profile: "claude-hires" }));
show("gdr_screenshot profile=openai", await call("gdr_screenshot", { profile: "openai" }));
show("gdr_screenshot profile=raw", await call("gdr_screenshot", { profile: "raw" }));

console.log("\n--- zoom on a small target ---");
// Top-left of the GNOME panel: the Activities corner, ~30px tall natively.
const zoom = show(
  "gdr_zoom top-left panel 140x26",
  await call("gdr_zoom", { x: 0, y: 0, width: 140, height: 26 })
);
console.log(
  `   zoom shows ${zoom.region.width}x${zoom.region.height} native at +${zoom.region.x},${zoom.region.y} ` +
    `as ${zoom.image_width}x${zoom.image_height} (${zoom.zoom}x)`
);

console.log("\n--- click safety after a zoom ---");
const bad = await call("gdr_click", { x: 1200, y: 800 });
console.log(
  `   full-desktop coords against a zoom frame: ${bad.isError ? "REJECTED" : "ACCEPTED (BUG)"}`
);
if (bad.meta?.error) console.log(`   ${bad.meta.error.split(".")[0]}.`);

console.log("\n--- unchanged short circuit ---");
await call("gdr_screenshot", {});
show("gdr_screenshot skip_unchanged", await call("gdr_screenshot", { skip_unchanged: true }));

console.log("\n--- gdr_act: open GNOME overview, verify, close ---");
const opened = show(
  "gdr_act [Super] expect_change",
  await call("gdr_act", { steps: [{ hotkey: "Super" }], expect_change: true })
);
console.log(`   steps ok=${opened.ok ?? "?"} changed=${opened.changed}`);
await new Promise((r) => setTimeout(r, 600));
const closed = show("gdr_act [Escape]", await call("gdr_act", { steps: [{ tap: "Escape" }] }));
console.log(`   completed ${closed.completed}/${closed.steps}`);

console.log("\n--- gdr_act failure reporting ---");
const failed = await call("gdr_act", {
  steps: [{ tap: "Escape" }, { tap: "NoSuchKeyXyz" }, { tap: "Escape" }],
});
console.log(
  `   ${failed.isError ? "reported as error" : "NOT an error (BUG)"}; ` +
    `completed ${failed.meta.completed}/${failed.meta.steps}, failed_at ${failed.meta.failed_at}`
);
console.log(`   screenshot still returned: ${failed.imageKb > 0 ? "yes" : "no"}`);

proc.kill();
