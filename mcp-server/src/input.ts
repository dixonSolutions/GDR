/**
 * Flexible keyboard / mouse input sequences for the MCP layer.
 * Built on KeyEvent press/release so chords like Super+PageDown work.
 */

import {
  BTN_LEFT,
  BTN_MIDDLE,
  BTN_RIGHT,
  GdrClient,
  Response,
} from "./gdrClient.js";
import { KeyRef, parseHotkey, resolveKey, sleep } from "./keys.js";

export type InputStep =
  | { tap: KeyRef }
  | { down: KeyRef }
  | { up: KeyRef }
  | { chord: KeyRef[] } // hold all but last, tap last, release mods
  | { hotkey: string } // "Alt+F4", "Super+PageDown"
  | { type: string }
  | { delay_ms: number }
  | { scroll: { dx?: number; dy?: number; at?: { x: number; y: number } } }
  | { move: { x: number; y: number } }
  | {
      click: {
        x: number;
        y: number;
        button?: "left" | "right" | "middle";
        clicks?: number;
      };
    };

function buttonCode(button?: "left" | "right" | "middle"): number {
  if (button === "right") return BTN_RIGHT;
  if (button === "middle") return BTN_MIDDLE;
  return BTN_LEFT;
}

export async function keyDown(client: GdrClient, ref: KeyRef): Promise<Response> {
  return client.request({ type: "KeyEvent", keycode: resolveKey(ref), pressed: true });
}

export async function keyUp(client: GdrClient, ref: KeyRef): Promise<Response> {
  return client.request({ type: "KeyEvent", keycode: resolveKey(ref), pressed: false });
}

export async function tapKey(client: GdrClient, ref: KeyRef): Promise<Response> {
  await keyDown(client, ref);
  return keyUp(client, ref);
}

/** Hold mods, tap key, release mods (reverse order). */
export async function chord(
  client: GdrClient,
  mods: KeyRef[],
  key: KeyRef
): Promise<Response> {
  for (const m of mods) await keyDown(client, m);
  await keyDown(client, key);
  await keyUp(client, key);
  for (const m of [...mods].reverse()) await keyUp(client, m);
  return { type: "Ok" };
}

export async function hotkey(client: GdrClient, spec: string): Promise<Response> {
  const { mods, key } = parseHotkey(spec);
  return chord(client, mods, key);
}

export async function clickN(
  client: GdrClient,
  x: number,
  y: number,
  button: number,
  clicks: number,
  gapMs = 60
): Promise<Response> {
  let last: Response = { type: "Ok" };
  await client.request({ type: "MouseMove", x, y });
  const n = Math.max(1, Math.min(10, Math.floor(clicks)));
  for (let i = 0; i < n; i++) {
    await client.request({ type: "MouseButton", button, pressed: true });
    last = await client.request({ type: "MouseButton", button, pressed: false });
    if (i + 1 < n) await sleep(gapMs);
  }
  return last;
}

/**
 * Wheel scroll, optionally after moving the pointer somewhere first.
 *
 * `at` matters more than it looks: a compositor delivers wheel events to
 * whatever sits under the pointer, so scrolling a page the agent has not
 * pointed at will silently scroll the wrong pane.
 */
export async function scroll(
  client: GdrClient,
  { dx = 0, dy = 0, at }: { dx?: number; dy?: number; at?: { x: number; y: number } }
): Promise<Response> {
  if (at) await client.request({ type: "MouseMove", x: at.x, y: at.y });
  return client.request({ type: "MouseScroll", dx, dy });
}

export interface SequenceOutcome {
  ok: boolean;
  steps: number;
  /** Steps that ran to completion before stopping. */
  completed: number;
  failed_at?: number;
  error?: string;
}

/**
 * Hook run after each step, e.g. to wait for the UI to settle or to assert
 * that the step actually changed something. Throwing aborts the sequence.
 */
export type AfterStep = (index: number, step: InputStep) => Promise<void>;

/**
 * Run steps in order, stopping at the first failure.
 *
 * Reports how far it got rather than just throwing: when a batch diverges
 * from what the agent expected, "step 3 of 7 failed because X" plus a
 * screenshot of the real state is recoverable, whereas a bare error leaves
 * the agent guessing which half of its plan happened.
 */
export async function runSequenceDetailed(
  client: GdrClient,
  steps: InputStep[],
  afterStep?: AfterStep
): Promise<SequenceOutcome> {
  for (let i = 0; i < steps.length; i++) {
    try {
      await runStep(client, steps[i], i);
      if (afterStep) await afterStep(i, steps[i]);
    } catch (e) {
      return {
        ok: false,
        steps: steps.length,
        completed: i,
        failed_at: i,
        error: e instanceof Error ? e.message : String(e),
      };
    }
  }
  return { ok: true, steps: steps.length, completed: steps.length };
}

/** Run an ordered list of flexible input steps, throwing on the first failure. */
export async function runSequence(
  client: GdrClient,
  steps: InputStep[]
): Promise<{ ok: true; steps: number }> {
  const outcome = await runSequenceDetailed(client, steps);
  if (!outcome.ok) throw new Error(outcome.error);
  return { ok: true, steps: outcome.steps };
}

async function runStep(client: GdrClient, step: InputStep, i: number): Promise<void> {
  {
    try {
      if ("tap" in step) {
        await tapKey(client, step.tap);
      } else if ("down" in step) {
        await keyDown(client, step.down);
      } else if ("up" in step) {
        await keyUp(client, step.up);
      } else if ("chord" in step) {
        if (step.chord.length === 0) throw new Error("empty chord");
        if (step.chord.length === 1) await tapKey(client, step.chord[0]);
        else await chord(client, step.chord.slice(0, -1), step.chord[step.chord.length - 1]);
      } else if ("hotkey" in step) {
        await hotkey(client, step.hotkey);
      } else if ("type" in step) {
        await client.request({ type: "TypeText", text: step.type });
      } else if ("delay_ms" in step) {
        await sleep(Math.max(0, step.delay_ms));
      } else if ("scroll" in step) {
        await scroll(client, step.scroll);
      } else if ("move" in step) {
        await client.request({ type: "MouseMove", x: step.move.x, y: step.move.y });
      } else if ("click" in step) {
        await clickN(
          client,
          step.click.x,
          step.click.y,
          buttonCode(step.click.button),
          step.click.clicks ?? 1
        );
      } else {
        throw new Error(`unknown step at index ${i}: ${JSON.stringify(step)}`);
      }
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new Error(`input step ${i} failed: ${msg}`);
    }
  }
}
