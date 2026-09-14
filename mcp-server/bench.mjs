// Ad-hoc benchmark of the CaptureFrame path against the legacy Screenshot
// path, using the MCP server's own client. Run: node bench.mjs
import { GdrClient } from "./dist/gdrClient.js";
import { profileRequest, visualTokens } from "./dist/screenshotLayout.js";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const cfg = JSON.parse(
  fs.readFileSync(path.join(os.homedir(), ".config", "gdr", "config.json"), "utf8")
);
const h = cfg.hosts.local;
const client = new GdrClient({
  host: "127.0.0.1",
  port: h.port ?? 7337,
  token: h.token,
  pinnedFingerprint: h.pin ?? undefined,
});

const N = 10;
async function bench(label, fn) {
  await fn();
  const t0 = performance.now();
  let last;
  for (let i = 0; i < N; i++) last = await fn();
  const ms = (performance.now() - t0) / N;
  return { label, ms, last };
}

const rows = [];

{
  const r = await bench("legacy Screenshot (native PNG)", () =>
    client.request({ type: "Screenshot", connector: null })
  );
  const bytes = r.last.png_base64.length;
  rows.push({
    path: r.label,
    ms: r.ms.toFixed(1),
    payload_kb: (bytes / 1024).toFixed(0),
    size: "1920x1200",
    tokens: visualTokens(1920, 1200),
  });
}

for (const profile of ["claude", "claude-hires", "openai"]) {
  const r = await bench(`CaptureFrame profile=${profile} jpeg`, () =>
    client.captureFrame({ ...profileRequest(profile), format: "jpeg", quality: 85 })
  );
  rows.push({
    path: r.label,
    ms: r.ms.toFixed(1),
    payload_kb: (r.last.data_base64.length / 1024).toFixed(0),
    size: `${r.last.image_width}x${r.last.image_height}`,
    tokens: visualTokens(r.last.image_width, r.last.image_height),
  });
}

{
  const first = await client.captureFrame({ ...profileRequest("claude"), format: "jpeg" });
  const r = await bench("CaptureFrame unchanged hit", () =>
    client.captureFrame({
      ...profileRequest("claude"),
      format: "jpeg",
      if_none_match: first.hash,
    })
  );
  rows.push({
    path: r.label + (r.last.unchanged ? " [unchanged=true]" : " [MISS]"),
    ms: r.ms.toFixed(1),
    payload_kb: (r.last.data_base64.length / 1024).toFixed(0),
    size: `${r.last.image_width}x${r.last.image_height}`,
    tokens: r.last.unchanged ? 0 : visualTokens(r.last.image_width, r.last.image_height),
  });
}

{
  const r = await bench("CaptureFrame + settle(120/1500)", () =>
    client.captureFrame({
      ...profileRequest("claude"),
      format: "jpeg",
      settle: { quiet_ms: 120, timeout_ms: 1500 },
    })
  );
  rows.push({
    path: `${r.label} settled=${r.last.settled}`,
    ms: r.ms.toFixed(1),
    payload_kb: (r.last.data_base64.length / 1024).toFixed(0),
    size: `${r.last.image_width}x${r.last.image_height}`,
    tokens: visualTokens(r.last.image_width, r.last.image_height),
  });
}

{
  const r = await bench("gdr_zoom 400x300 native crop", () =>
    client.captureFrame({ region: { x: 1000, y: 700, width: 400, height: 300 }, format: "png" })
  );
  rows.push({
    path: r.label,
    ms: r.ms.toFixed(1),
    payload_kb: (r.last.data_base64.length / 1024).toFixed(0),
    size: `${r.last.image_width}x${r.last.image_height}`,
    tokens: visualTokens(r.last.image_width, r.last.image_height),
  });
}

console.table(rows);
client.close();
