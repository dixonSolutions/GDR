#!/usr/bin/env node
// Sets up and tears down a single benchmark run.
//
// The browser is launched in app mode with a throwaway profile so every run
// starts from the same pixels: no tabs, no restored session, no first-run
// prompts. A benchmark that measures "did the agent notice yesterday's open
// tab" is measuring the wrong thing.
//
// Usage:
//   node bench/runner.mjs open <task> [--toolset full|basic] [--agent name] [--seed N]
//   node bench/runner.mjs end
//   node bench/runner.mjs close

import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import { rm } from "node:fs/promises";

const run = promisify(execFile);

const BASE = process.env.GDR_BENCH_URL ?? "http://127.0.0.1:7788";
const PROFILE_DIR = "/tmp/gdr-bench-chrome";
const TASKS = ["shapes", "cycle", "quiz", "slides", "news"];

const argv = process.argv.slice(2);
const cmd = argv[0];

function flag(name, fallback) {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : fallback;
}

async function post(path, body) {
  const res = await fetch(`${BASE}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body ?? {}),
  });
  return res.json();
}

async function browserBinary() {
  for (const bin of ["chromium", "chromium-browser", "google-chrome"]) {
    try {
      const { stdout } = await run("which", [bin]);
      if (stdout.trim()) return bin;
    } catch {
      /* try the next one */
    }
  }
  throw new Error("no chromium/chrome found on PATH");
}

async function closeBrowser() {
  try {
    await run("pkill", ["-f", PROFILE_DIR]);
  } catch {
    /* nothing running */
  }
  // Chromium needs a moment to release the profile lock before relaunch.
  await new Promise((r) => setTimeout(r, 700));
}

async function openTask() {
  const task = argv[1];
  if (!TASKS.includes(task)) {
    console.error(`unknown task '${task}'. Choose one of: ${TASKS.join(", ")}`);
    process.exit(2);
  }
  const toolset = flag("toolset", "full");
  const agent = flag("agent", "unknown");
  const seed = flag("seed");

  await closeBrowser();
  // A fresh profile each time, so cache and history cannot leak between runs.
  await rm(PROFILE_DIR, { recursive: true, force: true });

  const started = await post("/api/run/start", {
    task,
    toolset,
    agent,
    notes: seed ? `seed=${seed}` : "",
  });
  if (!started.ok) {
    console.error("could not start run:", started.error);
    process.exit(1);
  }

  const url = `${BASE}/${task}/${seed ? `?seed=${seed}` : ""}`;
  const bin = await browserBinary();
  const child = spawn(
    bin,
    [
      `--app=${url}`,
      `--user-data-dir=${PROFILE_DIR}`,
      "--window-position=0,0",
      "--window-size=1920,1180",
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-session-crashed-bubble",
      "--disable-infobars",
      "--hide-crash-restore-bubble",
    ],
    { detached: true, stdio: "ignore" }
  );
  child.unref();

  // Give the window time to map and paint before an agent screenshots it.
  await new Promise((r) => setTimeout(r, 2500));

  console.log(
    JSON.stringify(
      {
        ok: true,
        task,
        toolset,
        agent,
        url,
        run: started.run.id,
        tools: started.tools,
      },
      null,
      2
    )
  );
}

async function endRun() {
  const out = await post("/api/run/end");
  console.log(JSON.stringify(out, null, 2));
}

switch (cmd) {
  case "open":
    await openTask();
    break;
  case "end":
    await endRun();
    break;
  case "close":
    await closeBrowser();
    console.log("browser closed");
    break;
  default:
    console.log(
      "usage: runner.mjs open <task> [--toolset full|basic] [--agent name] [--seed N] | end | close"
    );
    process.exitCode = 2;
}
