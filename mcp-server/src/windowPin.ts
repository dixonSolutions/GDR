/**
 * Pinned window: the per-device default target, plus the window log.
 *
 * ## Why pin at all
 *
 * Driving one app over a long session means repeating the same selector on
 * every call, and every repetition is a chance to typo it and act on the
 * wrong window. A pin says "unless told otherwise, this window" once.
 *
 * ## Where it lives
 *
 * In `~/.config/gdr/config.json` next to the device it belongs to, so it is
 * per-device (pinning an editor on the desktop must not aim at the laptop),
 * survives restarts, and is visible to the Rust CLI, which carries the same
 * field. Pins are stored as *selectors*, not window ids: ids die with the
 * window, and a pin that stops working the moment the app restarts would be
 * worse than no pin at all.
 *
 * ## Logging
 *
 * Pin changes and observed window events both append to
 * `~/.local/share/gdr/window-log.jsonl`, alongside gdrd's own audit log.
 * The point is being able to answer "what was open at 14:30, and what was I
 * pointed at?" after the fact — window ids and titles are gone once the
 * window closes.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

import type { WindowTarget } from "./gdrClient.js";
import { configPath, findDeviceName, loadConfig, saveConfig } from "./config.js";

/** A pin is a selector plus the bookkeeping that makes it explainable. */
export interface PinnedWindow extends WindowTarget {
  /** Index signature so a pin drops straight into the loosely-typed
   *  `HostProfile.pinned_window` slot without a cast at every call site. */
  [key: string]: unknown;
  /** Human name for logs and tool output. Defaults to the window's title. */
  label?: string | null;
  /** Free-text reason, e.g. "the editor I'm driving". */
  note?: string | null;
  /** ISO-8601, when this pin was set. */
  pinned_at: string;
  /** Snapshot of what it resolved to at pin time, for the log. */
  pinned_to?: {
    id: number;
    title: string | null;
    app_id: string | null;
    wm_class: string | null;
  } | null;
}

export type WindowLogKind =
  | "pin_set"
  | "pin_cleared"
  | "window_event"
  | "window_action";

export interface WindowLogEntry {
  ts: string;
  device: string | null;
  kind: WindowLogKind;
  [key: string]: unknown;
}

/** Same directory gdrd writes its audit log to, on the controller side. */
export function windowLogPath(): string {
  return path.join(os.homedir(), ".local", "share", "gdr", "window-log.jsonl");
}

/** Keep the log from growing without bound; one rotation, like the daemon's. */
const MAX_LOG_BYTES = 2 * 1024 * 1024;

export function appendWindowLog(entries: WindowLogEntry[]): void {
  if (entries.length === 0) return;
  const p = windowLogPath();
  try {
    fs.mkdirSync(path.dirname(p), { recursive: true, mode: 0o700 });
    rotateIfLarge(p);
    fs.appendFileSync(p, entries.map((e) => JSON.stringify(e)).join("\n") + "\n", {
      mode: 0o600,
    });
  } catch (e) {
    // Logging must never be the reason a window action fails. Say so on
    // stderr (which MCP hosts surface) and carry on.
    console.error(`[gdr-mcp] window log write failed: ${String(e)}`);
  }
}

function rotateIfLarge(p: string): void {
  let size = 0;
  try {
    size = fs.statSync(p).size;
  } catch {
    return; // no file yet
  }
  if (size < MAX_LOG_BYTES) return;
  try {
    fs.renameSync(p, `${p}.1`);
  } catch {
    /* leave it; appending to an oversized log beats losing the write */
  }
}

export interface WindowLogQuery {
  /** Most recent N entries (default 50). */
  tail?: number;
  device?: string | null;
  kind?: WindowLogKind | null;
  /** Only entries at or after this ISO timestamp. */
  since?: string | null;
}

export function readWindowLog(q: WindowLogQuery = {}): {
  path: string;
  total: number;
  entries: WindowLogEntry[];
} {
  const p = windowLogPath();
  let raw = "";
  try {
    raw = fs.readFileSync(p, "utf8");
  } catch {
    return { path: p, total: 0, entries: [] };
  }
  const all: WindowLogEntry[] = [];
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    try {
      all.push(JSON.parse(line) as WindowLogEntry);
    } catch {
      // A torn last line after a crash should not make the whole log
      // unreadable.
    }
  }
  const filtered = all.filter((e) => {
    if (q.device && e.device !== q.device) return false;
    if (q.kind && e.kind !== q.kind) return false;
    if (q.since && String(e.ts) < q.since) return false;
    return true;
  });
  const tail = q.tail && q.tail > 0 ? q.tail : 50;
  return {
    path: p,
    total: filtered.length,
    entries: filtered.slice(-tail),
  };
}

/** Canonical device id for a query, or throw with the known ids listed. */
export function deviceIdFor(query?: string | null): string {
  const cfg = loadConfig();
  if (query && query.trim()) {
    const id = findDeviceName(cfg, query);
    if (!id) throw new Error(`Unknown device '${query}'.`);
    return id;
  }
  const ids = Object.keys(cfg.hosts);
  const fallback =
    (cfg.default_host && findDeviceName(cfg, cfg.default_host)) ||
    (ids.length === 1 ? ids[0] : undefined);
  if (!fallback) {
    throw new Error(
      "No device specified and no default_host is set — pass dev= or run gdr_device_default."
    );
  }
  return fallback;
}

export function getPin(deviceId: string): PinnedWindow | null {
  const cfg = loadConfig();
  const profile = cfg.hosts[deviceId];
  return (profile?.pinned_window as PinnedWindow | undefined) ?? null;
}

export function setPin(
  deviceId: string,
  pin: PinnedWindow
): { path: string; pin: PinnedWindow } {
  const cfg = loadConfig();
  const profile = cfg.hosts[deviceId];
  if (!profile) throw new Error(`Unknown device '${deviceId}'.`);
  profile.pinned_window = pin;
  saveConfig(cfg);
  appendWindowLog([
    {
      ts: new Date().toISOString(),
      device: deviceId,
      kind: "pin_set",
      pin,
    },
  ]);
  return { path: configPath(), pin };
}

/** Wipe the pin. Returns what was there, so the log and the reply can say. */
export function clearPin(deviceId: string): {
  path: string;
  previous: PinnedWindow | null;
} {
  const cfg = loadConfig();
  const profile = cfg.hosts[deviceId];
  if (!profile) throw new Error(`Unknown device '${deviceId}'.`);
  const previous = (profile.pinned_window as PinnedWindow | undefined) ?? null;
  delete profile.pinned_window;
  saveConfig(cfg);
  appendWindowLog([
    {
      ts: new Date().toISOString(),
      device: deviceId,
      kind: "pin_cleared",
      previous,
    },
  ]);
  return { path: configPath(), previous };
}

/** Strip the bookkeeping fields, leaving a plain wire selector. */
export function pinToTarget(pin: PinnedWindow | null): WindowTarget | null {
  if (!pin) return null;
  const t: WindowTarget = {};
  if (pin.id != null) t.id = pin.id;
  if (pin.app_id) t.app_id = pin.app_id;
  if (pin.wm_class) t.wm_class = pin.wm_class;
  if (pin.title) t.title = pin.title;
  if (pin.pid != null) t.pid = pin.pid;
  if (pin.focused) t.focused = true;
  return t;
}
