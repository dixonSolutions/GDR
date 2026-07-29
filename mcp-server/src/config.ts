// Shared host-profile store at ~/.config/gdr/config.json — same JSON shape
// as client/src/config.rs (snake_case). Resolution order mirrors the Rust CLI:
// env vars → named host arg → default_host → sole profile.

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

export interface HostProfile {
  address: string;
  port: number;
  token: string;
  pin?: string | null;
  ssh?: string | null;
  sudo_password?: string | null;
  user_password?: string | null;
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

/**
 * Resolve a host for an MCP tool call.
 * Env vars (GDR_HOST/PORT/TOKEN/PIN) act as an implicit single-host override
 * when no named profile is requested — keeps existing Claude Desktop configs
 * working without a config.json.
 */
export function resolveHost(hostArg?: string | null): ResolvedHost {
  const cfg = loadConfig();
  const envHost = process.env.GDR_HOST;
  const envPort = process.env.GDR_PORT;
  const envToken = process.env.GDR_TOKEN;
  const envPin = process.env.GDR_PIN;

  if (hostArg) {
    const p = cfg.hosts[hostArg];
    if (!p) {
      throw new Error(
        `Unknown host profile '${hostArg}'. Known: ${Object.keys(cfg.hosts).join(", ") || "(none)"}`
      );
    }
    return {
      name: hostArg,
      address: p.address,
      port: p.port ?? 7337,
      token: p.token,
      pin: p.pin ?? undefined,
      sudo_password: p.sudo_password,
      user_password: p.user_password,
    };
  }

  // Env override (legacy / Claude Desktop env block)
  if (envHost && envToken) {
    return {
      address: envHost,
      port: Number(envPort ?? "7337"),
      token: envToken,
      pin: envPin,
      sudo_password: null,
      user_password: null,
    };
  }

  const name =
    cfg.default_host ??
    (Object.keys(cfg.hosts).length === 1 ? Object.keys(cfg.hosts)[0] : undefined);

  if (!name) {
    throw new Error(
      "No host configured. Set GDR_HOST/GDR_TOKEN env, or add a profile to ~/.config/gdr/config.json"
    );
  }
  return resolveHost(name);
}

export function getPasswordMessage(
  kind: "sudo" | "user",
  hostArg?: string | null
): { ok: boolean; message: string } {
  const cfg = loadConfig();
  let name = hostArg ?? cfg.default_host ?? undefined;
  if (!name) {
    const keys = Object.keys(cfg.hosts);
    if (keys.length === 1) name = keys[0];
  }
  if (!name) {
    return { ok: false, message: "No host specified and no default_host is set." };
  }
  const p = cfg.hosts[name];
  if (!p) {
    return { ok: false, message: `Unknown host profile '${name}'.` };
  }
  if (kind === "sudo") {
    if (p.sudo_password) return { ok: true, message: p.sudo_password };
    return { ok: false, message: `No sudo password is set for host '${name}'.` };
  }
  if (p.user_password) return { ok: true, message: p.user_password };
  return { ok: false, message: `No user password is set for host '${name}'.` };
}
