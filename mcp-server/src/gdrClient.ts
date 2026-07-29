// Re-implementation of the gdr wire protocol described in
// common/src/lib.rs (Rust). Deliberately JSON, not a Rust binary format, so
// this file doesn't need to depend on the Rust crate at all - the two are
// kept in sync by hand; if you change common/src/lib.rs, mirror the shape here.
//
// Framing: [u32 length, big-endian][UTF-8 JSON payload]

import * as tls from "node:tls";
import * as net from "node:net";
import { createHash } from "node:crypto";

export type Request =
  | { type: "Auth"; token: string }
  | { type: "Screenshot"; connector: string | null }
  | { type: "MouseMove"; x: number; y: number }
  | { type: "MouseButton"; button: number; pressed: boolean }
  | { type: "MouseScroll"; dx: number; dy: number }
  | { type: "KeyEvent"; keycode: number; pressed: boolean }
  | { type: "TypeText"; text: string }
  | { type: "Ping" };

export type Response =
  | { type: "Ok" }
  | { type: "AuthOk" }
  | { type: "AuthOkScoped"; scopes: string[] }
  | { type: "AuthFailed" }
  | { type: "Screenshot"; png_base64: string }
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
 * protocol is strictly request/response per connection. */
export class GdrClient {
  private socket: tls.TLSSocket | null = null;
  private queue: Promise<unknown> = Promise.resolve();

  constructor(private cfg: GdrConfig) {}

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
      };
      const onError = (e: Error) => {
        cleanup();
        this.socket = null;
        reject(e);
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
      socket.write(Buffer.concat([lenBuf, payload]));
    });
  }

  /** Send one request, waiting for its response. Serialized across callers. */
  request(req: Request): Promise<Response> {
    const run = async () => {
      const socket = await this.ensureConnected();
      try {
        return await this.sendRaw(socket, req);
      } catch (e) {
        this.socket = null;
        throw e;
      }
    };
    const result = this.queue.then(run, run);
    this.queue = result.catch(() => undefined);
    return result as Promise<Response>;
  }

  async click(x: number, y: number, button: number = BTN_LEFT): Promise<Response> {
    await this.request({ type: "MouseMove", x, y });
    await this.request({ type: "MouseButton", button, pressed: true });
    return this.request({ type: "MouseButton", button, pressed: false });
  }

  close() {
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
