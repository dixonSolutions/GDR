// Subscription hooks, agent side: turn tool arguments into hook specs, and
// turn hook events back into something an agent can act on.
//
// The daemon reports activity in capture-stream pixels, which is the space
// `Region` and `MouseMove` use — not the space of the image the agent looked
// at. Everything here that produces coordinates therefore also offers them in
// image space when a screenshot for that device is known, because "the screen
// moved at (1840, 96)" is not directly clickable and "at (1470, 76) in your
// last screenshot" is.

import type {
  ActivityReport,
  HookEvent,
  HookSpec,
  HookStatus,
  Region,
  WindowEventInfo,
  WindowTarget,
} from "./gdrClient.js";
import { toImageCoords, type FrameGeometry } from "./screenshotLayout.js";

/** Window lifecycle events a hook can subscribe to. Mirrors WINDOW_EVENT_KINDS. */
export const WINDOW_HOOK_EVENTS = [
  "opened",
  "closed",
  "resized",
  "moved",
  "retitled",
  "focused",
  "minimized",
  "unminimized",
  "workspace",
] as const;

export type WindowHookEvent = (typeof WINDOW_HOOK_EVENTS)[number];

export interface ActivityArgs {
  target?: WindowTarget | null;
  region?: Region | null;
  buffer_ms?: number;
  min_interval_ms?: number;
  max_burst_ms?: number;
  poll_ms?: number;
  threshold?: number;
  min_cells?: number;
  grid?: number;
  max_radius?: number;
}

export interface WindowArgs {
  target?: WindowTarget | null;
  events?: string[];
  buffer_ms?: number;
  max_burst_ms?: number;
  poll_ms?: number;
  include_skip_taskbar?: boolean;
  include_process?: boolean;
  geometry_threshold?: number;
}

/** Drop empty selectors so the daemon sees "no filter", not "filter on null". */
function tidyTarget(target?: WindowTarget | null): WindowTarget | null {
  if (!target) return null;
  const out: WindowTarget = {};
  if (target.id != null) out.id = target.id;
  if (target.app_id) out.app_id = target.app_id;
  if (target.wm_class) out.wm_class = target.wm_class;
  if (target.title) out.title = target.title;
  if (target.pid != null) out.pid = target.pid;
  if (target.focused) out.focused = true;
  return Object.keys(out).length ? out : null;
}

export function activitySpec(args: ActivityArgs): HookSpec {
  const target = tidyTarget(args.target);
  return {
    kind: "activity",
    target,
    // A window filter and a fixed rectangle answer the same question two
    // ways, and the daemon ignores the rectangle when a window is named.
    // Dropping it here keeps the stored spec from claiming otherwise.
    region: target ? null : (args.region ?? null),
    buffer_ms: args.buffer_ms,
    min_interval_ms: args.min_interval_ms,
    max_burst_ms: args.max_burst_ms,
    poll_ms: args.poll_ms,
    threshold: args.threshold,
    min_cells: args.min_cells,
    grid: args.grid,
    max_radius: args.max_radius,
  };
}

export function windowSpec(args: WindowArgs): HookSpec {
  return {
    kind: "window",
    target: tidyTarget(args.target),
    events: args.events?.length ? args.events : undefined,
    buffer_ms: args.buffer_ms,
    max_burst_ms: args.max_burst_ms,
    poll_ms: args.poll_ms,
    include_skip_taskbar: args.include_skip_taskbar,
    include_process: args.include_process,
    geometry_threshold: args.geometry_threshold,
  };
}

/** Compact view of a hook — everything an agent needs to decide about it. */
export function hookSummary(hook: HookStatus) {
  return {
    id: hook.id,
    kind: hook.kind,
    label: hook.label,
    enabled: hook.enabled,
    state: hook.state,
    watching: hook.summary,
    buffer_ms: (hook.spec as { buffer_ms?: number }).buffer_ms,
    // The whole spec as the daemon holds it, not just the field that
    // happened to be interesting. Two reasons: a reconfigure is otherwise
    // unverifiable — you cannot tell whether your `grid` or `threshold`
    // landed — and the daemon *clamps* what it is given, so the effective
    // value can legitimately differ from the one you sent.
    config: hook.spec,
    events_emitted: hook.events_emitted,
    buffered: hook.buffered,
    last_event_at: hook.last_event_at,
    last_error: hook.last_error,
    scopes: {
      required: hook.required_scope,
      // The daemon only returns hooks this token may see, so anything
      // listed here is already readable with the token in hand. This block
      // is for auditing *which* authority created it, which is the question
      // that outlives the token.
      created_with: hook.created_with_scopes,
      created_by: hook.created_by,
    },
    created_at: hook.created_at,
  };
}

/** Where an activity burst happened, in both coordinate spaces. */
function activityWhere(report: ActivityReport, frame?: FrameGeometry) {
  const image = frame
    ? toImageCoords(report.circle.x, report.circle.y, frame)
    : null;
  // Radius scales with the crop's downscale factor; both axes share it
  // whenever aspect is preserved, which every sizing profile does.
  const scale = frame ? frame.image_width / frame.region.width : 1;
  return {
    circle: {
      x: Math.round(report.circle.x),
      y: Math.round(report.circle.y),
      radius: Math.round(report.circle.radius),
      space: "stream",
    },
    circle_in_last_screenshot: image
      ? {
          x: image.x,
          y: image.y,
          radius: Math.round(report.circle.radius * scale * 10) / 10,
          note: "gdr_click takes these coordinates.",
        }
      : frame
        ? { note: "outside the area your last screenshot covers." }
        : { note: "no screenshot yet for this device, so no image coordinates." },
    bbox: report.bbox,
  };
}

/**
 * The `gdr_zoom` call that looks at what this report is pointing at.
 *
 * Handed over pre-computed because the obvious next step was, for a while,
 * impossible: the circle is in stream pixels, `gdr_zoom` defaulted to the
 * last screenshot's image pixels, and the whole point of a hook is not
 * having taken a screenshot. A square around the circle in stream space is
 * the call that always works.
 */
function zoomFor(report: ActivityReport) {
  // Built from the bbox, not the circle. The circle is a lossy re-encoding
  // of this rectangle — its radius is the box's half-diagonal — so squaring
  // it back up hands over ~2.4x the area the measurement actually covers,
  // which for small text is resolution and tokens spent on nothing. A small
  // pad because a cell that changed means "something in here moved" and the
  // glyph may run a few pixels past it.
  const pad = 16;
  const x = Math.max(0, Math.round(report.bbox.x - pad));
  const y = Math.max(0, Math.round(report.bbox.y - pad));
  return {
    tool: "gdr_zoom",
    args: {
      space: "stream" as const,
      x,
      y,
      width: Math.min(report.stream_width - x, Math.round(report.bbox.width) + pad * 2),
      height: Math.min(report.stream_height - y, Math.round(report.bbox.height) + pad * 2),
    },
  };
}

export function activitySummary(report: ActivityReport, frame?: FrameGeometry) {
  return {
    ...activityWhere(report, frame),
    look_here: zoomFor(report),
    area: report.area,
    duration_ms: report.duration_ms,
    buffer_ms: report.buffer_ms,
    samples: report.samples,
    changed_fraction: Math.round(report.changed_fraction * 1000) / 1000,
    stream: { width: report.stream_width, height: report.stream_height },
    settled: report.settled,
    session_locked: report.session_locked ?? null,
    ...(report.session_locked
      ? {
          warning:
            "The session is LOCKED. What is on the capture stream is the lock screen, " +
            "not the desktop — activity here is the lock clock, not the app you are " +
            "watching. Ask the user to unlock; nothing behind the shield is visible " +
            "or reachable until they do.",
        }
      : {}),
    ...(report.settled
      ? {}
      : {
          warning:
            "This burst was still moving when the max_burst_ms cap fired, so the " +
            "screen may already differ from this report. Re-screenshot before acting.",
        }),
  };
}

export function windowEventSummary(info: WindowEventInfo) {
  return {
    id: info.id,
    title: info.title,
    app_id: info.app_id,
    wm_class: info.wm_class,
    size: { width: info.frame_rect.width, height: info.frame_rect.height },
    position: { x: info.frame_rect.x, y: info.frame_rect.y },
    space: "logical",
    stream_region: info.stream_region,
    monitor: info.monitor,
    workspace: info.workspace,
    minimized: info.minimized,
    focus: info.focus,
    maximized: info.maximized,
    fullscreen: info.fullscreen,
    // One shape whether or not the lookup ran: a field that sometimes exists
    // and sometimes does not is the kind of thing a caller stops checking.
    process: {
      pid: info.process?.pid ?? info.pid,
      user: info.process?.user ?? null,
      uid: info.process?.uid ?? null,
      command: info.process?.comm ?? null,
      exe: info.process?.exe ?? null,
      cmdline: info.process?.cmdline ?? null,
      ppid: info.process?.ppid ?? null,
      error: info.process?.error ?? null,
      ...(info.process
        ? {}
        : { note: "process lookup was turned off for this hook." }),
    },
    previous: info.previous,
    samples: info.samples,
    settled: info.settled,
  };
}

export function hookEventSummary(event: HookEvent, frame?: FrameGeometry) {
  return {
    seq: event.seq,
    hook: event.hook_id,
    label: event.label,
    kind: event.kind,
    at: event.at,
    ...(event.activity ? { activity: activitySummary(event.activity, frame) } : {}),
    ...(event.window ? { window: windowEventSummary(event.window) } : {}),
  };
}

/**
 * The sentence that tells an agent what to do with an empty poll.
 *
 * A subscription that has reported nothing is ambiguous in a way a normal
 * tool result is not: it can mean "nothing happened", "nothing is watching"
 * or "the thing you are watching cannot be seen from here". Each needs a
 * different next move, so say which one this is.
 */
export function pollNote(result: {
  events: unknown[];
  dropped: boolean;
  hooks: HookStatus[];
  cursor_scope?: string | null;
  skipped_other_hooks?: number;
}): string {
  if (result.skipped_other_hooks) {
    // Said first because it is the one that bites silently: sequence
    // numbers are global, so a cursor from a filtered drain is only valid
    // for that same filter.
    return (
      `next_seq is the cursor for hook ${result.cursor_scope} ONLY — ${result.skipped_other_hooks} ` +
      "event(s) from other hooks sit inside the range it covers and are still waiting. " +
      "Keep a separate cursor per filter, or drain without id= to follow everything at once."
    );
  }
  if (result.dropped) {
    return (
      "Older events aged out of the journal before you polled — poll more often, " +
      "or pass wait_ms to block until something lands."
    );
  }
  if (result.events.length) {
    return "Pass next_seq back as since= on the next poll.";
  }
  if (!result.hooks.length) {
    return "No hooks exist yet. Create one with gdr_hook_screen or gdr_hook_window.";
  }
  const enabled = result.hooks.filter((h) => h.enabled);
  if (!enabled.length) {
    return "Every hook is switched off. Turn one on with gdr_hooks({action:'enable', id}).";
  }
  const stuck = enabled.filter((h) => h.state !== "watching");
  if (stuck.length === enabled.length) {
    return (
      "Nothing is watching yet: " +
      stuck
        .map((h) => `${h.id} is ${h.state}${h.last_error ? ` (${h.last_error})` : ""}`)
        .join("; ")
    );
  }
  return "Nothing happened yet. Pass wait_ms to block until something does.";
}
