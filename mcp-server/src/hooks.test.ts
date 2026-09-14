import { strict as assert } from "node:assert";
import { describe, it } from "node:test";

import type { ActivityReport, HookStatus, WindowEventInfo } from "./gdrClient.js";
import type { FrameGeometry } from "./screenshotLayout.js";
import {
  activitySpec,
  activitySummary,
  hookEventSummary,
  hookSummary,
  pollNote,
  windowEventSummary,
  windowSpec,
} from "./hooks.js";

/** A 1920x1200 desktop shown to the agent at 1400x875. */
const FRAME: FrameGeometry = {
  native_width: 1920,
  native_height: 1200,
  image_width: 1400,
  image_height: 875,
  region: { x: 0, y: 0, width: 1920, height: 1200 },
};

function report(over: Partial<ActivityReport> = {}): ActivityReport {
  return {
    circle: { x: 960, y: 600, radius: 100 },
    bbox: { x: 890, y: 530, width: 140, height: 140 },
    started_at: "2026-09-14T10:00:00Z",
    ended_at: "2026-09-14T10:00:01Z",
    duration_ms: 1000,
    buffer_ms: 400,
    samples: 8,
    changed_fraction: 0.0125,
    area: { scope: "screen" },
    stream_width: 1920,
    stream_height: 1200,
    settled: true,
    session_locked: false,
    ...over,
  };
}

function windowInfo(over: Partial<WindowEventInfo> = {}): WindowEventInfo {
  return {
    id: 42,
    title: "New Tab - Chromium",
    app_id: "chromium.desktop",
    wm_class: "chromium",
    pid: 4242,
    process: {
      pid: 4242,
      uid: 1000,
      user: "eva",
      comm: "chromium",
      exe: "/usr/lib/chromium/chromium",
      cmdline: "/usr/lib/chromium/chromium --new-window",
      ppid: 1,
      error: null,
    },
    window_type: "normal",
    frame_rect: { x: 100, y: 50, width: 1024, height: 768 },
    stream_region: { x: 125, y: 62, width: 1280, height: 960 },
    monitor: 0,
    workspace: 0,
    minimized: false,
    focus: true,
    maximized: "none",
    fullscreen: false,
    previous: null,
    samples: 3,
    settled: true,
    ...over,
  };
}

function status(over: Partial<HookStatus> = {}): HookStatus {
  return {
    id: "window-1",
    kind: "window",
    label: null,
    enabled: true,
    state: "watching",
    spec: { kind: "window", buffer_ms: 250 },
    summary: "opened/closed/resized on any window, buffer 250ms",
    created_at: "2026-09-14T09:00:00Z",
    updated_at: "2026-09-14T09:00:00Z",
    required_scope: "window",
    created_with_scopes: ["screenshot", "window"],
    created_by: "laptop",
    events_emitted: 0,
    last_event_at: null,
    last_error: null,
    buffered: 0,
    ...over,
  };
}

describe("hook specs", () => {
  it("drops an empty selector rather than filtering on nothing", () => {
    const spec = windowSpec({ target: { id: undefined, title: undefined } });
    assert.equal(spec.kind, "window");
    assert.equal((spec as { target: unknown }).target, null);
  });

  it("keeps the selector fields that were actually given", () => {
    const spec = activitySpec({ target: { title: "Inbox", app_id: "" } });
    assert.deepEqual((spec as { target: unknown }).target, { title: "Inbox" });
  });

  it("a window filter wins over a rectangle, instead of storing both", () => {
    // The daemon ignores `region` when a window is named; a spec that still
    // carried one would describe a watch that does not exist.
    const spec = activitySpec({
      target: { title: "Inbox" },
      region: { x: 0, y: 0, width: 10, height: 10 },
    });
    assert.equal((spec as { region: unknown }).region, null);
  });

  it("passes a bare rectangle through untouched", () => {
    const region = { x: 10, y: 20, width: 300, height: 200 };
    const spec = activitySpec({ region });
    assert.deepEqual((spec as { region: unknown }).region, region);
  });
});

describe("activity reports", () => {
  it("gives the circle in screenshot coordinates as well as stream ones", () => {
    const summary = activitySummary(report(), FRAME);
    assert.deepEqual(summary.circle, { x: 960, y: 600, radius: 100, space: "stream" });
    // 1400/1920 = 0.729…
    const shot = summary.circle_in_last_screenshot as {
      x: number;
      y: number;
      radius: number;
    };
    assert.equal(shot.x, 700);
    assert.equal(Math.round(shot.y), 438);
    assert.equal(Math.round(shot.radius), 73);
  });

  it("says so rather than guessing when there is no screenshot", () => {
    const summary = activitySummary(report());
    const shot = summary.circle_in_last_screenshot as { note: string; x?: number };
    assert.equal(shot.x, undefined);
    assert.match(shot.note, /no screenshot yet/);
  });

  it("says so when the circle is outside the frame the agent looked at", () => {
    const zoom: FrameGeometry = {
      native_width: 1920,
      native_height: 1200,
      image_width: 400,
      image_height: 300,
      region: { x: 0, y: 0, width: 400, height: 300 },
    };
    const shot = activitySummary(report(), zoom).circle_in_last_screenshot as {
      note: string;
      x?: number;
    };
    assert.equal(shot.x, undefined);
    assert.match(shot.note, /outside/);
  });

  it("hands over a zoom call that needs no prior screenshot", () => {
    // The follow-up step used to be impossible on the intended path: the
    // circle is in stream pixels and gdr_zoom defaulted to image pixels,
    // which only exist once you have taken the screenshot the hook is there
    // to avoid.
    const { look_here } = activitySummary(report());
    assert.equal(look_here.args.space, "stream");
    // The measured box plus a small pad — not a square around the circle,
    // which would hand over ~2.4x the area the measurement covers.
    assert.equal(look_here.args.x, 874);
    assert.equal(look_here.args.y, 514);
    assert.equal(look_here.args.width, 172);
    assert.equal(look_here.args.height, 172);
    const r = report();
    const suggested = look_here.args.width * look_here.args.height;
    const measured = r.bbox.width * r.bbox.height;
    assert.ok(suggested < measured * 2, `${suggested} vs ${measured}`);
  });

  it("clamps the suggested zoom to the stream it came from", () => {
    const corner = activitySummary(
      report({
        circle: { x: 1900, y: 1190, radius: 300 },
        stream_width: 1920,
        stream_height: 1200,
      })
    );
    const a = corner.look_here.args;
    assert.ok(a.x >= 0 && a.y >= 0);
    assert.ok(a.x + a.width <= 1920, `${a.x}+${a.width}`);
    assert.ok(a.y + a.height <= 1200, `${a.y}+${a.height}`);
  });

  it("says outright when the activity is on a lock screen", () => {
    // Every circle on a locked session is the lock clock. An agent that is
    // not told this tunes its hook against a shield for as long as it has
    // patience for.
    const locked = activitySummary(report({ session_locked: true }));
    assert.equal(locked.session_locked, true);
    assert.match(String(locked.warning), /LOCKED/);
    assert.equal(activitySummary(report()).warning, undefined);
  });

  it("warns loudly when a burst was cut off while still moving", () => {
    const summary = activitySummary(report({ settled: false }), FRAME);
    assert.equal(summary.settled, false);
    assert.match(String(summary.warning), /still moving/);
  });

  it("carries no warning when the burst settled", () => {
    assert.equal(activitySummary(report(), FRAME).warning, undefined);
  });
});

describe("window events", () => {
  it("reports title, size and the owning process", () => {
    const summary = windowEventSummary(windowInfo());
    assert.deepEqual(summary.size, { width: 1024, height: 768 });
    assert.deepEqual(summary.position, { x: 100, y: 50 });
    assert.equal(summary.process.user, "eva");
    assert.equal(summary.process.command, "chromium");
    assert.equal(summary.process.exe, "/usr/lib/chromium/chromium");
  });

  it("does not pretend to know the process when the lookup was turned off", () => {
    const summary = windowEventSummary(windowInfo({ process: null }));
    assert.equal(summary.process.pid, 4242);
    assert.match(String(summary.process.note), /turned off/);
    assert.equal(summary.process.user, null);
  });

  it("passes the failure reason through when /proc could not answer", () => {
    const summary = windowEventSummary(
      windowInfo({
        process: {
          pid: 4242,
          uid: null,
          user: null,
          comm: null,
          exe: null,
          cmdline: null,
          ppid: null,
          error: "process 4242 is gone",
        },
      })
    );
    assert.equal(summary.process.error, "process 4242 is gone");
  });

  it("keeps the previous geometry on a resize, so the delta is readable", () => {
    const summary = windowEventSummary(
      windowInfo({
        previous: {
          frame_rect: { x: 100, y: 50, width: 800, height: 600 },
          title: null,
          minimized: null,
          workspace: null,
          dw: 224,
          dh: 168,
          dx: null,
          dy: null,
        },
      })
    );
    assert.equal(summary.previous?.dw, 224);
  });
});

describe("hook summaries", () => {
  it("surfaces the scope the hook needs and who created it", () => {
    const summary = hookSummary(status());
    assert.equal(summary.scopes.required, "window");
    assert.deepEqual(summary.scopes.created_with, ["screenshot", "window"]);
    assert.equal(summary.scopes.created_by, "laptop");
  });

  it("echoes the effective config, so a reconfigure can be verified", () => {
    // Only buffer_ms used to come back, which made grid/threshold changes
    // unverifiable — and the daemon clamps what it is given, so the value in
    // effect is not necessarily the value that was sent.
    const summary = hookSummary(
      status({ spec: { kind: "activity", buffer_ms: 300, grid: 256, threshold: 3 } })
    );
    assert.deepEqual(summary.config, {
      kind: "activity",
      buffer_ms: 300,
      grid: 256,
      threshold: 3,
    });
  });

  it("keeps a disabled hook's state visible", () => {
    const summary = hookSummary(status({ enabled: false, state: "paused" }));
    assert.equal(summary.enabled, false);
    assert.equal(summary.state, "paused");
  });
});

describe("empty polls explain themselves", () => {
  it("distinguishes 'nothing happened' from 'nothing is watching'", () => {
    assert.match(pollNote({ events: [], dropped: false, hooks: [] }), /No hooks exist/);
    assert.match(
      pollNote({ events: [], dropped: false, hooks: [status({ enabled: false })] }),
      /switched off/
    );
    assert.match(
      pollNote({ events: [], dropped: false, hooks: [status()] }),
      /Nothing happened yet/
    );
  });

  it("names the reason a watcher has not started", () => {
    const note = pollNote({
      events: [],
      dropped: false,
      hooks: [status({ state: "waiting", last_error: "no capture stream yet" })],
    });
    assert.match(note, /waiting/);
    assert.match(note, /no capture stream yet/);
  });

  it("tells a caller who fell behind what to change", () => {
    assert.match(pollNote({ events: [], dropped: true, hooks: [status()] }), /poll more often/);
  });

  it("warns first when a filtered cursor stepped over another hook", () => {
    // The silent one: sequence numbers are global, so reusing a filtered
    // cursor on an unfiltered poll used to lose events outright.
    const note = pollNote({
      events: [{}],
      dropped: false,
      hooks: [status()],
      cursor_scope: "activity-4",
      skipped_other_hooks: 2,
    });
    assert.match(note, /activity-4 ONLY/);
    assert.match(note, /still waiting/);
  });
});

describe("event summaries", () => {
  it("carries the hook id and label so a multi-hook drain is readable", () => {
    const summary = hookEventSummary(
      {
        seq: 7,
        hook_id: "window-1",
        hook_kind: "window",
        label: "chromium watch",
        kind: "opened",
        at: "2026-09-14T10:00:00Z",
        activity: null,
        window: windowInfo(),
      },
      FRAME
    );
    assert.equal(summary.seq, 7);
    assert.equal(summary.hook, "window-1");
    assert.equal(summary.label, "chromium watch");
    assert.equal(summary.window?.title, "New Tab - Chromium");
    assert.equal(summary.activity, undefined);
  });
});
