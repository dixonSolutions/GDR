// Re-implementation of the gdr wire protocol described in
// common/src/lib.rs (Rust). Deliberately JSON, not a Rust binary format, so
// this file doesn't need to depend on the Rust crate at all - the two are
// kept in sync by hand; if you change common/src/lib.rs, mirror the shape here.
//
// Framing: [u32 length, big-endian][UTF-8 JSON payload]

import * as tls from "node:tls";
import * as net from "node:net";
import { createHash } from "node:crypto";

export type ImageFormat = "png" | "jpeg";

/** Rectangle in native stream pixels. */
export interface Region {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** Wait for the compositor to stop emitting damage before capturing. */
export interface Settle {
  quiet_ms: number;
  timeout_ms: number;
}

/** Reply to `CaptureFrame` — already cropped, sized and encoded by gdrd. */
export interface Frame {
  /** Base64 image bytes. Empty string when `unchanged`. */
  data_base64: string;
  format: ImageFormat;
  native_width: number;
  native_height: number;
  region: Region;
  image_width: number;
  image_height: number;
  hash: string;
  unchanged: boolean;
  /** False when a requested settle hit its timeout. */
  settled: boolean;
  capture_ms: number;
}

/** Rectangle in GNOME logical (stage) pixels — NOT capture-stream pixels. */
export interface LogicalRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface MonitorInfo {
  index: number;
  connector: string | null;
  geometry: LogicalRect;
  scale: number;
  primary: boolean;
  /** True for the monitor gdrd is streaming; only these can be captured. */
  captured: boolean;
}

/**
 * One window on the remote desktop.
 *
 * `frame_rect` is logical, `stream_region` is capture-stream pixels and is
 * null when the window is not on the captured monitor. Never scale between
 * them here — gdrd measured the ratio from the live stream.
 */
export interface WindowInfo {
  id: number;
  title: string | null;
  wm_class: string | null;
  app_id: string | null;
  pid: number;
  window_type: string;
  frame_rect: LogicalRect;
  stream_region: Region | null;
  monitor: number;
  connector: string | null;
  workspace: number | null;
  on_active_workspace: boolean;
  minimized: boolean;
  maximized: string;
  fullscreen: boolean;
  focus: boolean;
  above: boolean;
  on_all_workspaces: boolean;
  skip_taskbar: boolean;
  can_close: boolean;
}

export interface AppInfo {
  app_id: string;
  name: string;
  windows: number;
  running: boolean;
}

/** Selector naming one window. Fields combine with AND. */
export interface WindowTarget {
  id?: number | null;
  app_id?: string | null;
  wm_class?: string | null;
  title?: string | null;
  pid?: number | null;
  focused?: boolean;
}

export type WindowOp =
  | { action: "activate" }
  | { action: "focus" }
  | { action: "raise" }
  | { action: "minimize" }
  | { action: "unminimize" }
  | { action: "maximize" }
  | { action: "unmaximize" }
  | { action: "fullscreen" }
  | { action: "unfullscreen" }
  | { action: "above" }
  | { action: "unabove" }
  | { action: "stick" }
  | { action: "unstick" }
  | { action: "close" }
  | { action: "move"; x: number; y: number }
  | { action: "resize"; width: number; height: number }
  | { action: "move_resize"; x: number; y: number; width: number; height: number }
  | { action: "workspace"; index: number };

export interface WindowEvent {
  seq: number;
  kind: string;
  at: string;
  id: number;
  title: string | null;
  wm_class: string | null;
  app_id: string | null;
}

export interface WindowList {
  backend: string;
  windows: WindowInfo[];
  monitors: MonitorInfo[];
  focus_window: number | null;
  active_workspace: number;
  n_workspaces: number;
  capture_connector: string | null;
  seq: number;
}

export type Request =
  | { type: "Auth"; token: string }
  | { type: "Screenshot"; connector: string | null }
  | {
      type: "CaptureFrame";
      region?: Region | null;
      max_width?: number | null;
      max_height?: number | null;
      format?: ImageFormat;
      quality?: number | null;
      settle?: Settle | null;
      if_none_match?: string | null;
    }
  | { type: "MouseMove"; x: number; y: number }
  | { type: "MouseButton"; button: number; pressed: boolean }
  | { type: "MouseScroll"; dx: number; dy: number }
  | { type: "KeyEvent"; keycode: number; pressed: boolean }
  | { type: "TypeText"; text: string }
  | { type: "GetCursor" }
  | { type: "ListWindows"; include_skip_taskbar?: boolean }
  | ({ type: "WindowAction"; target: WindowTarget } & WindowOp)
  | { type: "LaunchApp"; app_id: string }
  | { type: "ListApps"; filter?: string | null }
  | { type: "WindowEvents"; since?: number; limit?: number; wait_ms?: number }
  | { type: "Ping" };

export type Response =
  | { type: "Ok" }
  | { type: "AuthOk" }
  | { type: "AuthOkScoped"; scopes: string[] }
  | { type: "AuthFailed" }
  | { type: "Screenshot"; png_base64: string }
  | ({ type: "Frame" } & Frame)
  | { type: "CursorPosition"; x: number; y: number; known: boolean }
  | ({ type: "Windows" } & WindowList)
  | {
      type: "WindowActed";
      action: string;
      window: WindowInfo | null;
      detail: string | null;
    }
  | { type: "AppLaunched"; app_id: string; name: string | null; was_running: boolean }
  | { type: "Apps"; apps: AppInfo[] }
  | {
      type: "WindowEvents";
      events: WindowEvent[];
      next_seq: number;
      dropped: boolean;
      reset: boolean;
    }
  | { type: "Pong" }
  | { type: "Error"; message: string };

export const BTN_LEFT = 0x110;
export const BTN_RIGHT = 0x111;
export const BTN_MIDDLE = 0x112;

export interface GdrConfig {
  host: string;
  port: number;
  token: string;
  /** Expected SHA-256 fingerprint (hex, no colons) of the server's TLS cert. */
  pinnedFingerprint?: string;
}

/** A persistent, auto-reconnecting connection to a gdrd.
 * Calls are serialized (one in-flight request at a time) since the wire
 * protocol is strictly request/response per connection.
 *
 * Idle disconnect: after `GDR_MCP_IDLE_MS` (default 15000) with no
 * requests, the TLS socket is closed so gdrd can idle-stop ScreenCast.
 * Cursor keeps the MCP *process* up; only the data-plane link drops. */
export class GdrClient {
  private socket: tls.TLSSocket | null = null;
  private queue: Promise<unknown> = Promise.resolve();
  private idleTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly idleMs: number;

  constructor(private cfg: GdrConfig) {
    const raw = process.env.GDR_MCP_IDLE_MS;
    const parsed = raw !== undefined ? Number(raw) : 15_000;
    this.idleMs = Number.isFinite(parsed) && parsed >= 0 ? parsed : 15_000;
  }

  private bumpIdle(): void {
    if (this.idleTimer) clearTimeout(this.idleTimer);
    if (this.idleMs === 0) return;
    this.idleTimer = setTimeout(() => {
      this.idleTimer = null;
      this.close();
    }, this.idleMs);
    // Don't keep Node alive solely for the idle timer.
    this.idleTimer.unref?.();
  }

  private async ensureConnected(): Promise<tls.TLSSocket> {
    if (this.socket && !this.socket.destroyed) return this.socket;

    const socket = await new Promise<tls.TLSSocket>((resolve, reject) => {
      const s = tls.connect(
        {
          host: this.cfg.host,
          port: this.cfg.port,
          rejectUnauthorized: false, // we do our own pinning below (no CA)
          servername: "gdrd",
        },
        () => resolve(s)
      );
      s.once("error", reject);
    });

    const cert = socket.getPeerCertificate();
    const actual = createHash("sha256").update(cert.raw).digest("hex");
    if (this.cfg.pinnedFingerprint) {
      if (this.cfg.pinnedFingerprint.toLowerCase() !== actual.toLowerCase()) {
        socket.destroy();
        throw new Error(
          `server cert fingerprint mismatch: expected ${this.cfg.pinnedFingerprint}, got ${actual}`
        );
      }
    } else {
      console.error(
        `[gdr-mcp] WARNING: no pinned fingerprint configured, trusting on first use.\n` +
          `Server cert SHA-256: ${actual}\nSet GDR_PIN=${actual} to pin it.`
      );
    }

    this.socket = socket;
    const authResp = await this.sendRaw(socket, { type: "Auth", token: this.cfg.token });
    if (authResp.type !== "AuthOk" && authResp.type !== "AuthOkScoped") {
      socket.destroy();
      this.socket = null;
      throw new Error(`auth failed: ${JSON.stringify(authResp)}`);
    }
    return socket;
  }

  private sendRaw(socket: net.Socket, req: Request): Promise<Response> {
    return new Promise((resolve, reject) => {
      const payload = Buffer.from(JSON.stringify(req), "utf8");
      const lenBuf = Buffer.alloc(4);
      lenBuf.writeUInt32BE(payload.length, 0);

      let acc: Buffer = Buffer.alloc(0);
      let expectedLen: number | null = null;

      const cleanup = () => {
        socket.removeListener("data", onData);
        socket.removeListener("error", onError);
        socket.removeListener("close", onClose);
        socket.removeListener("end", onClose);
      };
      const onError = (e: Error) => {
        cleanup();
        this.socket = null;
        reject(e);
      };
      // A daemon that cannot parse a request drops the connection without
      // replying, and a socket close raises no "error" — so without this the
      // promise never settles and the caller hangs forever. The commonest
      // cause is version skew: an older gdrd that has never heard of the
      // request type, which is worth naming since the fix is an upgrade.
      const onClose = () => {
        cleanup();
        this.socket = null;
        reject(
          new Error(
            `gdrd closed the connection without answering ${req.type}. ` +
              "Most likely the daemon is older than this client and does not " +
              "know that request — update gdrd on the target."
          )
        );
      };
      const onData = (chunk: Buffer) => {
        try {
          acc = acc.length ? Buffer.concat([acc, chunk]) : chunk;

          if (expectedLen === null) {
            if (acc.length < 4) return;
            expectedLen = acc.readUInt32BE(0);
            acc = acc.subarray(4);
          }
          if (acc.length < expectedLen) return;

          const body = acc.subarray(0, expectedLen);
          cleanup();
          resolve(JSON.parse(body.toString("utf8")) as Response);
        } catch (e) {
          cleanup();
          reject(e as Error);
        }
      };
      socket.on("data", onData);
      socket.once("error", onError);
      socket.once("close", onClose);
      socket.once("end", onClose);
      socket.write(Buffer.concat([lenBuf, payload]));
    });
  }

  /** Send one request, waiting for its response. Serialized across callers. */
  request(req: Request): Promise<Response> {
    const run = async () => {
      // Stop the idle timer for the duration of the call. A long-polling
      // WindowEvents can legitimately sit for 30s, which is longer than the
      // 15s idle default — without this the timer armed by the *previous*
      // call fires mid-flight, destroys the socket, and the poll fails with
      // a socket error that looks like the daemon died.
      if (this.idleTimer) {
        clearTimeout(this.idleTimer);
        this.idleTimer = null;
      }
      const socket = await this.ensureConnected();
      try {
        const resp = await this.sendRaw(socket, req);
        this.bumpIdle();
        return resp;
      } catch (e) {
        this.socket = null;
        throw e;
      }
    };
    const result = this.queue.then(run, run);
    this.queue = result.catch(() => undefined);
    return result as Promise<Response>;
  }

  /**
   * Capture with server-side crop/resize/encode.
   * gdrd does the sizing in one pass from the raw PipeWire buffer, so there
   * is nothing to decode and re-encode here.
   */
  async captureFrame(req: Omit<Request & { type: "CaptureFrame" }, "type">): Promise<Frame> {
    const resp = await this.request({ type: "CaptureFrame", ...req });
    if (resp.type === "Error") throw new Error(resp.message);
    if (resp.type !== "Frame") {
      throw new Error(`expected Frame, got ${resp.type}`);
    }
    return resp;
  }

  /** Every managed window, with logical and capture-stream geometry. */
  async listWindows(includeSkipTaskbar = false): Promise<WindowList> {
    const resp = await this.request({
      type: "ListWindows",
      include_skip_taskbar: includeSkipTaskbar,
    });
    if (resp.type === "Error") throw new Error(resp.message);
    if (resp.type !== "Windows") throw new Error(`expected Windows, got ${resp.type}`);
    return resp;
  }

  async windowAction(
    target: WindowTarget,
    op: WindowOp
  ): Promise<{ action: string; window: WindowInfo | null; detail: string | null }> {
    const resp = await this.request({ type: "WindowAction", target, ...op });
    if (resp.type === "Error") throw new Error(resp.message);
    if (resp.type !== "WindowActed") {
      throw new Error(`expected WindowActed, got ${resp.type}`);
    }
    return resp;
  }

  async launchApp(appId: string) {
    const resp = await this.request({ type: "LaunchApp", app_id: appId });
    if (resp.type === "Error") throw new Error(resp.message);
    if (resp.type !== "AppLaunched") {
      throw new Error(`expected AppLaunched, got ${resp.type}`);
    }
    return resp;
  }

  async listApps(filter?: string | null): Promise<AppInfo[]> {
    const resp = await this.request({ type: "ListApps", filter: filter ?? null });
    if (resp.type === "Error") throw new Error(resp.message);
    if (resp.type !== "Apps") throw new Error(`expected Apps, got ${resp.type}`);
    return resp.apps;
  }

  /**
   * Poll the window journal. `waitMs > 0` holds the request open until
   * something happens, so a watcher does not have to spin.
   *
   * The socket's own idle timer is not the concern here (the daemon answers),
   * but a long wait does occupy this client's single in-flight slot — every
   * other call on the same device queues behind it.
   */
  async windowEvents(since = 0, limit = 100, waitMs = 0) {
    const resp = await this.request({
      type: "WindowEvents",
      since,
      limit,
      wait_ms: waitMs,
    });
    if (resp.type === "Error") throw new Error(resp.message);
    if (resp.type !== "WindowEvents") {
      throw new Error(`expected WindowEvents, got ${resp.type}`);
    }
    return resp;
  }

  async click(x: number, y: number, button: number = BTN_LEFT): Promise<Response> {
    await this.request({ type: "MouseMove", x, y });
    await this.request({ type: "MouseButton", button, pressed: true });
    return this.request({ type: "MouseButton", button, pressed: false });
  }

  /** Double-click (or N-click) at absolute coordinates. */
  async multiClick(
    x: number,
    y: number,
    button: number = BTN_LEFT,
    clicks = 2,
    gapMs = 60
  ): Promise<Response> {
    let last: Response = { type: "Ok" };
    await this.request({ type: "MouseMove", x, y });
    const n = Math.max(1, Math.min(10, Math.floor(clicks)));
    for (let i = 0; i < n; i++) {
      await this.request({ type: "MouseButton", button, pressed: true });
      last = await this.request({ type: "MouseButton", button, pressed: false });
      if (i + 1 < n) {
        await new Promise((r) => setTimeout(r, gapMs));
      }
    }
    return last;
  }

  close() {
    if (this.idleTimer) {
      clearTimeout(this.idleTimer);
      this.idleTimer = null;
    }
    this.socket?.destroy();
    this.socket = null;
  }
}

/** Pool of clients keyed by host profile name (or "__env__"). */
export class GdrClientPool {
  private clients = new Map<string, GdrClient>();

  get(key: string, cfg: GdrConfig): GdrClient {
    let c = this.clients.get(key);
    if (!c) {
      c = new GdrClient(cfg);
      this.clients.set(key, c);
    }
    return c;
  }
}
