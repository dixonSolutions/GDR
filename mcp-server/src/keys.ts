/**
 * Named keys → Linux evdev keycodes for flexible MCP keyboard input.
 * Accepts names ("Super", "PageDown"), aliases ("meta", "pgdn"), or raw numbers.
 */

const NAME_TO_CODE: Record<string, number> = {
  // Modifiers
  esc: 1,
  escape: 1,
  tab: 15,
  enter: 28,
  return: 28,
  space: 57,
  backspace: 14,
  delete: 111,
  del: 111,
  insert: 110,
  home: 102,
  end: 107,
  pageup: 104,
  page_up: 104,
  pgup: 104,
  pagedown: 109,
  page_down: 109,
  pgdn: 109,
  up: 103,
  down: 108,
  left: 105,
  right: 106,

  leftctrl: 29,
  ctrl: 29,
  control: 29,
  rightctrl: 97,
  leftshift: 42,
  shift: 42,
  rightshift: 54,
  leftalt: 56,
  alt: 56,
  rightalt: 100,
  altgr: 100,
  leftmeta: 125,
  meta: 125,
  super: 125,
  win: 125,
  cmd: 125,
  rightmeta: 126,

  capslock: 58,
  compose: 127,
  menu: 139,

  // Function keys
  f1: 59,
  f2: 60,
  f3: 61,
  f4: 62,
  f5: 63,
  f6: 64,
  f7: 65,
  f8: 66,
  f9: 67,
  f10: 68,
  f11: 87,
  f12: 88,

  // Letters (lowercase QWERTY row codes)
  a: 30,
  b: 48,
  c: 46,
  d: 32,
  e: 18,
  f: 33,
  g: 34,
  h: 35,
  i: 23,
  j: 36,
  k: 37,
  l: 38,
  m: 50,
  n: 49,
  o: 24,
  p: 25,
  q: 16,
  r: 19,
  s: 31,
  t: 20,
  u: 22,
  v: 47,
  w: 17,
  x: 45,
  y: 21,
  z: 44,

  // Digits
  "1": 2,
  "2": 3,
  "3": 4,
  "4": 5,
  "5": 6,
  "6": 7,
  "7": 8,
  "8": 9,
  "9": 10,
  "0": 11,

  minus: 12,
  equal: 13,
  equals: 13,
  leftbrace: 26,
  rightbrace: 27,
  semicolon: 39,
  apostrophe: 40,
  grave: 41,
  backslash: 43,
  comma: 51,
  dot: 52,
  period: 52,
  slash: 53,
};

export type KeyRef = string | number;

/** Resolve a key name or numeric evdev code. */
export function resolveKey(ref: KeyRef): number {
  if (typeof ref === "number") {
    if (!Number.isInteger(ref) || ref < 0) {
      throw new Error(`invalid keycode: ${ref}`);
    }
    return ref;
  }
  const raw = ref.trim();
  if (/^\d+$/.test(raw)) return Number(raw);
  const norm = raw.toLowerCase().replace(/[\s-]+/g, "");
  // Keep underscore variants: page_down already in map via page_down key
  const withUnderscore = raw.toLowerCase().replace(/[\s]+/g, "_").replace(/-/g, "_");
  const code = NAME_TO_CODE[norm] ?? NAME_TO_CODE[withUnderscore];
  if (code === undefined) {
    throw new Error(
      `unknown key '${ref}'. Use a name (Super, Alt, F4, PageDown, a…) or an evdev keycode integer.`
    );
  }
  return code;
}

/**
 * Parse a hotkey string like "Super+PageDown", "Alt+F4", "ctrl+alt+t".
 * Last token is the main key; earlier tokens are modifiers held for the tap.
 */
export function parseHotkey(spec: string): { mods: number[]; key: number } {
  const parts = spec
    .split("+")
    .map((p) => p.trim())
    .filter(Boolean);
  if (parts.length === 0) throw new Error("empty hotkey");
  if (parts.length === 1) return { mods: [], key: resolveKey(parts[0]) };
  const mods = parts.slice(0, -1).map(resolveKey);
  const key = resolveKey(parts[parts.length - 1]);
  return { mods, key };
}

export function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}
