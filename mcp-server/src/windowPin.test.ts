/**
 * Pin storage and window log.
 *
 * Both touch real paths under $HOME, so this file redirects HOME to a
 * throwaway directory *before* importing the modules under test — running
 * these against the developer's own ~/.config/gdr/config.json would rewrite
 * their devices and clobber a pin they were using.
 */
import { strict as assert } from "node:assert";
import { after, before, describe, it } from "node:test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

// Type-only, so it is erased at compile time and does not load the module
// before HOME is redirected below.
import type { PinnedWindow } from "./windowPin.js";

const sandbox = fs.mkdtempSync(path.join(os.tmpdir(), "gdr-pin-test-"));
const realHome = process.env.HOME;
process.env.HOME = sandbox;

const { configPath, saveConfig, loadConfig, upsertDevice } = await import("./config.js");
const {
  appendWindowLog,
  clearPin,
  deviceIdFor,
  getPin,
  pinToTarget,
  readWindowLog,
  setPin,
  windowLogPath,
} = await import("./windowPin.js");

function seedConfig(): void {
  saveConfig({
    default_host: "desk",
    hosts: {
      desk: { address: "10.0.0.5", port: 7337, token: "t1", label: "home computer" },
      lap: { address: "10.0.0.6", port: 7337, token: "t2" },
    },
  });
}

function pin(over: Partial<PinnedWindow> = {}): PinnedWindow {
  return {
    id: 42,
    app_id: "org.gnome.TextEditor.desktop",
    title: "notes",
    label: "notes editor",
    pinned_at: "2026-09-07T10:00:00.000Z",
    ...over,
  };
}

before(() => {
  assert.equal(
    path.dirname(path.dirname(configPath())),
    path.join(sandbox, ".config"),
    "tests must not touch the real ~/.config/gdr"
  );
  seedConfig();
});

after(() => {
  process.env.HOME = realHome;
  fs.rmSync(sandbox, { recursive: true, force: true });
});

describe("pin storage", () => {
  it("round-trips through config.json", () => {
    seedConfig();
    setPin("desk", pin());
    const back = getPin("desk");
    assert.equal(back?.id, 42);
    assert.equal(back?.label, "notes editor");
  });

  it("is per device", () => {
    seedConfig();
    setPin("desk", pin());
    assert.equal(getPin("lap"), null, "pinning one device must not aim the other");
  });

  it("clears and reports what was there", () => {
    seedConfig();
    setPin("desk", pin());
    const { previous } = clearPin("desk");
    assert.equal(previous?.id, 42);
    assert.equal(getPin("desk"), null);
    assert.equal(
      "pinned_window" in (loadConfig().hosts.desk as object),
      false,
      "cleared pins are removed, not left as null"
    );
  });

  it("rejects an unknown device instead of writing a phantom profile", () => {
    seedConfig();
    assert.throws(() => setPin("nope", pin()), /Unknown device/);
  });

  it("survives gdr_device_add rewriting the profile", () => {
    // upsertDevice rebuilds the profile object wholesale, so this guards the
    // regression where updating a device's label silently unpinned it.
    seedConfig();
    setPin("desk", pin());
    upsertDevice({ id: "desk", label: "renamed" });
    assert.equal(getPin("desk")?.id, 42);
    assert.equal(loadConfig().hosts.desk.label, "renamed");
  });

  it("keeps secrets and pin in the same 0600 file", () => {
    seedConfig();
    setPin("desk", pin());
    assert.equal(fs.statSync(configPath()).mode & 0o777, 0o600);
  });
});

describe("deviceIdFor", () => {
  before(seedConfig);

  it("resolves by id, label and default_host", () => {
    assert.equal(deviceIdFor("desk"), "desk");
    assert.equal(deviceIdFor("home computer"), "desk");
    assert.equal(deviceIdFor(null), "desk");
  });

  it("rejects an unknown name", () => {
    assert.throws(() => deviceIdFor("ghost"), /Unknown device/);
  });
});

describe("pinToTarget", () => {
  it("strips the bookkeeping fields from the wire selector", () => {
    assert.deepEqual(pinToTarget(pin()), {
      id: 42,
      app_id: "org.gnome.TextEditor.desktop",
      title: "notes",
    });
  });

  it("maps a missing pin to null", () => {
    assert.equal(pinToTarget(null), null);
  });
});

describe("window log", () => {
  it("records pin changes without being asked", () => {
    seedConfig();
    fs.rmSync(windowLogPath(), { force: true });
    setPin("desk", pin());
    clearPin("desk");
    const kinds = readWindowLog({ device: "desk" }).entries.map((e) => e.kind);
    assert.deepEqual(kinds, ["pin_set", "pin_cleared"]);
  });

  it("filters by device, kind and time, and tails", () => {
    fs.rmSync(windowLogPath(), { force: true });
    appendWindowLog([
      { ts: "2026-09-01T00:00:00.000Z", device: "desk", kind: "window_event", event: "opened" },
      { ts: "2026-09-02T00:00:00.000Z", device: "lap", kind: "window_event", event: "opened" },
      { ts: "2026-09-03T00:00:00.000Z", device: "desk", kind: "window_action", action: "close" },
    ]);
    assert.equal(readWindowLog({ device: "desk" }).total, 2);
    assert.equal(readWindowLog({ kind: "window_action" }).total, 1);
    assert.equal(readWindowLog({ since: "2026-09-02T00:00:00.000Z" }).total, 2);
    const tailed = readWindowLog({ tail: 1 });
    assert.equal(tailed.entries.length, 1);
    assert.equal(tailed.total, 3, "total counts matches, not the tail");
    assert.equal(tailed.entries[0].kind, "window_action", "tail keeps the newest");
  });

  it("survives a torn line rather than losing the whole log", () => {
    fs.rmSync(windowLogPath(), { force: true });
    appendWindowLog([
      { ts: "2026-09-01T00:00:00.000Z", device: "desk", kind: "window_event" },
    ]);
    fs.appendFileSync(windowLogPath(), '{"ts":"2026-09-02T00:00:0\n');
    appendWindowLog([
      { ts: "2026-09-03T00:00:00.000Z", device: "desk", kind: "window_event" },
    ]);
    assert.equal(readWindowLog({}).total, 2);
  });

  it("reads as empty before anything is logged", () => {
    fs.rmSync(windowLogPath(), { force: true });
    const r = readWindowLog({});
    assert.deepEqual(r.entries, []);
    assert.equal(r.total, 0);
  });
});
