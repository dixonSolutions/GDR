/**
 * MCP process argv / env for default device selection.
 *
 * Examples that actually bind a default device for every tool call:
 *   gdr-mcp --dev "home computer"
 *   gdr-mcp --dev=desktop
 *   gdr-mcp -d local
 *   GDR_DEV="home computer" gdr-mcp
 *
 * Cursor mcp.json:
 *   { "command": "gdr-mcp", "args": ["--dev", "home computer"] }
 *
 * Chat shorthand for agents: `@gdr -dev="home computer"` → pass tool arg
 * `dev: "home computer"` (or rely on server --dev default).
 */

let defaultDevice: string | undefined;

export function getDefaultDevice(): string | undefined {
  return defaultDevice;
}

export function setDefaultDevice(dev: string | undefined): void {
  defaultDevice = dev?.trim() || undefined;
}

export function parseServerArgv(argv: string[] = process.argv.slice(2)): void {
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--help" || a === "-h") {
      printHelp();
      process.exit(0);
    }
    if (a === "--dev" || a === "--default-host" || a === "--host" || a === "-d") {
      const v = argv[i + 1];
      if (!v || v.startsWith("-")) {
        console.error(`gdr-mcp: ${a} requires a device id/label`);
        process.exit(2);
      }
      setDefaultDevice(v);
      i++;
      continue;
    }
    if (
      a.startsWith("--dev=") ||
      a.startsWith("-dev=") ||
      a.startsWith("--default-host=") ||
      a.startsWith("--host=") ||
      a.startsWith("-d=")
    ) {
      setDefaultDevice(a.slice(a.indexOf("=") + 1));
      continue;
    }
    // Ignore unknown flags so MCP hosts can pass through extras safely.
  }

  if (!defaultDevice) {
    setDefaultDevice(process.env.GDR_DEV || process.env.GDR_DEFAULT_HOST || undefined);
  }
}

function printHelp(): void {
  console.error(`gdr-mcp — GNOME desktop remote MCP server

Usage:
  gdr-mcp [--dev <id|label|alias>]

Options:
  --dev, -d, --default-host   Default device for tools when host/dev omitted
  --help                      Show this help

Devices, tokens, sudo passwords: ~/.config/gdr/config.json
  gdr device list
  gdr device add home --address … --token … --label "home computer" --ask-sudo

Cursor (~/.cursor/mcp.json):
  { "command": "gdr-mcp", "args": ["--dev", "home computer"] }

Or per-device servers via: gdr mcp setup-cursor --per-device
`);
}
