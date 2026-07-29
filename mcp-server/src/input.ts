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

/** Run an ordered list of flexible input steps. */
export async function runSequence(
  client: GdrClient,
  steps: InputStep[]
): Promise<{ ok: true; steps: number }> {
  for (let i = 0; i < steps.length; i++) {
    const step = steps[i];
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
  return { ok: true, steps: steps.length };
}
