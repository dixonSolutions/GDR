/**
 * Window selection and presentation for the MCP tools.
 *
 * Pure functions only — no sockets, no filesystem — because this is where
 * "which window did the agent mean?" gets decided, and getting that wrong
 * silently acts on someone else's editor.
 *
 * The selector semantics mirror `common/src/windows.rs` (`WindowTarget`)
 * deliberately: gdrd resolves selectors too, for the CLI and any other
 * client. The MCP resolves locally first so it can *report* ambiguity with
 * the candidate list, then sends the winning id — the daemon still has the
 * final say if the window vanished in between.
 */

import type { WindowInfo, WindowTarget } from "./gdrClient.js";

/** A selector plus where it came from, for honest reporting in results. */
export interface ResolvedTarget {
  target: WindowTarget;
  source: "explicit" | "pin" | "focused";
}

/**
 * Why a selector did not name exactly one window.
 *
 * `kind` exists because the three cases need different handling and the
 * candidate list alone cannot tell them apart: a `not_found` carries the full
 * window list as context ("here is what *is* open"), which looks identical to
 * an `ambiguous` carrying two matches unless the reason is recorded.
 */
export type SelectionFailure = "empty" | "not_found" | "ambiguous";

export class WindowSelectionError extends Error {
  readonly kind: SelectionFailure;
  readonly candidates: WindowInfo[];

  constructor(kind: SelectionFailure, message: string, candidates: WindowInfo[] = []) {
    super(message);
    this.name = "WindowSelectionError";
    this.kind = kind;
    this.candidates = candidates;
  }
}

function normAppId(s: string): string {
  return s.trim().replace(/\.desktop$/i, "").toLowerCase();
}

export function isEmptyTarget(t: WindowTarget | null | undefined): boolean {
  if (!t) return true;
  return (
    t.id == null &&
    !t.app_id &&
    !t.wm_class &&
    !t.title &&
    t.pid == null &&
    !t.focused
  );
}

/** Every window the selector matches, in the order gdrd listed them. */
export function matchWindows(target: WindowTarget, windows: WindowInfo[]): WindowInfo[] {
  return windows.filter((w) => {
    if (target.id != null && w.id !== target.id) return false;
    if (target.focused && !w.focus) return false;
    if (target.pid != null && w.pid !== target.pid) return false;
    if (target.app_id) {
      if (!w.app_id || normAppId(w.app_id) !== normAppId(target.app_id)) return false;
    }
    if (target.wm_class) {
      const needle = target.wm_class.trim().toLowerCase();
      if (!w.wm_class || !w.wm_class.toLowerCase().includes(needle)) return false;
    }
    if (target.title) {
      const needle = target.title.trim().toLowerCase();
      if (!w.title || !w.title.toLowerCase().includes(needle)) return false;
    }
    return true;
  });
}

/**
 * Pick the single window a selector names.
 *
 * An explicit `id` wins outright — it is the only exact handle. Otherwise
 * multiple matches is an error carrying the candidates, not a coin flip:
 * closing or typing into the wrong window is far worse than one extra round
 * trip. The single deliberate tie-break is the focused window.
 */
export function resolveWindow(target: WindowTarget, windows: WindowInfo[]): WindowInfo {
  if (isEmptyTarget(target)) {
    throw new WindowSelectionError(
      "empty",
      "No window selected. Pass id/app_id/wm_class/title/pid or focused=true, " +
        "or pin a default window with gdr_window_pin."
    );
  }
  if (target.id != null) {
    const hit = windows.find((w) => w.id === target.id);
    if (!hit) {
      throw new WindowSelectionError(
        "not_found",
        `No open window with id ${target.id}. Window ids do not survive the window ` +
          "closing — re-run gdr_windows, or pin by app_id/title instead.",
        windows
      );
    }
    return hit;
  }
  const hits = matchWindows(target, windows);
  if (hits.length === 0) {
    throw new WindowSelectionError(
      "not_found",
      `No open window matches ${describeTarget(target)}.`,
      windows
    );
  }
  if (hits.length === 1) return hits[0];
  const focused = hits.find((w) => w.focus);
  if (focused) return focused;
  throw new WindowSelectionError(
    "ambiguous",
    `${hits.length} windows match ${describeTarget(target)} — pass id=, or narrow ` +
      "the selector. Candidates are listed in `candidates`.",
    hits
  );
}

export function describeTarget(t: WindowTarget): string {
  const parts: string[] = [];
  if (t.id != null) parts.push(`id=${t.id}`);
  if (t.app_id) parts.push(`app_id=${t.app_id}`);
  if (t.wm_class) parts.push(`wm_class~${t.wm_class}`);
  if (t.title) parts.push(`title~${t.title}`);
  if (t.pid != null) parts.push(`pid=${t.pid}`);
  if (t.focused) parts.push("focused");
  return parts.length ? parts.join(" ") : "(no selector)";
}

/** Drop null/undefined/empty fields so a target serializes cleanly. */
export function cleanTarget(t: WindowTarget): WindowTarget {
  const out: WindowTarget = {};
  if (t.id != null) out.id = t.id;
  if (t.app_id) out.app_id = t.app_id;
  if (t.wm_class) out.wm_class = t.wm_class;
  if (t.title) out.title = t.title;
  if (t.pid != null) out.pid = t.pid;
  if (t.focused) out.focused = true;
  return out;
}

/**
 * Decide which selector a tool call should use.
 *
 * An explicit selector always wins over the pin — pinning must never make a
 * spelled-out request act on something else. `focused` is the last resort so
 * that a tool call with no selector and no pin still does the obvious thing
 * on an interactive desktop, and says which it did.
 */
export function chooseTarget(
  explicit: WindowTarget | null | undefined,
  pinned: WindowTarget | null | undefined,
  allowFocusedFallback = true
): ResolvedTarget {
  if (!isEmptyTarget(explicit)) {
    return { target: cleanTarget(explicit as WindowTarget), source: "explicit" };
  }
  if (!isEmptyTarget(pinned)) {
    return { target: cleanTarget(pinned as WindowTarget), source: "pin" };
  }
  if (allowFocusedFallback) {
    return { target: { focused: true }, source: "focused" };
  }
  throw new WindowSelectionError(
    "empty",
    "No window selected and none pinned. Pass a selector or run gdr_window_pin."
  );
}

/** Compact per-window row for tool output — full objects are mostly noise. */
export function windowSummary(w: WindowInfo) {
  return {
    id: w.id,
    app_id: w.app_id,
    wm_class: w.wm_class,
    title: w.title,
    pid: w.pid,
    workspace: w.workspace,
    monitor: w.monitor,
    frame_rect: w.frame_rect,
    state: [
      w.focus ? "focused" : null,
      w.minimized ? "minimized" : null,
      w.fullscreen ? "fullscreen" : null,
      w.maximized && w.maximized !== "none" ? `maximized:${w.maximized}` : null,
      w.above ? "always-on-top" : null,
      w.on_all_workspaces ? "all-workspaces" : null,
      w.on_active_workspace ? null : "other-workspace",
      w.skip_taskbar ? "skip-taskbar" : null,
    ].filter(Boolean),
    /** Whether a screenshot right now would actually show it. */
    capturable: w.stream_region != null && !w.minimized && w.on_active_workspace,
  };
}

/**
 * Why a window cannot be screenshotted as-is, or null when it can.
 *
 * Wayland has no per-window buffer for us: the capture stream carries the
 * composited screen. So "view this window" means "put it on screen first",
 * and the reasons it might not be are worth stating exactly.
 */
export function captureBlocker(w: WindowInfo): string | null {
  if (w.minimized) return "window is minimized";
  if (!w.on_active_workspace) return `window is on workspace ${w.workspace}`;
  if (w.stream_region == null) {
    return `window is on monitor ${w.monitor}, which gdrd is not streaming`;
  }
  return null;
}

/**
 * Selectors to try for a pin, most exact first.
 *
 * A pin records the window id *and* the app/title it belonged to. The id is
 * the fast exact path while the window lives; the app/title selector is what
 * survives the app being restarted, which is the whole reason a pin is worth
 * having. Trying both in order means a pin keeps working across a restart
 * instead of quietly aiming at nothing.
 */
export function pinSelectors(pin: WindowTarget | null | undefined): WindowTarget[] {
  if (!pin) return [];
  const out: WindowTarget[] = [];
  if (pin.id != null) out.push({ id: pin.id });
  const durable = cleanTarget({
    app_id: pin.app_id,
    wm_class: pin.wm_class,
    title: pin.title,
  });
  if (!isEmptyTarget(durable)) out.push(durable);
  // `pid` is deliberately not a durable selector: it dies with the process
  // and gets recycled, so a stale pid can match a completely unrelated app.
  if (out.length === 0 && pin.pid != null) out.push({ pid: pin.pid });
  if (out.length === 0 && pin.focused) out.push({ focused: true });
  return out;
}

export interface PinResolution {
  window: WindowInfo;
  /** Which selector level matched — "id" means the exact window survived. */
  matched: "id" | "selector";
  /** True when the id moved, so the caller can rewrite the stored pin. */
  stale_id: boolean;
}

/**
 * Resolve a pin against the live window list, falling back from id to the
 * durable selector. Throws {@link WindowSelectionError} when nothing matches,
 * with the message naming what was pinned.
 */
export function resolvePin(pin: WindowTarget, windows: WindowInfo[]): PinResolution {
  const selectors = pinSelectors(pin);
  if (selectors.length === 0) {
    throw new WindowSelectionError(
      "empty",
      "The pinned window has no usable selector."
    );
  }
  let lastError: WindowSelectionError | null = null;
  for (const [i, sel] of selectors.entries()) {
    try {
      const window = resolveWindow(sel, windows);
      return {
        window,
        matched: sel.id != null ? "id" : "selector",
        stale_id: i > 0 && pin.id != null,
      };
    } catch (e) {
      // Ambiguity is a real answer, not something to paper over by falling
      // through to a vaguer selector — and it must be told apart from a
      // not-found, which also carries a populated candidate list (the full
      // window list, as context). Only the reason distinguishes them.
      if (e instanceof WindowSelectionError && e.kind === "ambiguous") throw e;
      lastError = e as WindowSelectionError;
    }
  }
  throw new WindowSelectionError(
    "not_found",
    `The pinned window (${describeTarget(pin)}) is not open. ` +
      "Re-pin with gdr_window_pin, clear it with gdr_window_pin({clear:true}), " +
      "or launch the app with gdr_app_launch.",
    lastError?.candidates ?? []
  );
}

export const WINDOW_ACTIONS = [
  "activate",
  "focus",
  "raise",
  "minimize",
  "unminimize",
  "maximize",
  "unmaximize",
  "fullscreen",
  "unfullscreen",
  "above",
  "unabove",
  "stick",
  "unstick",
  "close",
  "move",
  "resize",
  "move_resize",
  "workspace",
] as const;

export type WindowActionName = (typeof WINDOW_ACTIONS)[number];
