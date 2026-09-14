// Shared plumbing for benchmark task pages.
//
// Tasks score themselves and report the verdict to the harness. An agent is
// never asked whether it succeeded — self-reported success is exactly the
// thing a benchmark cannot afford to trust.

/**
 * Deterministic PRNG (mulberry32).
 *
 * Layouts are seeded so every agent and every ablation run faces an identical
 * board. Without this, comparing two runs would be comparing two different
 * tasks.
 */
export function rng(seed) {
  let a = seed >>> 0;
  return function next() {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Seed from the URL so the runner can vary boards without editing pages. */
export function seedFromUrl(fallback = 7) {
  const raw = new URLSearchParams(location.search).get("seed");
  const n = Number(raw);
  return Number.isFinite(n) && raw !== null ? n : fallback;
}

/**
 * Fire-and-forget event report.
 *
 * Deliberately not awaited by callers and deliberately swallowing failures: a
 * harness that is down must not change how the task behaves, or the benchmark
 * would be measuring the harness.
 */
export function report(payload) {
  try {
    const body = JSON.stringify(payload);
    if (navigator.sendBeacon) {
      navigator.sendBeacon("/api/event", new Blob([body], { type: "application/json" }));
      return;
    }
    fetch("/api/event", { method: "POST", headers: { "content-type": "application/json" }, body });
  } catch {
    /* harness offline; the page still works standalone */
  }
}

/** Announce the task and its parameters at load. */
export function start(task, detail = {}) {
  report({ type: "start", task, detail });
}

/**
 * Record the final verdict and show it on screen.
 *
 * The banner matters as much as the report: an agent needs a visible signal
 * that it is finished, or it will keep acting and inflate the round-trip count
 * of a task it already solved.
 */
export function complete(task, { success, score, detail }) {
  report({ type: "complete", task, success, score, detail });
  const el = document.querySelector(".done-banner");
  if (el) {
    el.textContent = success
      ? `TASK COMPLETE — score ${Math.round((score ?? 1) * 100)}%`
      : `TASK FAILED — score ${Math.round((score ?? 0) * 100)}%`;
    el.classList.add("show");
    el.scrollIntoView({ block: "center" });
  }
  window.__taskResult = { success, score, detail };
}

/** Progress ping, used to reconstruct how a run unfolded over time. */
export function progress(task, detail) {
  report({ type: "progress", task, detail });
}

/** Expose state for verification without giving the agent DOM access. */
export function publish(state) {
  window.__task = state;
}
