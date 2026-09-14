// End-to-end exercise of the window tools over real MCP stdio JSON-RPC,
// against a live gdrd.
//
//   node e2e-windows.mjs [device]        # default: the gdr default device
//
// Three outcomes, and the script says which it saw:
//
//   * gdr-windows extension present  — windows are listed, pinned, captured
//     and acted on for real. The only full pass.
//   * extension absent               — every window tool must fail with the
//     install hint and nothing else. That is the state every target is in
//     before its first logout, so it is worth asserting rather than skipping.
//   * daemon too old                 — gdrd predates the window plane and
//     drops the connection on an unknown request. Reported as a skew error
//     with the fix, not as a pile of unrelated failures.
//
// What is NOT covered: it never closes a window or launches an app, because
// this runs against whatever desktop the developer is using.

import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

import { GdrClient } from "./dist/gdrClient.js";

const DEV = process.argv[2] || undefined;
const here = path.dirname(fileURLToPath(import.meta.url));
const proc = spawn("node", [path.join(here, "dist", "index.js")], {
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

async function call(name, args = {}) {
  const t0 = performance.now();
  const r = await rpc("tools/call", {
    name,
    arguments: { ...(DEV ? { dev: DEV } : {}), ...args },
  });
  const ms = Math.round(performance.now() - t0);
  const content = r.result?.content ?? [];
  const text = content.find((c) => c.type === "text")?.text ?? "";
  const image = content.find((c) => c.type === "image");
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    /* non-JSON text result */
  }
  return { ms, json, text, image, isError: Boolean(r.result?.isError), raw: r };
}

/**
 * Assert the daemon's Rust deserializer accepts the requests this TypeScript
 * client builds.
 *
 * The two definitions are mirrored by hand (common/src/lib.rs vs
 * gdrClient.ts), and `WindowAction` is the fragile one: it flattens the
 * operation and its arguments into the same object as the selector. A shape
 * mismatch does not come back as an error — gdrd fails to parse the frame and
 * drops the connection — so a socket error here means the mirror has drifted,
 * not that the desktop is unhappy.
 */
async function checkWireShapes() {
  console.log("\n== wire shapes accepted by the Rust daemon");
  const cfgPath = path.join(homedir(), ".config", "gdr", "config.json");
  const cfg = JSON.parse(readFileSync(cfgPath, "utf8"));
  const name = DEV
    ? Object.keys(cfg.hosts).find(
        (n) =>
          n === DEV ||
          cfg.hosts[n].label === DEV ||
          (cfg.hosts[n].aliases ?? []).includes(DEV)
      )
    : cfg.default_host;
  const profile = cfg.hosts?.[name];
  if (!profile) {
    check("resolve device for the raw client", false, `no profile for ${DEV ?? "(default)"}`);
    return;
  }
  const client = new GdrClient({
    host: /^(local|localhost|\.|this|loopback)$/i.test(profile.address)
      ? "127.0.0.1"
      : profile.address,
    port: profile.port ?? 7337,
    token: profile.token,
    pinnedFingerprint: profile.pin ?? undefined,
  });
  const cases = [
    ["ListWindows", { type: "ListWindows", include_skip_taskbar: true }],
    ["WindowAction activate", { type: "WindowAction", target: { title: "x" }, action: "activate" }],
    [
      "WindowAction move_resize",
      { type: "WindowAction", target: { id: 7 }, action: "move_resize", x: 1, y: 2, width: 3, height: 4 },
    ],
    ["WindowAction workspace", { type: "WindowAction", target: { focused: true }, action: "workspace", index: 2 }],
    ["WindowEvents", { type: "WindowEvents", since: 0, limit: 10, wait_ms: 0 }],
    ["ListApps", { type: "ListApps", filter: "term" }],
    ["LaunchApp", { type: "LaunchApp", app_id: "does.not.exist" }],
  ];
  for (const [label, req] of cases) {
    try {
      const r = await client.request(req);
      // Anything but a dropped connection means the frame deserialized; the
      // reply may still be an Error because the extension is missing or the
      // window does not exist, which is not what this is testing.
      check(`${label} deserializes`, r.type === "Error" || r.type !== undefined, r.type);
    } catch (e) {
      check(`${label} deserializes`, false, `connection dropped: ${e.message}`);
    }
  }
  client.close();
}

let failures = 0;
function check(label, ok, detail = "") {
  console.log(`${ok ? "  ok  " : "  FAIL"}  ${label}${detail ? `  — ${detail}` : ""}`);
  if (!ok) failures++;
}

const HINT = /gdr-windows GNOME Shell extension is not running/;
const SKEW = /closed the connection without answering/;

async function main() {
  await rpc("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "e2e-windows", version: "1" },
  });
  proc.stdin.write(
    JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n"
  );

  const tools = await rpc("tools/list", {});
  const names = (tools.result?.tools ?? []).map((t) => t.name);
  const expected = [
    "gdr_windows",
    "gdr_window_info",
    "gdr_window_control",
    "gdr_window_screenshot",
    "gdr_window_act",
    "gdr_window_events",
    "gdr_window_pin",
    "gdr_window_log",
    "gdr_app_launch",
  ];
  console.log("\n== tool registration");
  for (const t of expected) check(`${t} registered`, names.includes(t));

  console.log("\n== gdr_windows");
  const list = await call("gdr_windows");
  const haveExtension = !list.isError;

  if (list.isError && SKEW.test(list.text)) {
    // Not a test failure — the target simply has not been updated. Saying so
    // once beats thirteen identical assertion failures that all mean this.
    check("skew is reported clearly rather than hanging", true);
    console.log(
      "\n  gdrd on this target predates the window plane and does not know the\n" +
        "  window requests. Update the daemon and re-run:\n" +
        "\n    cargo build --release && ./scripts/install-local.sh   (or scripts/update.sh)\n"
    );
    console.log(`\n${failures === 0 ? "SKIPPED" : "FAIL"} — daemon too old\n`);
    proc.kill();
    process.exit(failures === 0 ? 0 : 1);
  }

  if (!haveExtension) {
    console.log("\n  (gdr-windows extension not active on the target)\n");
    check("error names the extension", HINT.test(list.text), list.text.slice(0, 90));
    for (const [tool, args] of [
      ["gdr_window_info", {}],
      ["gdr_window_control", { action: "activate" }],
      ["gdr_window_screenshot", {}],
      ["gdr_window_events", {}],
      ["gdr_app_launch", { list: true }],
    ]) {
      const r = await call(tool, args);
      check(`${tool} fails with the install hint`, r.isError && HINT.test(r.text));
    }
    // The log is controller-side, so it must work regardless of the target.
    const log = await call("gdr_window_log", { tail: 5 });
    check("gdr_window_log works without the extension", !log.isError, log.json?.path);
    console.log("\n  Install it and log out/in, then re-run for the full pass.");
  } else {
    check("lists windows", Array.isArray(list.json.windows), `${list.json.count} windows`);
    check("names the capture backend", list.json.backend === "extension");
    check("reports monitors", (list.json.monitors ?? []).length > 0);
    const capturable = (list.json.windows ?? []).filter((w) => w.capturable);
    check("at least one window is capturable", capturable.length > 0);

    const target = capturable[0] ?? list.json.windows[0];
    console.log(`\n== single window: ${target.app_id} — ${target.title}`);

    const info = await call("gdr_window_info", { id: target.id });
    check("gdr_window_info resolves by id", info.json?.window?.id === target.id);

    const shot = await call("gdr_window_screenshot", { id: target.id });
    check("gdr_window_screenshot returns an image", Boolean(shot.image), `${shot.ms}ms`);
    if (shot.image) {
      const meta = JSON.parse(shot.text.split("\n")[0]);
      check("screenshot is cropped to the window", meta.window?.id === target.id);
    }

    console.log("\n== pin");
    const pinned = await call("gdr_window_pin", { id: target.id, note: "e2e" });
    check("pins the window", pinned.json?.pinned_window?.id === target.id);
    const viaPin = await call("gdr_window_info");
    check("bare call now resolves via the pin", viaPin.json?.source === "pin");
    check("pin points at the right window", viaPin.json?.window?.id === target.id);
    const shown = await call("gdr_window_pin");
    check("bare gdr_window_pin reports rather than repins", shown.json?.resolves_to != null);
    const cleared = await call("gdr_window_pin", { clear: true });
    check("clears the pin", cleared.json?.pinned_window === null);
    check("clearing reports the previous pin", cleared.json?.previous?.id === target.id);

    console.log("\n== events");
    const ev = await call("gdr_window_events", { since: 0, limit: 20 });
    check("polls events", Array.isArray(ev.json?.events), `next_seq=${ev.json?.next_seq}`);
    const ev2 = await call("gdr_window_events", { since: ev.json.next_seq, wait_ms: 0 });
    check("resuming from next_seq returns nothing new", ev2.json?.count === 0);

    console.log("\n== apps");
    const apps = await call("gdr_app_launch", { list: true, filter: "termin" });
    check("lists installed apps", Array.isArray(apps.json?.apps), `${apps.json?.count} match`);

    console.log("\n== log");
    const log = await call("gdr_window_log", { tail: 10 });
    check("log recorded the pin changes", (log.json?.entries ?? []).some((e) => e.kind === "pin_set"));
  }

  await checkWireShapes();

  console.log("\n== selector errors (no daemon needed)");
  const bogus = await call("gdr_window_info", { id: 999999999 });
  check("unknown id is a clean error, not a crash", bogus.isError);

  console.log(
    `\n${failures === 0 ? "PASS" : `FAIL (${failures})`} — extension ${haveExtension ? "active" : "absent"}\n`
  );
  proc.kill();
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  proc.kill();
  process.exit(1);
});
