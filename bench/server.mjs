// Benchmark harness for GDR computer use.
//
// One process does three jobs so a run has a single source of truth:
//
//   1. Serves the task pages.
//   2. Collects scoring events the pages post back, so success is measured by
//      the task itself rather than by asking the agent whether it succeeded.
//   3. Bridges agent tool calls to the real MCP server over stdio, recording
//      latency, payload and visual-token cost for every one.
//
// Everything an agent does therefore lands in the same run log as the score it
// earned, which is what makes the two comparable afterwards.
//
// Run: node bench/server.mjs [--port 7788]

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { readFile, mkdir, writeFile, appendFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const TASKS_DIR = path.join(HERE, "tasks");
const RESULTS_DIR = path.join(HERE, "results");
const SHOTS_DIR = path.join(HERE, "shots");

const MCP_ENTRY = existsSync("/usr/share/gdr/mcp-server/dist/index.js")
  ? "/usr/share/gdr/mcp-server/dist/index.js"
  : path.join(HERE, "..", "mcp-server", "dist", "index.js");

const PORT = Number(argOf("--port") ?? 7788);
const DEVICE = argOf("--dev") ?? "local";

function argOf(flag) {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

/**
 * Which tools an agent is allowed to use.
 *
 * The point of the `basic` set is to reproduce the pre-optimisation surface —
 * screenshot, click, key, nothing else — so the cost of the batching and zoom
 * tools can be measured rather than asserted.
 */
const TOOLSETS = {
  full: [
    "gdr_screenshot",
    "gdr_zoom",
    "gdr_act",
    "gdr_click",
    "gdr_double_click",
    "gdr_move",
    "gdr_scroll",
    "gdr_key",
    "gdr_hotkey",
    "gdr_type",
  ],
  basic: [
    "gdr_screenshot",
    "gdr_click",
    "gdr_double_click",
    "gdr_move",
    "gdr_scroll",
    "gdr_key",
    "gdr_type",
  ],
};

// ---------------------------------------------------------------- MCP client

/** Long-lived MCP server speaking stdio JSON-RPC, shared by every tool call. */
class McpBridge {
  #proc;
  #pending = new Map();
  #buf = "";
  #id = 0;
  #ready;

  constructor(entry) {
    this.#proc = spawn("node", [entry], { stdio: ["pipe", "pipe", "inherit"] });
    this.#proc.stdout.on("data", (d) => this.#onData(d));
    this.#ready = this.#rpc("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "gdr-bench", version: "1" },
    });
  }

  #onData(chunk) {
    this.#buf += chunk.toString();
    let nl;
    while ((nl = this.#buf.indexOf("\n")) >= 0) {
      const line = this.#buf.slice(0, nl).trim();
      this.#buf = this.#buf.slice(nl + 1);
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      const resolve = this.#pending.get(msg.id);
      if (resolve) {
        this.#pending.delete(msg.id);
        resolve(msg);
      }
    }
  }

  #rpc(method, params) {
    const id = ++this.#id;
    return new Promise((resolve) => {
      this.#pending.set(id, resolve);
      this.#proc.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });
  }

  async call(name, args) {
    await this.#ready;
    return this.#rpc("tools/call", { name, arguments: { dev: DEVICE, ...args } });
  }

  close() {
    this.#proc.kill();
  }
}

const mcp = new McpBridge(MCP_ENTRY);

// -------------------------------------------------------------- run tracking

/**
 * A single agent attempt at a single task.
 *
 * Metrics are accumulated as the run happens rather than reconstructed from
 * logs afterwards, so a crashed or abandoned run still reports whatever it got
 * through — an agent that gives up halfway is itself a result worth keeping.
 */
class Run {
  constructor({ id, task, toolset, agent, notes }) {
    this.id = id;
    this.task = task;
    this.toolset = toolset;
    this.agent = agent ?? "unknown";
    this.notes = notes ?? "";
    this.started_at = new Date().toISOString();
    this.t0 = performance.now();
    this.tool_calls = 0;
    this.screenshots = 0;
    this.zooms = 0;
    this.acts = 0;
    this.act_steps = 0;
    this.clicks = 0;
    this.keys = 0;
    this.types = 0;
    this.denied = 0;
    this.tool_errors = 0;
    this.visual_tokens = 0;
    this.capture_ms = 0;
    this.events = [];
    this.score = null;
    this.finished = false;
  }

  get wall_ms() {
    return performance.now() - this.t0;
  }

  /**
   * Round trips are the headline cost: each one is a model inference, which
   * dwarfs any latency inside the daemon.
   */
  get round_trips() {
    return this.tool_calls;
  }

  summary() {
    return {
      id: this.id,
      task: this.task,
      toolset: this.toolset,
      agent: this.agent,
      notes: this.notes,
      started_at: this.started_at,
      wall_ms: Math.round(this.wall_ms),
      round_trips: this.round_trips,
      screenshots: this.screenshots,
      zooms: this.zooms,
      acts: this.acts,
      act_steps: this.act_steps,
      clicks: this.clicks,
      keys: this.keys,
      types: this.types,
      denied: this.denied,
      tool_errors: this.tool_errors,
      visual_tokens: this.visual_tokens,
      capture_ms: Math.round(this.capture_ms),
      score: this.score,
      finished: this.finished,
    };
  }
}

/** @type {Run | null} */
let current = null;

async function persist(run) {
  await mkdir(RESULTS_DIR, { recursive: true });
  const out = path.join(RESULTS_DIR, `${run.id}.json`);
  await writeFile(out, JSON.stringify({ ...run.summary(), events: run.events }, null, 2));
  return out;
}

async function logLine(obj) {
  await mkdir(RESULTS_DIR, { recursive: true });
  await appendFile(path.join(RESULTS_DIR, "calls.jsonl"), JSON.stringify(obj) + "\n");
}

// ------------------------------------------------------------- tool dispatch

/**
 * Pull the text/JSON block the MCP tools return alongside their image.
 * Tools report geometry and token cost there, which is where the per-call
 * metrics come from.
 */
function metaOf(result) {
  const text = (result?.content ?? []).find((c) => c.type === "text")?.text;
  if (!text) return {};
  try {
    return JSON.parse(text);
  } catch {
    return { note: text };
  }
}

function imageOf(result) {
  return (result?.content ?? []).find((c) => c.type === "image");
}

/**
 * Run one tool and fold its cost into the active run.
 *
 * Screenshots are written to disk rather than returned inline: the agents
 * driving this read images with a file-reading tool, and a path keeps the
 * transcript small enough to stay readable.
 */
async function runTool(name, args) {
  if (!current) throw new Error("no active run — POST /api/run/start first");
  const allowed = TOOLSETS[current.toolset] ?? TOOLSETS.full;
  if (!allowed.includes(name)) {
    current.denied += 1;
    current.tool_calls += 1;
    return {
      ok: false,
      error: `tool '${name}' is not available in the '${current.toolset}' toolset`,
      available: allowed,
    };
  }

  const t0 = performance.now();
  const rpc = await mcp.call(name, args);
  const ms = performance.now() - t0;
  const result = rpc.result ?? {};
  const meta = metaOf(result);
  const image = imageOf(result);

  current.tool_calls += 1;
  current.capture_ms += meta.capture_ms ?? 0;
  if (typeof meta.visual_tokens === "number") current.visual_tokens += meta.visual_tokens;
  if (name === "gdr_screenshot") current.screenshots += 1;
  if (name === "gdr_zoom") current.zooms += 1;
  if (name === "gdr_click" || name === "gdr_double_click") current.clicks += 1;
  if (name === "gdr_key" || name === "gdr_hotkey") current.keys += 1;
  if (name === "gdr_type") current.types += 1;
  if (name === "gdr_act") {
    current.acts += 1;
    current.act_steps += Array.isArray(args.steps) ? args.steps.length : 0;
    if (args.screenshot !== false) current.screenshots += 1;
  }
  if (result.isError) current.tool_errors += 1;

  let imagePath;
  if (image) {
    await mkdir(SHOTS_DIR, { recursive: true });
    const ext = image.mimeType === "image/png" ? "png" : "jpg";
    imagePath = path.join(
      SHOTS_DIR,
      `${current.id}-${String(current.tool_calls).padStart(3, "0")}.${ext}`
    );
    await writeFile(imagePath, Buffer.from(image.data, "base64"));
  }

  const record = {
    run: current.id,
    seq: current.tool_calls,
    tool: name,
    args,
    ms: Math.round(ms),
    is_error: !!result.isError,
    meta,
    image: imagePath,
  };
  current.events.push(record);
  await logLine(record);

  return {
    ok: !result.isError,
    tool: name,
    ms: Math.round(ms),
    image: imagePath,
    ...meta,
  };
}

// ----------------------------------------------------------------- http glue

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
};

function send(res, code, body, type = "application/json; charset=utf-8") {
  const payload = typeof body === "string" || Buffer.isBuffer(body) ? body : JSON.stringify(body);
  res.writeHead(code, {
    "content-type": type,
    "cache-control": "no-store",
    "access-control-allow-origin": "*",
    "access-control-allow-headers": "content-type",
  });
  res.end(payload);
}

function readBody(req) {
  return new Promise((resolve) => {
    let b = "";
    req.on("data", (d) => (b += d));
    req.on("end", () => {
      try {
        resolve(b ? JSON.parse(b) : {});
      } catch {
        resolve({});
      }
    });
  });
}

const routes = {
  async "POST /api/run/start"(body) {
    if (current && !current.finished) await persist(current);
    const id = body.id ?? `${body.task ?? "task"}-${body.toolset ?? "full"}-${Date.now()}`;
    current = new Run({
      id,
      task: body.task ?? "unknown",
      toolset: body.toolset in TOOLSETS ? body.toolset : "full",
      agent: body.agent,
      notes: body.notes,
    });
    return { ok: true, run: current.summary(), tools: TOOLSETS[current.toolset] };
  },

  async "POST /api/run/end"() {
    if (!current) return { ok: false, error: "no active run" };
    current.finished = true;
    const file = await persist(current);
    const summary = current.summary();
    current = null;
    return { ok: true, summary, file };
  },

  async "GET /api/run"() {
    return current ? { ok: true, run: current.summary() } : { ok: false, error: "no active run" };
  },

  /** Task pages post progress and their own verdict here. */
  async "POST /api/event"(body) {
    if (!current) return { ok: true, ignored: "no active run" };
    const event = { at: Math.round(current.wall_ms), ...body };
    current.events.push({ page_event: event });
    if (body.type === "complete") {
      current.score = {
        success: !!body.success,
        score: typeof body.score === "number" ? body.score : body.success ? 1 : 0,
        detail: body.detail ?? null,
      };
    }
    await logLine({ run: current.id, page_event: event });
    return { ok: true };
  },

  async "POST /api/tool"(body) {
    const { name, args } = body;
    if (!name) return { ok: false, error: "missing tool name" };
    try {
      return await runTool(name, args ?? {});
    } catch (e) {
      if (current) current.tool_errors += 1;
      return { ok: false, error: String(e.message ?? e) };
    }
  },
};

const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);
  const key = `${req.method} ${url.pathname}`;

  if (req.method === "OPTIONS") return send(res, 204, "");

  if (routes[key]) {
    const body = req.method === "POST" ? await readBody(req) : {};
    try {
      return send(res, 200, await routes[key](body));
    } catch (e) {
      return send(res, 500, { ok: false, error: String(e.message ?? e) });
    }
  }

  // Static task files. Confined to tasks/ so a traversal cannot read the repo.
  const rel = url.pathname.endsWith("/") ? `${url.pathname}index.html` : url.pathname;
  const file = path.normalize(path.join(TASKS_DIR, rel));
  if (!file.startsWith(TASKS_DIR)) return send(res, 403, { error: "forbidden" });
  try {
    const data = await readFile(file);
    return send(res, 200, data, MIME[path.extname(file)] ?? "application/octet-stream");
  } catch {
    return send(res, 404, { error: "not found" });
  }
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`gdr bench harness on http://127.0.0.1:${PORT}`);
  console.log(`  tasks   ${TASKS_DIR}`);
  console.log(`  results ${RESULTS_DIR}`);
  console.log(`  mcp     ${MCP_ENTRY}  (dev=${DEVICE})`);
});

for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, async () => {
    if (current && !current.finished) await persist(current);
    mcp.close();
    process.exit(0);
  });
}
