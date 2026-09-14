#!/usr/bin/env node
// Aggregate benchmark runs into something readable.
//
// Reports per-run detail and a per-toolset comparison. Round trips are printed
// next to wall time deliberately: a run can be quick in seconds and still
// expensive in inferences, and the inference count is what actually costs an
// agent anything.
//
// Usage: node bench/analyse.mjs [--json] [--dir bench/results]

import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const dirFlag = process.argv.indexOf("--dir");
const RESULTS = dirFlag >= 0 ? process.argv[dirFlag + 1] : path.join(HERE, "results");
const AS_JSON = process.argv.includes("--json");

const files = (await readdir(RESULTS)).filter((f) => f.endsWith(".json"));
const runs = [];
for (const f of files) {
  try {
    runs.push(JSON.parse(await readFile(path.join(RESULTS, f), "utf8")));
  } catch {
    /* skip unreadable or half-written results */
  }
}
runs.sort((a, b) => String(a.started_at).localeCompare(String(b.started_at)));

const pct = (v) => (v === null || v === undefined ? "—" : `${Math.round(v * 100)}%`);
const secs = (ms) => `${(ms / 1000).toFixed(1)}s`;

/** Runs an agent abandoned score zero — giving up is a result, not a gap. */
function scoreOf(run) {
  return run.score?.score ?? 0;
}
function succeeded(run) {
  return run.score?.success === true;
}

function mean(xs) {
  return xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : 0;
}

if (AS_JSON) {
  console.log(JSON.stringify(runs.map((r) => ({ ...r, events: undefined })), null, 2));
  process.exit(0);
}

console.log(`\n${runs.length} run(s) in ${RESULTS}\n`);

console.log("Per run");
console.table(
  runs.map((r) => ({
    task: r.task,
    toolset: r.toolset,
    agent: r.agent,
    score: pct(r.score?.score),
    ok: succeeded(r) ? "yes" : "no",
    trips: r.round_trips,
    shots: r.screenshots,
    zooms: r.zooms,
    acts: r.acts,
    clicks: r.clicks,
    denied: r.denied,
    errors: r.tool_errors,
    tokens: r.visual_tokens,
    wall: secs(r.wall_ms),
  }))
);

const byToolset = new Map();
for (const r of runs) {
  if (!byToolset.has(r.toolset)) byToolset.set(r.toolset, []);
  byToolset.get(r.toolset).push(r);
}

if (byToolset.size > 1) {
  console.log("\nBy toolset");
  console.table(
    [...byToolset.entries()].map(([toolset, rs]) => ({
      toolset,
      runs: rs.length,
      solved: `${rs.filter(succeeded).length}/${rs.length}`,
      mean_score: pct(mean(rs.map(scoreOf))),
      mean_trips: mean(rs.map((r) => r.round_trips)).toFixed(1),
      mean_tokens: Math.round(mean(rs.map((r) => r.visual_tokens))),
      mean_wall: secs(mean(rs.map((r) => r.wall_ms))),
    }))
  );

  // Same task under both toolsets is the only fair comparison; anything else
  // is comparing task difficulty rather than tooling.
  const tasks = [...new Set(runs.map((r) => r.task))];
  const paired = tasks
    .map((task) => {
      const full = runs.filter((r) => r.task === task && r.toolset === "full");
      const basic = runs.filter((r) => r.task === task && r.toolset === "basic");
      if (!full.length || !basic.length) return null;
      return {
        task,
        full_trips: mean(full.map((r) => r.round_trips)).toFixed(1),
        basic_trips: mean(basic.map((r) => r.round_trips)).toFixed(1),
        trip_delta: `${(
          ((mean(basic.map((r) => r.round_trips)) - mean(full.map((r) => r.round_trips))) /
            Math.max(mean(basic.map((r) => r.round_trips)), 1)) *
          100
        ).toFixed(0)}%`,
        full_score: pct(mean(full.map(scoreOf))),
        basic_score: pct(mean(basic.map(scoreOf))),
      };
    })
    .filter(Boolean);

  if (paired.length) {
    console.log("\nHead to head (same task, both toolsets) — trip_delta is what full saves");
    console.table(paired);
  }
}

const solved = runs.filter(succeeded).length;
console.log(
  `\nOverall: ${solved}/${runs.length} solved, mean score ${pct(mean(runs.map(scoreOf)))}, ` +
    `${mean(runs.map((r) => r.round_trips)).toFixed(1)} round trips per run.\n`
);
