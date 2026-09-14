// End-to-end exercise of the subscription hooks over real MCP stdio
// JSON-RPC, against a live gdrd.
//
//   node e2e-hooks.mjs [device]            # lifecycle only — touches nothing
//   node e2e-hooks.mjs rig --drive         # also opens and drives real windows
//
// Two halves, because the interesting half needs something to actually
// happen on the target desktop:
//
//   * lifecycle  — create, list, toggle, reconfigure, drain, remove. Safe to
//     run against the desktop you are sitting in front of: it watches, it
//     does not touch.
//   * --drive    — launches chromium on the target, resizes it and closes it,
//     and asserts the hooks reported each step with the right geometry and
//     the right owning process. Meant for the throwaway session that
//     scripts/hook-test-rig.sh brings up; do not point it at your own
//     desktop unless you want windows opening on it.
//
// The rig is how the window half gets tested at all: GNOME only scans for
// shell extensions at session start, so a change to shell-extension/ is
// invisible to your own session until you log out.

import { spawn, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";

const args = process.argv.slice(2);
const DEV = args.find((a) => !a.startsWith("--"));
const DRIVE = args.includes("--drive");
const here = path.dirname(fileURLToPath(import.meta.url));
const RIG = path.join(here, "..", "scripts", "hook-test-rig.sh");

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

async function call(name, extra = {}) {
  const r = await rpc("tools/call", {
    name,
    arguments: { ...(DEV ? { dev: DEV } : {}), ...extra },
  });
  const text = (r.result?.content ?? []).find((c) => c.type === "text")?.text ?? "";
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    /* non-JSON result */
  }
  return { json, text, isError: Boolean(r.result?.isError) };
}

let failures = 0;
function check(label, ok, detail = "") {
  console.log(`${ok ? "  ok  " : "  FAIL"}  ${label}${detail ? `  — ${detail}` : ""}`);
  if (!ok) failures++;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Drain until an event of `kind` shows up, or the budget runs out. */
async function waitFor(kind, hookId, budgetMs = 15000) {
  const deadline = Date.now() + budgetMs;
  let since = 0;
  const seen = [];
  while (Date.now() < deadline) {
    const r = await call("gdr_hook_events", {
      id: hookId,
      since,
      wait_ms: Math.min(4000, Math.max(0, deadline - Date.now())),
    });
    if (r.isError) return { error: r.text, seen };
    since = r.json.next_seq;
    seen.push(...r.json.events.map((e) => e.kind));
    const hit = r.json.events.find((e) => e.kind === kind);
    if (hit) return { event: hit, since, seen };
  }
  return { seen };
}

const HINT = /gdr-windows GNOME Shell extension is not running/;

/**
 * Create, describe, toggle, reconfigure and drain one hook.
 *
 * Parameterised by kind because the two are deliberately interchangeable
 * here: a target whose shell extension is not installed yet cannot make a
 * window hook at all, and running the same lifecycle against an activity
 * hook keeps that target covered instead of skipped.
 */
async function lifecycle(kind) {
  const tool = kind === "window" ? "gdr_hook_window" : "gdr_hook_screen";
  const scope = kind === "window" ? "window" : "screenshot";
  const extra = kind === "window" ? { events: ["opened", "closed", "resized"] } : {};

  console.log(`\n== a ${kind} hook, created and described`);
  const made = await call(tool, { label: "e2e lifecycle", buffer_ms: 300, ...extra });
  if (made.isError) {
    if (kind === "window" && HINT.test(made.text)) {
      check("a window hook is refused, with the fix, when the extension is missing", true);
      console.log(
        "\n  (the target has no gdr-windows extension yet — window hooks cannot run\n" +
          "   there at all, so the lifecycle runs against an activity hook instead)\n"
      );
      return lifecycle("activity");
    }
    check(`create a ${kind} hook`, false, made.text.slice(0, 140));
    return null;
  }
  const hook = made.json.hook;
  check(`create a ${kind} hook`, true, hook.id);
  check("reports the scope it needs", hook.scopes?.required === scope, hook.scopes?.required);
  check(
    "reports the scopes it was created with",
    Array.isArray(hook.scopes?.created_with) && hook.scopes.created_with.length > 0,
    JSON.stringify(hook.scopes?.created_with)
  );
  check("starts enabled", hook.enabled === true, hook.state);
  check("echoes the buffer time", hook.buffer_ms === 300, String(hook.buffer_ms));

  console.log("\n== the toggle");
  const off = await call("gdr_hooks", { action: "disable", id: hook.id });
  check("disable switches it off", off.json?.hook?.enabled === false, off.json?.hook?.state);
  check(
    "disabling keeps the configuration",
    off.json?.hook?.watching === hook.watching,
    off.json?.hook?.watching
  );
  const on = await call("gdr_hooks", { action: "enable", id: hook.id });
  check("enable switches it back on", on.json?.hook?.enabled === true, on.json?.hook?.state);
  check("the id survives the round trip", on.json?.hook?.id === hook.id);

  console.log("\n== reconfiguring in place");
  const changed = await call(tool, { id: hook.id, buffer_ms: 700, ...extra });
  check("updates rather than creating a second hook", changed.json?.action === "updated");
  check(
    "the new buffer time took",
    changed.json?.hook?.buffer_ms === 700,
    String(changed.json?.hook?.buffer_ms)
  );

  console.log("\n== listing");
  const listed = await call("gdr_hooks", { action: "list" });
  check("the hook is listed", (listed.json?.hooks ?? []).some((h) => h.id === hook.id));

  console.log("\n== an empty poll explains itself");
  const drained = await call("gdr_hook_events", { id: hook.id, since: 0 });
  check("poll succeeds", !drained.isError, drained.text.slice(0, 120));
  if (!drained.isError && drained.json.events.length === 0) {
    check(
      "says why there is nothing rather than just 'count: 0'",
      typeof drained.json.note === "string" && drained.json.note.length > 20,
      drained.json.note
    );
  }

  console.log("\n== bad input is refused, not silently ignored");
  const badEvent = await call("gdr_hook_window", { events: ["resize"] });
  check("a misspelled event name is rejected", badEvent.isError, badEvent.text.slice(0, 120));
  const noId = await call("gdr_hooks", { action: "disable" });
  check("disable without an id says so", noId.isError, noId.text.slice(0, 90));

  return hook.id;
}

async function drive(lifecycleHookId) {
  if (!existsSync(RIG)) {
    console.log("\n  (no rig script found; skipping the driven half)");
    return;
  }
  console.log("\n== driving real windows in the rig");

  const watch = await call("gdr_hook_window", {
    label: "e2e drive",
    events: ["opened", "closed", "resized", "moved", "retitled"],
    buffer_ms: 300,
    poll_ms: 150,
  });
  if (watch.isError) {
    check("create the driving hook", false, watch.text.slice(0, 140));
    return;
  }
  const hookId = watch.json.hook.id;

  const screen = await call("gdr_hook_screen", { label: "e2e activity", buffer_ms: 400 });
  check("create an activity hook", !screen.isError, screen.text.slice(0, 120));
  const activityId = screen.json?.hook?.id;

  // The rig script finds its session through GDR_RIG_DIR, so --drive only
  // works when that is set to the same rig this device points at. The
  // script's own error says which directory it looked in.
  spawnSync("bash", [RIG, "chromium", "data:text/html,<h1>gdr+hooks</h1>"], {
    stdio: "inherit",
    env: process.env,
  });

  const opened = await waitFor("opened", hookId, 25000);
  check("chromium opening is reported", Boolean(opened.event), opened.seen.join(",") || opened.error);
  if (!opened.event) return;

  const w = opened.event.window;
  check("the event carries a title", typeof w.title === "string" && w.title.length > 0, w.title);
  check("the event carries a size", w.size?.width > 0 && w.size?.height > 0, JSON.stringify(w.size));
  check("the event carries the owning process", w.process?.pid > 0, JSON.stringify(w.process?.pid));
  check("the process is named", /chrom/i.test(w.process?.command ?? ""), w.process?.command);
  check("the process owner is resolved", Boolean(w.process?.user), w.process?.user);
  check("the executable is resolved", Boolean(w.process?.exe), w.process?.exe);
  check("the geometry has settled", w.settled === true);

  // Derive the target size from what the window actually is, rather than
  // hard-coding one: chromium remembers its geometry in the profile, so a
  // fixed size silently becomes a no-op resize on the second run — and a
  // no-op resize correctly produces no event, which looks like a bug in the
  // hook rather than in the test.
  const targetTitle = "Chromium";
  const want = { width: w.size.width - 137, height: w.size.height - 91 };
  const resize = await call("gdr_window_control", {
    action: "resize",
    title: targetTitle,
    ...want,
  });
  check("resize the window", !resize.isError, resize.text.slice(0, 120));

  const resized = await waitFor("resized", hookId, 15000);
  check("the resize is reported", Boolean(resized.event), resized.seen.join(","));
  if (resized.event) {
    const r = resized.event.window;
    check(
      "the reported size is the size it ended at",
      r.size.width === want.width && r.size.height === want.height,
      `${JSON.stringify(r.size)} want ${JSON.stringify(want)}`
    );
    check(
      "the previous geometry comes with it",
      Number.isFinite(r.previous?.dw),
      JSON.stringify(r.previous)
    );
    check(
      "a drag-resize collapses into one event",
      resized.seen.filter((k) => k === "resized").length === 1,
      resized.seen.join(",")
    );
  }

  if (activityId) {
    const act = await waitFor("activity", activityId, 15000);
    check("screen activity is reported", Boolean(act.event), act.seen.join(","));
    if (act.event) {
      const a = act.event.activity;
      check(
        "activity comes back as a circle in stream pixels",
        Number.isFinite(a.circle?.x) && a.circle.radius > 0,
        JSON.stringify(a.circle)
      );
      check("the bounding box it came from is included", Number.isFinite(a.bbox?.width));
      check("the buffer time is echoed", a.buffer_ms > 0, String(a.buffer_ms));
    }
    await call("gdr_hooks", { action: "remove", id: activityId });
  }

  const close = await call("gdr_window_control", { action: "close", title: targetTitle });
  check("close the window", !close.isError, close.text.slice(0, 120));
  const closed = await waitFor("closed", hookId, 15000);
  check("the close is reported", Boolean(closed.event), closed.seen.join(","));
  if (closed.event) {
    check(
      "a closed window still says who owned it",
      closed.event.window?.process?.pid > 0,
      JSON.stringify(closed.event.window?.process?.command)
    );
  }

  await call("gdr_hooks", { action: "remove", id: hookId });
  if (lifecycleHookId) await call("gdr_hooks", { action: "remove", id: lifecycleHookId });
}

async function main() {
  await rpc("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "e2e-hooks", version: "1" },
  });
  proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");

  const hookId = await lifecycle("window");
  if (DRIVE) {
    await drive(hookId);
  } else if (hookId) {
    console.log("\n  (lifecycle only — pass --drive against the rig to open real windows)");
    await call("gdr_hooks", { action: "remove", id: hookId });
  }

  console.log(`\n${failures === 0 ? "PASS" : `FAIL (${failures})`}\n`);
  proc.kill();
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error(e);
  proc.kill();
  process.exit(1);
});
