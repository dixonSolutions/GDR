// Shared host/device store at ~/.config/gdr/config.json — same JSON shape
// as client/src/config.rs (snake_case). Resolution order mirrors the Rust CLI:
// tool host/dev → server --dev → env → default_host → sole profile.

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { getDefaultDevice } from "./cliArgs.js";

export interface HostProfile {
  address: string;
  port: number;
  token: string;
  pin?: string | null;
  ssh?: string | null;
  sudo_password?: string | null;
  user_password?: string | null;
  label?: string | null;
  aliases?: string[];
  /**
   * Default window for this device's window tools — see windowPin.ts.
   * Typed loosely here so config.ts stays free of window imports; the
   * shape is `PinnedWindow`.
   */
  pinned_window?: Record<string, unknown>;
}

export interface GdrConfigFile {
  default_host?: string | null;
  hosts: Record<string, HostProfile>;
}

export interface ResolvedHost {
  name?: string;
  address: string;
  port: number;
  token: string;
  pin?: string;
  sudo_password?: string | null;
  user_password?: string | null;
  /** True when the profile targets this machine (no LAN/Tailscale IP). */
  same_machine?: boolean;
  label?: string | null;
}

/** Addresses that mean "this machine" — stored without a real IP. */
export function isLoopbackAlias(address: string): boolean {
  switch (address.trim().toLowerCase()) {
    case "":
    case "local":
    case "localhost":
    case "loopback":
    case "this":
    case ".":
    case "127.0.0.1":
    case "::1":
      return true;
    default:
      return false;
  }
}

/** Map same-machine aliases to a connectable loopback host. */
export function normalizeAddress(address: string): string {
  return isLoopbackAlias(address) ? "127.0.0.1" : address.trim();
}

export function configPath(): string {
  return path.join(os.homedir(), ".config", "gdr", "config.json");
}

export function loadConfig(): GdrConfigFile {
  const p = configPath();
  if (!fs.existsSync(p)) {
    return { hosts: {} };
  }
  const raw = fs.readFileSync(p, "utf8");
  const parsed = JSON.parse(raw) as GdrConfigFile;
  parsed.hosts = parsed.hosts ?? {};
  return parsed;
}

/** Persist config.json (mode 0o600). Never log secrets. */
export function saveConfig(cfg: GdrConfigFile): void {
  const p = configPath();
  fs.mkdirSync(path.dirname(p), { recursive: true, mode: 0o700 });
  const tmp = `${p}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(cfg, null, 2) + "\n", { mode: 0o600 });
  fs.renameSync(tmp, p);
  try {
    fs.chmodSync(p, 0o600);
  } catch {
    /* ignore */
  }
}

export type DeviceUpsertInput = {
  id: string;
  address?: string;
  local?: boolean;
  port?: number;
  token?: string;
  pin?: string | null;
  ssh?: string | null;
  label?: string | null;
  aliases?: string[];
  sudo_password?: string | null;
  user_password?: string | null;
  default?: boolean;
};

/** Add or update a device profile. Merges unset fields from the previous entry. */
export function upsertDevice(input: DeviceUpsertInput): {
  id: string;
  created: boolean;
  path: string;
} {
  const cfg = loadConfig();
  const id = input.id.trim();
  if (!id) throw new Error("device id is required");
  const prev = cfg.hosts[id];
  const address = input.local
    ? "localhost"
    : (input.address?.trim() || prev?.address);
  if (!address) {
    throw new Error("pass address= or local=true (or update an existing id)");
  }
  const token = input.token?.trim() || prev?.token;
  if (!token) throw new Error("pass token= (required for new devices)");

  const profile: HostProfile = {
    address,
    port: input.port ?? prev?.port ?? 7337,
    token,
    pin: input.pin !== undefined ? input.pin : (prev?.pin ?? null),
    ssh: input.ssh !== undefined ? input.ssh : (prev?.ssh ?? null),
    sudo_password:
      input.sudo_password !== undefined
        ? input.sudo_password
        : (prev?.sudo_password ?? null),
    user_password:
      input.user_password !== undefined
        ? input.user_password
        : (prev?.user_password ?? null),
    label: input.label !== undefined ? input.label : (prev?.label ?? null),
    aliases:
      input.aliases !== undefined ? input.aliases : (prev?.aliases ?? []),
    // Carried forward explicitly: this function rebuilds the profile from
    // scratch, so anything not named here is silently dropped, and a
    // `gdr_device_add` that quietly unpinned your window would be a nasty
    // surprise to debug.
    ...(prev?.pinned_window ? { pinned_window: prev.pinned_window } : {}),
  };
  cfg.hosts[id] = profile;
  if (input.default) cfg.default_host = id;
  saveConfig(cfg);
  return { id, created: !prev, path: configPath() };
}

export function removeDevice(query: string): { id: string; path: string } {
  const cfg = loadConfig();
  const id = findDeviceName(cfg, query);
  if (!id) {
    throw new Error(
      `Unknown device '${query}'. Known: ${knownDevicesSummary(cfg)}`
    );
  }
  delete cfg.hosts[id];
  if (cfg.default_host === id) {
    const rest = Object.keys(cfg.hosts);
    cfg.default_host = rest.length === 1 ? rest[0] : null;
  }
  saveConfig(cfg);
  return { id, path: configPath() };
}

export function setDefaultDevice(query: string): { id: string; path: string } {
  const cfg = loadConfig();
  const id = findDeviceName(cfg, query);
  if (!id) {
    throw new Error(
      `Unknown device '${query}'. Known: ${knownDevicesSummary(cfg)}`
    );
  }
  cfg.default_host = id;
  saveConfig(cfg);
  return { id, path: configPath() };
}

/** Public (no-secret) view of one resolved device. */
export function devicePublicInfo(query?: string | null): Record<string, unknown> {
  const resolved = resolveHost(query);
  const cfg = loadConfig();
  const id = resolved.name;
  const p = id ? cfg.hosts[id] : undefined;
  return {
    id: id ?? null,
    label: p?.label ?? resolved.label ?? null,
    aliases: p?.aliases ?? [],
    address: `${resolved.address}:${resolved.port}`,
    same_machine: Boolean(resolved.same_machine),
    default: id != null && cfg.default_host === id,
    has_token: Boolean(resolved.token),
    has_pin: Boolean(resolved.pin),
    has_sudo: Boolean(resolved.sudo_password),
    has_user: Boolean(resolved.user_password),
    pinned_window: p?.pinned_window ?? null,
    ssh: p?.ssh ?? null,
    chat: id
      ? `@gdr -dev="${p?.label || id}"`
      : '@gdr -dev="<label or id>"',
  };
}

/** Match device id, label, or alias (case-insensitive). Returns canonical key. */
export function findDeviceName(cfg: GdrConfigFile, query: string): string | undefined {
  const q = query.trim();
  if (!q) return undefined;
  if (cfg.hosts[q]) return q;
  const ql = q.toLowerCase();
  for (const [name, p] of Object.entries(cfg.hosts)) {
    if (name.toLowerCase() === ql) return name;
    if (p.label && p.label.trim().toLowerCase() === ql) return name;
    if ((p.aliases ?? []).some((a) => a.trim().toLowerCase() === ql)) return name;
  }
  return undefined;
}

export function knownDevicesSummary(cfg: GdrConfigFile): string {
  const parts: string[] = [];
  for (const [name, p] of Object.entries(cfg.hosts)) {
    const bits = [name];
    if (p.label) bits.push(`label="${p.label}"`);
    if (p.aliases?.length) bits.push(`aliases=${p.aliases.join("|")}`);
    parts.push(bits.join(" "));
  }
  return parts.join(", ") || "(none)";
}

function fromProfile(name: string, p: HostProfile): ResolvedHost {
  return {
    name,
    address: normalizeAddress(p.address),
    port: p.port ?? 7337,
    token: p.token,
    pin: p.pin ?? undefined,
    sudo_password: p.sudo_password,
    user_password: p.user_password,
    same_machine: isLoopbackAlias(p.address),
    label: p.label,
  };
}

/**
 * Resolve a device for an MCP tool call.
 * Query may be the profile id, label ("home computer"), or alias.
 * When omitted: server --dev / GDR_DEV → env GDR_HOST+TOKEN → default_host → sole profile.
 */
export function resolveHost(hostOrDev?: string | null): ResolvedHost {
  const cfg = loadConfig();
  const envHost = process.env.GDR_HOST;
  const envPort = process.env.GDR_PORT;
  const envToken = process.env.GDR_TOKEN;
  const envPin = process.env.GDR_PIN;

  const query = (hostOrDev && hostOrDev.trim()) || getDefaultDevice() || undefined;

  if (query) {
    const name = findDeviceName(cfg, query);
    if (!name) {
      throw new Error(
        `Unknown device '${query}'. Known: ${knownDevicesSummary(cfg)}. ` +
          `Pass host=/dev= (id, label, or alias), or set --dev on gdr-mcp.`
      );
    }
    return fromProfile(name, cfg.hosts[name]);
  }

  // Env override (legacy / single-host Claude Desktop env block)
  if (envHost && envToken) {
    const address = normalizeAddress(envHost);
    return {
      address,
      port: Number(envPort ?? "7337"),
      token: envToken,
      pin: envPin,
      sudo_password: null,
      user_password: null,
      same_machine: isLoopbackAlias(envHost),
    };
  }

  const name =
    (cfg.default_host && findDeviceName(cfg, cfg.default_host)) ||
    (Object.keys(cfg.hosts).length === 1 ? Object.keys(cfg.hosts)[0] : undefined);

  if (!name) {
    throw new Error(
      "No device configured. Add one with `gdr device add`, set GDR_DEV / --dev, " +
        "or pass host=/dev= on the tool."
    );
  }
  return fromProfile(name, cfg.hosts[name]);
}

/** Prefer explicit `dev` or `host` tool args (same meaning). */
export function resolveToolDevice(args: {
  host?: string | null;
  dev?: string | null;
}): ResolvedHost {
  return resolveHost(args.dev || args.host || null);
}

export function getPasswordMessage(
  kind: "sudo" | "user",
  hostArg?: string | null
): { ok: boolean; message: string } {
  const cfg = loadConfig();
  let name =
    (hostArg && findDeviceName(cfg, hostArg)) ||
    getDefaultDevice() && findDeviceName(cfg, getDefaultDevice()!) ||
    (cfg.default_host && findDeviceName(cfg, cfg.default_host)) ||
    undefined;
  if (!name) {
    const keys = Object.keys(cfg.hosts);
    if (keys.length === 1) name = keys[0];
  }
  if (!name) {
    return { ok: false, message: "No device specified and no default_host is set." };
  }
  const p = cfg.hosts[name];
  if (!p) {
    return { ok: false, message: `Unknown device '${name}'.` };
  }
  if (kind === "sudo") {
    if (p.sudo_password) return { ok: true, message: p.sudo_password };
    return { ok: false, message: `No sudo password is set for device '${name}'.` };
  }
  if (p.user_password) return { ok: true, message: p.user_password };
  return { ok: false, message: `No user password is set for device '${name}'.` };
}

export function listDevicesPayload(): unknown {
  const cfg = loadConfig();
  const hosts = Object.keys(cfg.hosts).map((n) => {
    const p = cfg.hosts[n];
    const same_machine = isLoopbackAlias(p.address);
    return {
      id: n,
      label: p.label ?? null,
      aliases: p.aliases ?? [],
      address: same_machine
        ? `${p.address} → 127.0.0.1:${p.port ?? 7337}`
        : `${p.address}:${p.port ?? 7337}`,
      same_machine,
      default: cfg.default_host === n,
      has_token: Boolean(p.token),
      has_sudo: Boolean(p.sudo_password),
      has_user: Boolean(p.user_password),
      has_pin: Boolean(p.pin),
      pinned_window: p.pinned_window ?? null,
      ssh: p.ssh ?? null,
    };
  });
  return {
    default_host: cfg.default_host ?? null,
    server_default_dev: getDefaultDevice() ?? null,
    choose_with:
      'pass host= or dev= (id, label, or alias). Chat: @gdr -dev="home computer" → tool arg dev="home computer".',
    cursor_mcp_args: 'gdr-mcp --dev "home computer"',
    hosts,
    devices: hosts,
  };
}
