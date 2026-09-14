import { strict as assert } from "node:assert";
import { describe, it } from "node:test";

import type { WindowInfo, WindowTarget } from "./gdrClient.js";
import {
  captureBlocker,
  chooseTarget,
  cleanTarget,
  isEmptyTarget,
  matchWindows,
  pinSelectors,
  resolvePin,
  resolveWindow,
  windowSummary,
  WindowSelectionError,
} from "./windows.js";

/**
 * `assert.throws` returns undefined, so grab the error the plain way when a
 * test needs to inspect its fields.
 */
function caught(fn: () => unknown): WindowSelectionError {
  try {
    fn();
  } catch (e) {
    return e as WindowSelectionError;
  }
  throw new Error("expected a WindowSelectionError, nothing was thrown");
}

function win(over: Partial<WindowInfo> & { id: number }): WindowInfo {
  return {
    title: "Untitled",
    wm_class: "app",
    app_id: "app.desktop",
    pid: 1000 + over.id,
    window_type: "normal",
    frame_rect: { x: 0, y: 0, width: 800, height: 600 },
    stream_region: { x: 0, y: 0, width: 1000, height: 750 },
    monitor: 0,
    connector: "eDP-1",
    workspace: 0,
    on_active_workspace: true,
    minimized: false,
    maximized: "none",
    fullscreen: false,
    focus: false,
    above: false,
    on_all_workspaces: false,
    skip_taskbar: false,
    can_close: true,
    ...over,
  };
}

const NAUTILUS = win({
  id: 1,
  app_id: "org.gnome.Nautilus.desktop",
  wm_class: "org.gnome.Nautilus",
  title: "Home",
});
const FF_GDR = win({
  id: 2,
  app_id: "firefox.desktop",
  wm_class: "firefox",
  title: "gdr — Mozilla Firefox",
});
const FF_DOCS = win({
  id: 3,
  app_id: "firefox.desktop",
  wm_class: "firefox",
  title: "docs — Mozilla Firefox",
});
const ALL = [NAUTILUS, FF_GDR, FF_DOCS];

describe("selector matching", () => {
  it("treats app_id as exact, ignoring case and the .desktop suffix", () => {
    assert.deepEqual(
      matchWindows({ app_id: "ORG.GNOME.NAUTILUS" }, ALL).map((w) => w.id),
      [1]
    );
    // Exact, not substring: "fire" must not silently match "firefox", or a
    // half-typed selector would act on a window the caller never named.
    assert.deepEqual(matchWindows({ app_id: "fire" }, ALL), []);
  });

  it("treats title and wm_class as substrings", () => {
    assert.deepEqual(matchWindows({ title: "docs" }, ALL).map((w) => w.id), [3]);
    assert.deepEqual(
      matchWindows({ wm_class: "fire" }, ALL).map((w) => w.id),
      [2, 3]
    );
  });

  it("combines fields with AND", () => {
    assert.deepEqual(
      matchWindows({ app_id: "firefox", title: "gdr" }, ALL).map((w) => w.id),
      [2]
    );
  });

  it("recognises an empty selector", () => {
    assert.equal(isEmptyTarget(undefined), true);
    assert.equal(isEmptyTarget({}), true);
    assert.equal(isEmptyTarget({ title: "" }), true);
    assert.equal(isEmptyTarget({ id: 0 }), false, "id 0 is still a selector");
    assert.equal(isEmptyTarget({ focused: true }), false);
  });
});

describe("resolveWindow", () => {
  it("refuses to guess with no selector", () => {
    assert.throws(() => resolveWindow({}, ALL), WindowSelectionError);
  });

  it("resolves an exact id", () => {
    assert.equal(resolveWindow({ id: 3 }, ALL).id, 3);
  });

  it("explains a dead id rather than falling back", () => {
    // Falling back to "something similar" is how you close the wrong tab.
    const e = caught(() => resolveWindow({ id: 99 }, ALL));
    assert.equal(e.kind, "not_found");
    assert.match(e.message, /id 99/);
    assert.match(e.message, /do not survive/);
  });

  it("reports ambiguity with the candidates attached", () => {
    const e = caught(() => resolveWindow({ app_id: "firefox" }, ALL));
    assert.equal(e.kind, "ambiguous");
    assert.match(e.message, /2 windows match/);
    assert.deepEqual(e.candidates.map((w) => w.id), [2, 3]);
  });

  it("breaks a tie on focus, deliberately", () => {
    const windows = [NAUTILUS, FF_GDR, { ...FF_DOCS, focus: true }];
    assert.equal(resolveWindow({ app_id: "firefox" }, windows).id, 3);
  });
});

describe("chooseTarget", () => {
  const pin: WindowTarget = { app_id: "firefox.desktop" };

  it("lets an explicit selector beat the pin", () => {
    const r = chooseTarget({ id: 1 }, pin);
    assert.equal(r.source, "explicit");
    assert.deepEqual(r.target, { id: 1 });
  });

  it("uses the pin when nothing was passed", () => {
    const r = chooseTarget({}, pin);
    assert.equal(r.source, "pin");
    assert.deepEqual(r.target, { app_id: "firefox.desktop" });
  });

  it("falls back to the focused window, and says so", () => {
    const r = chooseTarget({}, null);
    assert.equal(r.source, "focused");
    assert.deepEqual(r.target, { focused: true });
  });

  it("can refuse the focus fallback", () => {
    assert.throws(() => chooseTarget({}, null, false), WindowSelectionError);
  });
});

describe("pins survive the app restarting", () => {
  const pin: WindowTarget = {
    id: 2,
    app_id: "firefox.desktop",
    title: "gdr",
  };

  it("tries the exact id first", () => {
    assert.deepEqual(pinSelectors(pin), [
      { id: 2 },
      { app_id: "firefox.desktop", title: "gdr" },
    ]);
    const r = resolvePin(pin, ALL);
    assert.equal(r.window.id, 2);
    assert.equal(r.matched, "id");
    assert.equal(r.stale_id, false);
  });

  it("falls back to app/title when the id is gone, and flags the stale id", () => {
    // The whole point of a pin: Firefox restarted, so id 2 no longer exists,
    // but the pin should still find the window it means.
    const restarted = [NAUTILUS, win({ ...FF_GDR, id: 77 })];
    const r = resolvePin(pin, restarted);
    assert.equal(r.window.id, 77);
    assert.equal(r.matched, "selector");
    assert.equal(r.stale_id, true, "caller should rewrite the stored id");
  });

  it("never falls back to pid, which gets recycled", () => {
    assert.deepEqual(pinSelectors({ id: 5, pid: 4242 }), [{ id: 5 }]);
  });

  it("surfaces ambiguity at the durable level instead of hiding it", () => {
    const broad: WindowTarget = { id: 999, app_id: "firefox.desktop" };
    const e = caught(() => resolvePin(broad, ALL));
    assert.match(e.message, /2 windows match/);
  });

  it("says the pinned window is not open when nothing matches", () => {
    const e = caught(() => resolvePin({ id: 42, app_id: "zed.desktop" }, ALL));
    assert.match(e.message, /not open/);
    assert.match(e.message, /gdr_window_pin/);
  });
});

describe("capture readiness", () => {
  it("passes a normal on-screen window", () => {
    assert.equal(captureBlocker(NAUTILUS), null);
    assert.equal(windowSummary(NAUTILUS).capturable, true);
  });

  it("names each reason a window cannot be captured", () => {
    assert.match(captureBlocker(win({ id: 9, minimized: true }))!, /minimized/);
    assert.match(
      captureBlocker(win({ id: 9, on_active_workspace: false, workspace: 3 }))!,
      /workspace 3/
    );
    // Wrong monitor: gdrd could not compute a crop, so it sent no region.
    assert.match(
      captureBlocker(win({ id: 9, monitor: 1, stream_region: null }))!,
      /not streaming/
    );
  });
});

describe("windowSummary", () => {
  it("collapses booleans into a readable state list", () => {
    const s = windowSummary(
      win({ id: 4, focus: true, maximized: "both", on_active_workspace: false, workspace: 2 })
    );
    assert.deepEqual(s.state, ["focused", "maximized:both", "other-workspace"]);
    assert.equal(s.capturable, false);
  });

  it("keeps quiet about a plain window", () => {
    assert.deepEqual(windowSummary(NAUTILUS).state, []);
  });
});

describe("cleanTarget", () => {
  it("drops empty and null fields so the wire selector stays minimal", () => {
    assert.deepEqual(
      cleanTarget({ id: null, app_id: "", title: "x", focused: false, pid: null }),
      { title: "x" }
    );
  });
});
